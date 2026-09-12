//! Bounded, deliberately volatile outbox. Slots cover admission, queued and
//! currently sending requests; no eviction of already accepted completions.
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::usage::{DeliveryError, UsageEvent, deliver};

#[derive(Clone)]
pub struct Settings {
    pub capacity: usize,
    pub workers: usize,
    pub max_age: Duration,
    pub drain: Duration,
    pub grace_seconds: u64,
}

impl Settings {
    pub fn from_env() -> io::Result<Self> {
        fn number(name: &str, default: u64, min: u64, max: u64) -> io::Result<u64> {
            let value = std::env::var(name)
                .unwrap_or_else(|_| default.to_string())
                .parse::<u64>()
                .map_err(|_| io::Error::other(format!("invalid {name}")))?;
            if !(min..=max).contains(&value) {
                return Err(io::Error::other(format!("out of range {name}")));
            }
            Ok(value)
        }
        Ok(Self {
            capacity: number("XSCOPE_USAGE_QUEUE_CAPACITY", 1024, 1, 65536)? as usize,
            workers: number("XSCOPE_USAGE_REPORT_WORKERS", 4, 1, 32)? as usize,
            max_age: Duration::from_secs(number("XSCOPE_USAGE_MAX_PENDING_SECONDS", 30, 1, 3600)?),
            drain: Duration::from_secs(number("XSCOPE_USAGE_DRAIN_SECONDS", 10, 1, 300)?),
            grace_seconds: number("XSCOPE_GATEWAY_GRACE_SECONDS", 20, 1, 600)?,
        })
    }
}

struct Entry {
    event: Option<UsageEvent>,
    queued: Option<Instant>,
    next: Instant,
    attempt: u32,
    busy: bool,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<u64, Entry>,
    sequence: u64,
    draining: bool,
    stopped: bool,
    fatal: bool,
}

pub struct Queue {
    settings: Settings,
    state: Mutex<State>,
    wake: Condvar,
}

pub struct Permit {
    queue: Arc<Queue>,
    id: u64,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.queue.state.lock() {
            // An accepted completion now owns this slot until ACK/disposition.
            if state
                .entries
                .get(&self.id)
                .is_some_and(|e| e.event.is_none())
            {
                state.entries.remove(&self.id);
                self.queue.wake.notify_all();
            }
        }
    }
}

