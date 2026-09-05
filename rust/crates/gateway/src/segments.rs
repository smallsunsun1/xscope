//! Switch only fully acknowledged journals into immutable local archives.
//! No deletion: delivery ACK is not proof of financial settlement.
use crate::{storage::StorageBudget, wal};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    ops::Deref,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    active: u64,
}

#[derive(Serialize, Deserialize, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
struct Seal {
    version: u32,
    generation: u64,
    bytes: u64,
    sha256: String,
}

pub(crate) struct Record {
    inner: wal::Record,
    generation: u64,
}
impl Deref for Record {
    type Target = wal::Record;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

pub(crate) struct Journal {
    base: PathBuf,
    current: wal::Journal,
    generation: u64,
    budget: Arc<StorageBudget>,
    _writer: File,
    // Retain the original inode lock even after rotation, rejecting old-version
    // writers while this process is running. Downgrade is still unsupported.
    legacy: Option<wal::Journal>,
    failed: bool,
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}
fn segment(base: &Path, generation: u64) -> PathBuf {
    if generation == 0 {
        base.into()
    } else {
        sidecar(base, ".segments").join(format!("{generation:020}.jsonl"))
    }
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    if !regular_file(path)? {
        return Err(invalid("WAL metadata missing"));
    }
    Ok(serde_json::from_reader(File::open(path)?.take(8192))?)
}
fn regular_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(invalid("WAL path is not a regular file")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
fn atomic_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let temporary = sidecar(path, &format!(".tmp-{}", uuid::Uuid::now_v7()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    sync_parent(path)
}
fn seal(path: &Path, generation: u64) -> io::Result<Seal> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(invalid("segment is not a regular file"));
    }
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0; 65536];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        digest.update(&buffer[..size]);
        bytes += size as u64;
    }
    Ok(Seal {
        version: 1,
        generation,
        bytes,
        sha256: format!("{:x}", digest.finalize()),
    })
}

