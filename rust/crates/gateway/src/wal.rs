//! Single-writer JSONL journal with a durable, contiguous acknowledgement cursor.
//! No compaction: retaining the source file also retains forensic/replay evidence.
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MAX_RECORD: u64 = 1 << 20;

#[derive(Default, Deserialize, Serialize)]
struct Checkpoint {
    version: u32,
    start: u64,
    end: u64,
    sha256: String,
}

pub(crate) struct Record {
    pub bytes: Vec<u8>,
    start: u64,
    end: u64,
}

pub(crate) struct Journal {
    file: File,
    checkpoint_path: PathBuf,
    offset: u64,
    end: u64,
    failed: bool,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?
    .sync_all()
}

impl Journal {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock().map_err(|error| {
            io::Error::other(format!("WAL requires an exclusive writer: {error}"))
        })?;
        file.sync_all()?;
        sync_parent(path)?;
        let end = file.metadata()?.len();
        if end > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last != *b"\n" {
                return Err(invalid(
                    "WAL has an incomplete tail; preserve and repair it before restarting (no records were discarded)",
                ));
            }
        }
        let checkpoint_path = sidecar(path, ".checkpoint");
        let checkpoint = match File::open(&checkpoint_path) {
            Ok(file) => serde_json::from_reader::<_, Checkpoint>(file.take(4096))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Checkpoint::default(),
            Err(error) => return Err(error),
        };
        if checkpoint.end != 0 {
            if checkpoint.version != 1
                || checkpoint.start >= checkpoint.end
                || checkpoint.end > end
                || checkpoint.end - checkpoint.start > MAX_RECORD
            {
                return Err(invalid("WAL checkpoint is outside the journal"));
            }
            file.seek(SeekFrom::Start(checkpoint.start))?;
            let mut bytes = Vec::new();
            (&mut file)
                .take(checkpoint.end - checkpoint.start)
                .read_to_end(&mut bytes)?;
            if bytes.last() != Some(&b'\n')
                || format!("{:x}", Sha256::digest(&bytes)) != checkpoint.sha256
            {
                return Err(invalid(
                    "WAL checkpoint does not match the acknowledged record",
                ));
            }
        } else if checkpoint.version != 0 || checkpoint.start != 0 || !checkpoint.sha256.is_empty()
        {
            return Err(invalid("invalid empty WAL checkpoint"));
        }
        Ok(Self {
            file,
            checkpoint_path,
            offset: checkpoint.end,
            end,
            failed: false,
        })
    }

    pub fn append(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(io::Error::other(
                "WAL writer failed; restart after storage repair",
            ));
        }
        if bytes.len() as u64 >= MAX_RECORD || bytes.contains(&b'\n') {
            return Err(invalid(
                "WAL record exceeds the limit or contains a raw newline",
            ));
        }
        let result = (|| {
            self.file.seek(SeekFrom::Start(self.end))?;
            self.file.write_all(bytes)?;
            self.file.write_all(b"\n")?;
            self.file.sync_data()?;
            self.end += bytes.len() as u64 + 1;
            Ok(())
        })();
        self.failed |= result.is_err();
        result
    }

    pub fn next(&mut self) -> io::Result<Option<Record>> {
        if self.offset == self.end {
            return Ok(None);
        }
        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        BufReader::new((&mut self.file).take(MAX_RECORD)).read_until(b'\n', &mut bytes)?;
        if bytes.last() != Some(&b'\n') {
            return Err(invalid("incomplete or oversized WAL record"));
        }
        Ok(Some(Record {
            start: self.offset,
            end: self.offset + bytes.len() as u64,
            bytes,
        }))
    }

    pub fn acknowledge(&mut self, record: &Record) -> io::Result<()> {
        if record.start != self.offset || record.end > self.end {
            return Err(invalid("WAL acknowledgement must advance contiguously"));
        }
        let checkpoint = Checkpoint {
            version: 1,
            start: record.start,
            end: record.end,
            sha256: format!("{:x}", Sha256::digest(&record.bytes)),
        };
        let temporary = sidecar(&self.checkpoint_path, ".tmp");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, &checkpoint)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &self.checkpoint_path)?;
        sync_parent(&self.checkpoint_path)?;
        self.offset = record.end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("xscope-wal-{}", uuid::Uuid::now_v7()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> PathBuf {
            self.0.join("usage.jsonl")
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    #[test]
    fn checkpoint_skips_acked_events_and_replays_unacked_after_restart() {
        let fixture = Fixture::new();
        let mut wal = Journal::open(&fixture.path()).unwrap();
        wal.append(br#"{"event_id":"one"}"#).unwrap();
        wal.append(br#"{"event_id":"two"}"#).unwrap();
        let first = wal.next().unwrap().unwrap();
        wal.acknowledge(&first).unwrap();
        assert!(wal.acknowledge(&first).is_err());
        let second = wal.next().unwrap().unwrap();
        drop(wal);
        let mut wal = Journal::open(&fixture.path()).unwrap();
        assert_eq!(wal.next().unwrap().unwrap().bytes, second.bytes);
        wal.acknowledge(&second).unwrap();
        drop(wal);
        assert!(
            Journal::open(&fixture.path())
                .unwrap()
                .next()
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn torn_tail_and_mismatched_checkpoint_fail_without_discarding_evidence() {
        let fixture = Fixture::new();
        std::fs::write(fixture.path(), b"{\"torn\":").unwrap();
        assert!(Journal::open(&fixture.path()).is_err());
        assert_eq!(std::fs::read(fixture.path()).unwrap(), b"{\"torn\":");
        std::fs::write(fixture.path(), b"{}\n").unwrap();
        let mut wal = Journal::open(&fixture.path()).unwrap();
        let record = wal.next().unwrap().unwrap();
        wal.acknowledge(&record).unwrap();
        drop(wal);
        std::fs::write(fixture.path(), b"[]\n").unwrap();
        assert!(Journal::open(&fixture.path()).is_err());
    }
    #[test]
    fn a_second_writer_is_rejected() {
        let fixture = Fixture::new();
        let first = Journal::open(&fixture.path()).unwrap();
        assert!(Journal::open(&fixture.path()).is_err());
        drop(first);
        assert!(Journal::open(&fixture.path()).is_ok());
    }
}