impl Queue {
    pub fn start(settings: Settings, url: String, token: String) -> io::Result<Arc<Self>> {
        if url.is_empty() || token.is_empty() {
            return Err(io::Error::other(
                "memory reporting requires an internal URL and token",
            ));
        }
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| io::Error::other("usage HTTP client initialization failed"))?;
        let queue = Arc::new(Self {
            settings,
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        for _ in 0..queue.settings.workers {
            let (queue, client, url, token) =
                (queue.clone(), client.clone(), url.clone(), token.clone());
            thread::spawn(move || queue.run(client, url, token));
        }
        Ok(queue)
    }

    fn ready(&self, state: &State) -> bool {
        !state.draining
            && !state.stopped
            && !state.fatal
            && state.entries.len() < self.settings.capacity
            && !state.entries.values().any(|e| {
                e.queued
                    .is_some_and(|t| t.elapsed() >= self.settings.max_age)
            })
    }

    pub fn is_ready(&self) -> bool {
        self.state.lock().is_ok_and(|state| self.ready(&state))
    }

    pub fn reserve(self: &Arc<Self>) -> io::Result<Permit> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("usage queue poisoned"))?;
        if !self.ready(&state) {
            xscope_telemetry::background_event("usage_queue", "rejected");
            return Err(io::Error::other(
                "usage queue full, stale, draining or unhealthy",
            ));
        }
        state.sequence = state
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("queue sequence exhausted"))?;
        let id = state.sequence;
        state.entries.insert(
            id,
            Entry {
                event: None,
                queued: None,
                next: Instant::now(),
                attempt: 0,
                busy: false,
            },
        );
        Ok(Permit {
            queue: self.clone(),
            id,
        })
    }

    pub fn submit(&self, permit: &Permit, event: UsageEvent) -> io::Result<()> {
        if !std::ptr::eq(self, permit.queue.as_ref()) {
            return Err(io::Error::other("usage permit belongs to another queue"));
        }
        // Bound bytes as well as record count. Bodies/credentials never enter this queue.
        if serde_json::to_vec(&event)?.len() > 32 * 1024 {
            return Err(io::Error::other("usage event exceeds 32 KiB"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("usage queue poisoned"))?;
        let entry = state
            .entries
            .get_mut(&permit.id)
            .ok_or_else(|| io::Error::other("missing usage queue permit"))?;
        if entry.event.is_some() {
            return Err(io::Error::other("usage permit already submitted"));
        }
        entry.event = Some(event);
        entry.queued = Some(Instant::now());
        entry.next = Instant::now();
        self.wake.notify_all();
        xscope_telemetry::background_event("usage_queue", "enqueued");
        Ok(())
    }

    pub fn begin_drain(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.draining = true;
            self.wake.notify_all();
        }
    }

    /// Called AFTER Pingora has stopped its request runtimes, not at SIGTERM.
    pub fn finish(&self) {
        self.begin_drain();
        let deadline = Instant::now() + self.settings.drain;
        if let Ok(mut state) = self.state.lock() {
            while !state.entries.is_empty() && Instant::now() < deadline {
                let Ok((next, _)) = self.wake.wait_timeout(state, Duration::from_millis(100))
                else {
                    return;
                };
                state = next;
            }
            let remaining = state.entries.len();
            state.stopped = true;
            self.wake.notify_all();
            if remaining > 0 {
                tracing::warn!(
                    remaining,
                    "volatile usage drain deadline reached; central holds require review"
                );
                xscope_telemetry::background_event("usage_queue", "drain_timeout");
            } else {
                xscope_telemetry::background_event("usage_queue", "drained");
            }
        }
    }

    fn run(&self, client: reqwest::blocking::Client, url: String, token: String) {
        loop {
            let work = {
                let Ok(mut state) = self.state.lock() else {
                    return;
                };
                if state.stopped {
                    return;
                }
                let pending = state.entries.values().filter(|e| e.event.is_some()).count();
                let oldest = state
                    .entries
                    .values()
                    .filter_map(|e| e.queued)
                    .map(|t| t.elapsed().as_secs())
                    .max()
                    .unwrap_or(0);
                xscope_telemetry::usage_queue(
                    state.entries.len(),
                    pending,
                    oldest,
                    self.settings.capacity,
                    self.ready(&state),
                );
                let now = Instant::now();
                let next = state
                    .entries
                    .iter_mut()
                    .find(|(_, e)| !e.busy && e.event.is_some() && e.next <= now);
                if let Some((id, entry)) = next {
                    entry.busy = true;
                    entry.event.clone().map(|event| (*id, event))
                } else {
                    if self
                        .wake
                        .wait_timeout(state, Duration::from_millis(100))
                        .is_err()
                    {
                        return;
                    }
                    None
                }
            };
            let Some((id, event)) = work else {
                continue;
            };
            let result = deliver(&client, &url, &token, &event);
            let Ok(mut state) = self.state.lock() else {
                return;
            };
            match result {
                Ok(()) => {
                    state.entries.remove(&id);
                    xscope_telemetry::background_event("usage_export", "success");
                }
                Err(DeliveryError::Retry(status)) => {
                    if let Some(entry) = state.entries.get_mut(&id) {
                        entry.busy = false;
                        entry.attempt = entry.attempt.saturating_add(1);
                        // Bounded exponential backoff with per-event jitter; other records advance.
                        entry.next =
                            Instant::now() + Duration::from_millis(backoff(entry.attempt, id));
                    }
                    tracing::warn!(status, event_id = %event.event_id, "usage HTTP retry pending");
                    xscope_telemetry::background_event("usage_export", "retry");
                }
                Err(DeliveryError::Permanent(status)) => {
                    state.entries.remove(&id);
                    // Bad credentials/configuration require operator repair, not a retry storm.
                    state.fatal |= matches!(status, 300..=399 | 401 | 403 | 404 | 0);
                    tracing::error!(status, request_id = %event.request_id, "usage delivery rejected; central reservation retained for review");
                    xscope_telemetry::background_event("usage_export", "permanent_failure");
                }
            }
            self.wake.notify_all();
        }
    }
}

fn backoff(attempt: u32, id: u64) -> u64 {
    (250_u64.saturating_mul(1_u64 << attempt.min(6))).min(10_000) + id % 251
}

pub struct Drain(pub Arc<Queue>);
#[async_trait::async_trait]
impl pingora_core::services::background::BackgroundService for Drain {
    async fn start(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let _ = shutdown.changed().await;
        self.0.begin_drain();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn queue() -> Arc<Queue> {
        Arc::new(Queue {
            settings: Settings {
                capacity: 1,
                workers: 1,
                max_age: Duration::from_secs(1),
                drain: Duration::from_millis(1),
                grace_seconds: 1,
            },
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        })
    }
    #[test]
    fn in_flight_reserves_capacity_and_cancel_returns_it() {
        let q = queue();
        let p = q.reserve().unwrap();
        assert!(!q.is_ready());
        assert!(q.reserve().is_err());
        drop(p);
        assert!(q.is_ready());
        q.begin_drain();
        assert!(q.reserve().is_err());
    }
    #[test]
    fn old_pending_blocks_admission_but_not_retries() {
        let q = queue();
        let _p = q.reserve().unwrap();
        let mut state = q.state.lock().unwrap();
        state.entries.get_mut(&1).unwrap().queued = Some(Instant::now() - Duration::from_secs(2));
        assert!(!q.ready(&state));
        assert!(backoff(1, 0) < backoff(4, 0));
        assert!(backoff(u32::MAX, u64::MAX) <= 10_250);
    }
}
