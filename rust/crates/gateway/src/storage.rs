//! Conservative per-process space reservations; not a filesystem quota.
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Maximum intent + completion records plus checkpoint/segment metadata margin.
pub const REQUEST_SPACE: u64 = (2 << 20) + (32 << 10);

#[derive(Clone, Debug)]
pub struct StorageSettings {
    pub segment_bytes: u64,
    pub max_bytes: u64,
    pub min_free_bytes: u64,
}

impl StorageSettings {
    pub fn from_env() -> io::Result<Self> {
        fn number(name: &str, fallback: u64) -> io::Result<u64> {
            match std::env::var(name) {
                Ok(value) => value
                    .parse()
                    .map_err(|_| io::Error::other(format!("invalid {name}"))),
                Err(std::env::VarError::NotPresent) => Ok(fallback),
                Err(e) => Err(io::Error::other(e)),
            }
        }
        let settings = Self {
            segment_bytes: number("XSCOPE_WAL_SEGMENT_BYTES", 16 << 20)?,
            max_bytes: number("XSCOPE_WAL_MAX_BYTES", 192 << 20)?,
            min_free_bytes: number("XSCOPE_WAL_MIN_FREE_BYTES", 32 << 20)?,
        };
        if settings.segment_bytes == 0
            || settings.segment_bytes > settings.max_bytes
            || settings.max_bytes < REQUEST_SPACE
            || settings.min_free_bytes < 4096
        {
            return Err(io::Error::other("invalid WAL storage thresholds"));
        }
        Ok(settings)
    }
}

#[derive(Default)]
struct State {
    retained: u64,
    reserved: u64,
}

pub struct StorageBudget {
    directory: PathBuf,
    pub(crate) settings: StorageSettings,
    state: Mutex<State>,
}

pub struct StoragePermit {
    budget: Arc<StorageBudget>,
}
impl Drop for StoragePermit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.budget.state.lock() {
            state.reserved -= REQUEST_SPACE;
        }
    }
}

impl StorageBudget {
    pub fn new(path: &Path, settings: StorageSettings) -> Arc<Self> {
        Arc::new(Self {
            directory: path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .into(),
            settings,
            state: Mutex::new(State::default()),
        })
    }

    #[allow(clippy::useless_conversion)] // statvfs field widths differ on Darwin/Linux.
    fn allowed(&self, state: &State) -> io::Result<bool> {
        let stats = nix::sys::statvfs::statvfs(&self.directory).map_err(io::Error::other)?;
        let free =
            u64::from(stats.blocks_available()).saturating_mul(u64::from(stats.fragment_size()));
        xscope_telemetry::wal_storage(
            state.retained,
            state.reserved,
            free,
            self.settings.max_bytes,
            self.settings.min_free_bytes,
        );
        Ok(state
            .retained
            .checked_add(state.reserved)
            .and_then(|v| v.checked_add(REQUEST_SPACE))
            .is_some_and(|v| v <= self.settings.max_bytes)
            && state
                .reserved
                .checked_add(REQUEST_SPACE)
                .and_then(|v| v.checked_add(self.settings.min_free_bytes))
                .is_some_and(|v| v <= free))
    }

    pub fn is_ready(&self) -> bool {
        self.state
            .lock()
            .ok()
            .is_some_and(|s| self.allowed(&s).unwrap_or(false))
    }

    pub fn reserve(self: &Arc<Self>) -> io::Result<Arc<StoragePermit>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("storage budget poisoned"))?;
        if !self.allowed(&state)? {
            xscope_telemetry::background_event("wal_capacity", "rejected");
            return Err(io::Error::other(
                "WAL capacity or free-space watermark reached",
            ));
        }
        state.reserved += REQUEST_SPACE;
        Ok(Arc::new(StoragePermit {
            budget: self.clone(),
        }))
    }

    pub(crate) fn add_retained(&self, bytes: u64) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("storage budget poisoned"))?;
        state.retained = state
            .retained
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("WAL size overflow"))?;
        Ok(())
    }
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn reservations_are_bounded_and_returned_only_after_last_owner() {
        let path = std::env::temp_dir().join("unused-storage-test.jsonl");
        let budget = StorageBudget::new(
            &path,
            StorageSettings {
                segment_bytes: 1,
                max_bytes: REQUEST_SPACE,
                min_free_bytes: 4096,
            },
        );
        let first = budget.reserve().unwrap();
        let second_owner = first.clone();
        assert!(!budget.is_ready());
        drop(first);
        assert!(budget.reserve().is_err());
        drop(second_owner);
        assert!(budget.is_ready());
        budget.add_retained(1).unwrap();
        assert!(budget.reserve().is_err());
    }
}