impl Journal {
    pub fn open(base: &Path, budget: Arc<StorageBudget>) -> io::Result<Self> {
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(sidecar(base, ".writer-lock"))?;
        writer.try_lock().map_err(io::Error::other)?;
        let manifest_path = sidecar(base, ".manifest");
        let generation = if regular_file(&manifest_path)? {
            let manifest: Manifest = read_json(&manifest_path)?;
            if manifest.version != 1 || manifest.active == 0 || manifest.active > 1_000_000 {
                return Err(invalid("invalid segment manifest"));
            }
            manifest.active
        } else {
            0
        };
        // Detect missing/rolled-back manifests, never silently replay only the
        // legacy prefix. A pre-commit orphan may ONLY be the next EMPTY file.
        let directory = sidecar(base, ".segments");
        if directory.try_exists()? {
            if !fs::symlink_metadata(&directory)?.file_type().is_dir() {
                return Err(invalid("segment directory is not a directory"));
            }
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| invalid("invalid segment name"))?;
                if let Some(stem) = name.strip_suffix(".jsonl") {
                    let id: u64 = stem
                        .parse()
                        .map_err(|_| invalid("invalid segment number"))?;
                    if id == 0
                        || !entry.file_type()?.is_file()
                        || name != format!("{id:020}.jsonl")
                        || id > generation + 1
                        || (id > generation && entry.metadata()?.len() != 0)
                    {
                        return Err(invalid(
                            "unreferenced nonempty segment; preserve and repair manifest",
                        ));
                    }
                }
            }
        }
        if !regular_file(base)? && generation > 0 {
            return Err(invalid("legacy archive missing"));
        }
        let original = wal::Journal::open(base)?;
        let mut retained = 0u64;
        for id in 0..generation {
            let path = segment(base, id);
            let expected: Seal = read_json(&sidecar(&path, ".seal"))?;
            let actual = seal(&path, id)?;
            if actual != expected {
                return Err(invalid("archived segment checksum mismatch"));
            }
            retained = retained
                .checked_add(actual.bytes)
                .ok_or_else(|| invalid("WAL size overflow"))?;
        }
        if generation > 0 && !regular_file(&segment(base, generation))? {
            return Err(invalid("active segment missing"));
        }
        let (current, legacy) = if generation == 0 {
            (original, None)
        } else {
            (
                wal::Journal::open(&segment(base, generation))?,
                Some(original),
            )
        };
        budget.add_retained(
            retained
                .checked_add(current.len())
                .ok_or_else(|| invalid("WAL size overflow"))?,
        )?;
        Ok(Self {
            base: base.into(),
            current,
            generation,
            budget,
            _writer: writer,
            legacy,
            failed: false,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        let path = segment(&self.base, self.generation);
        let seal_path = sidecar(&path, ".seal");
        let frozen = seal(&path, self.generation)?;
        if seal_path.exists() {
            if read_json::<Seal>(&seal_path)? != frozen {
                return Err(invalid("sealed data was modified"));
            }
        } else {
            atomic_json(&seal_path, &frozen)?;
        }
        let directory = sidecar(&self.base, ".segments");
        fs::create_dir_all(&directory)?;
        sync_parent(&directory)?;
        let generation = self
            .generation
            .checked_add(1)
            .filter(|generation| *generation <= 1_000_000)
            .ok_or_else(|| invalid("segment number overflow"))?;
        let next = segment(&self.base, generation);
        if regular_file(&next)? && fs::metadata(&next)?.len() != 0 {
            return Err(invalid("next segment already contains data"));
        }
        let current = wal::Journal::open(&next)?;
        if !current.drained() || current.len() != 0 {
            return Err(invalid("next segment is not empty"));
        }
        // Commit the pointer only AFTER file and directory durability, and
        // before any append into the new generation. Old checkpoints stay put.
        atomic_json(
            &sidecar(&self.base, ".manifest"),
            &Manifest {
                version: 1,
                active: generation,
            },
        )?;
        let old = std::mem::replace(&mut self.current, current);
        if self.legacy.is_none() {
            self.legacy = Some(old);
        }
        self.generation = generation;
        xscope_telemetry::background_event("wal_segment", "sealed");
        Ok(())
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(invalid(
                "segmented WAL failed; repair storage before restart",
            ));
        }
        let result = (|| {
            if self.current.drained()
                && self.current.len() > 0
                && (self.current.len() >= self.budget.settings.segment_bytes
                    || sidecar(&segment(&self.base, self.generation), ".seal").exists())
            {
                self.rotate()?;
            }
            self.current.append(bytes)?;
            self.budget.add_retained(bytes.len() as u64 + 1)
        })();
        self.failed |= result.is_err();
        result
    }
    pub fn next(&mut self) -> io::Result<Option<Record>> {
        self.current.next().map(|row| {
            row.map(|inner| Record {
                inner,
                generation: self.generation,
            })
        })
    }
    pub fn acknowledge(&mut self, record: &Record) -> io::Result<()> {
        if record.generation != self.generation {
            return Err(invalid("ACK belongs to an archived generation"));
        }
        self.current.acknowledge(&record.inner)
    }
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::storage::{REQUEST_SPACE, StorageSettings};
    #[test]
    fn sealed_rotation_restart_and_corruption_preserve_every_record() {
        let root = std::env::temp_dir().join(format!("xscope-segments-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        let base = root.join("usage.jsonl");
        let budget = || {
            StorageBudget::new(
                &base,
                StorageSettings {
                    segment_bytes: 1,
                    max_bytes: 100 * REQUEST_SPACE,
                    min_free_bytes: 4096,
                },
            )
        };
        let mut journal = Journal::open(&base, budget()).unwrap();
        journal.append(b"one").unwrap();
        let first = journal.next().unwrap().unwrap();
        journal.acknowledge(&first).unwrap();
        journal.append(b"two").unwrap();
        assert!(journal.acknowledge(&first).is_err());
        assert!(Journal::open(&base, budget()).is_err());
        assert!(wal::Journal::open(&base).is_err());
        drop(journal);
        let mut journal = Journal::open(&base, budget()).unwrap();
        let second = journal.next().unwrap().unwrap();
        assert_eq!(second.bytes, b"two\n");
        journal.acknowledge(&second).unwrap();
        journal.append(b"three").unwrap();
        drop(journal);
        assert_eq!(fs::read(&base).unwrap(), b"one\n");
        assert_eq!(fs::read(segment(&base, 1)).unwrap(), b"two\n");
        fs::write(&base, b"bad\n").unwrap();
        assert!(Journal::open(&base, budget()).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rotation_orphan_before_manifest_is_reused_but_nonempty_orphan_fails_closed() {
        let root =
            std::env::temp_dir().join(format!("xscope-segment-crash-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        let base = root.join("usage.jsonl");
        let budget = || {
            StorageBudget::new(
                &base,
                StorageSettings {
                    segment_bytes: 1,
                    max_bytes: 100 * REQUEST_SPACE,
                    min_free_bytes: 4096,
                },
            )
        };
        let mut journal = Journal::open(&base, budget()).unwrap();
        journal.append(b"one").unwrap();
        let first = journal.next().unwrap().unwrap();
        journal.acknowledge(&first).unwrap();
        drop(journal);
        atomic_json(&sidecar(&base, ".seal"), &seal(&base, 0).unwrap()).unwrap();
        fs::create_dir(sidecar(&base, ".segments")).unwrap();
        fs::write(segment(&base, 1), b"").unwrap();
        let mut journal = Journal::open(&base, budget()).unwrap();
        journal.append(b"two").unwrap();
        drop(journal);
        fs::remove_file(sidecar(&base, ".manifest")).unwrap();
        assert!(Journal::open(&base, budget()).is_err());
        assert_eq!(fs::read(segment(&base, 1)).unwrap(), b"two\n");
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unacked_data_stays_active_and_committed_empty_segment_survives_restart() {
        let root =
            std::env::temp_dir().join(format!("xscope-segment-commit-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root).unwrap();
        let base = root.join("usage.jsonl");
        let budget = || {
            StorageBudget::new(
                &base,
                StorageSettings {
                    segment_bytes: 1,
                    max_bytes: 100 * REQUEST_SPACE,
                    min_free_bytes: 4096,
                },
            )
        };
        let mut journal = Journal::open(&base, budget()).unwrap();
        journal.append(b"one").unwrap();
        journal.append(b"two").unwrap();
        assert!(!sidecar(&base, ".manifest").exists());
        for _ in 0..2 {
            let record = journal.next().unwrap().unwrap();
            journal.acknowledge(&record).unwrap();
        }
        journal.rotate().unwrap(); // Crash after pointer commit, before append.
        drop(journal);
        let mut journal = Journal::open(&base, budget()).unwrap();
        assert!(journal.next().unwrap().is_none());
        journal.append(b"three").unwrap();
        assert_eq!(journal.next().unwrap().unwrap().bytes, b"three\n");
        drop(journal);
        fs::remove_file(sidecar(&base, ".seal")).unwrap();
        assert!(Journal::open(&base, budget()).is_err());
        assert_eq!(fs::read(segment(&base, 1)).unwrap(), b"three\n");
        fs::remove_dir_all(root).unwrap();
    }
}
