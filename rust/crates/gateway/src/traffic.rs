//! One lock serializes local admission and a drain ACK. Counters include
//! admission and the entire HTTP/SSE lifetime, including cancellation cleanup.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use xscope_domain::traffic::{LEASE_SECONDS, TrafficReport, TrafficSnapshot};

#[derive(Default)]
struct State {
    snapshot: Option<TrafficSnapshot>,
    controls: BTreeMap<String, xscope_domain::traffic::PoolControl>,
    deadline: Option<Instant>,
    active: BTreeMap<String, u64>,
    closed: bool,
}
pub struct Traffic {
    pub session_id: String,
    state: Mutex<State>,
}
pub(crate) struct Permit {
    traffic: Arc<Traffic>,
    pool: String,
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.traffic.state.lock()
            && let Some(count) = state.active.get_mut(&self.pool)
        {
            *count = count.saturating_sub(1);
        }
    }
}
impl Traffic {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            session_id: uuid::Uuid::now_v7().to_string(),
            state: Mutex::new(State::default()),
        })
    }
    pub fn install(
        &self,
        snapshot: TrafficSnapshot,
        fetched_at: Instant,
    ) -> Result<(), &'static str> {
        if snapshot.session_id != self.session_id
            || snapshot.sequence <= 0
            || snapshot.lease_seconds != LEASE_SECONDS
            || uuid::Uuid::parse_str(&snapshot.nonce).is_err()
            || snapshot.pools.len() > 10000
        {
            return Err("invalid traffic grant");
        }
        let mut ids = std::collections::HashSet::new();
        for pool in &snapshot.pools {
            if !xscope_domain::traffic::managed(&pool.id)
                || pool.generation <= 0
                || !ids.insert(&pool.id)
            {
                return Err("invalid traffic pool");
            }
        }
        let controls: BTreeMap<_, _> = snapshot
            .pools
            .iter()
            .cloned()
            .map(|p| (p.id.clone(), p))
            .collect();
        let mut state = self.state.lock().map_err(|_| "traffic gate unavailable")?;
        if let Some(previous) = &state.snapshot {
            if previous.sequence >= snapshot.sequence {
                return Err("stale traffic grant");
            }
            for old in &previous.pools {
                if !controls
                    .get(&old.id)
                    .is_some_and(|p| p.generation >= old.generation)
                {
                    return Err("traffic grant removed or rewound a participant");
                }
            }
        }
        for pool in &snapshot.pools {
            state.active.entry(pool.id.clone()).or_default();
        }
        state.deadline = Some(fetched_at + Duration::from_secs(LEASE_SECONDS));
        state.controls = controls;
        state.snapshot = Some(snapshot);
        Ok(())
    }
    pub(crate) fn enter(
        self: &Arc<Self>,
        pool: &str,
        generation: i64,
    ) -> Result<Permit, &'static str> {
        let mut state = self.state.lock().map_err(|_| "traffic gate unavailable")?;
        if state.closed
            || state.deadline.is_none_or(|t| t <= Instant::now())
            || !state
                .controls
                .get(pool)
                .is_some_and(|p| p.generation == generation && p.accepting)
        {
            return Err("pool is unready, draining, or traffic grant expired");
        }
        let count = state.active.entry(pool.into()).or_default();
        *count = count
            .checked_add(1)
            .ok_or("active request count exhausted")?;
        Ok(Permit {
            traffic: self.clone(),
            pool: pool.into(),
        })
    }
    pub fn report(&self) -> Option<TrafficReport> {
        let state = self.state.lock().ok()?;
        let snapshot = state.snapshot.as_ref()?;
        Some(TrafficReport {
            closed: state.closed,
            session_id: self.session_id.clone(),
            sequence: snapshot.sequence,
            nonce: snapshot.nonce.clone(),
            active: state.active.clone(),
        })
    }
    pub fn admission_headers(&self) -> Option<(String, String)> {
        let state = self.state.lock().ok()?;
        if state.closed || state.deadline.is_none_or(|v| v <= Instant::now()) {
            return None;
        }
        let snapshot = state.snapshot.as_ref()?;
        if snapshot.admission_token.is_empty() {
            return None;
        }
        Some((self.session_id.clone(), snapshot.admission_token.clone()))
    }
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use xscope_domain::traffic::PoolControl;
    fn snapshot(t: &Traffic, sequence: i64, generation: i64, accepting: bool) -> TrafficSnapshot {
        TrafficSnapshot {
            admission_token: String::new(),
            session_id: t.session_id.clone(),
            sequence,
            nonce: uuid::Uuid::now_v7().to_string(),
            lease_seconds: LEASE_SECONDS,
            pools: vec![PoolControl {
                id: "managed-synthetic".into(),
                generation,
                accepting,
            }],
        }
    }
    #[test]
    fn drain_fences_old_contexts_and_counts_until_drop() {
        let t = Traffic::new();
        t.install(snapshot(&t, 1, 1, true), Instant::now()).unwrap();
        let first = t.enter("managed-synthetic", 1).unwrap();
        t.install(snapshot(&t, 2, 2, false), Instant::now())
            .unwrap();
        assert!(t.enter("managed-synthetic", 1).is_err());
        assert!(t.enter("managed-synthetic", 2).is_err());
        assert_eq!(t.report().unwrap().active["managed-synthetic"], 1);
        drop(first);
        assert_eq!(t.report().unwrap().active["managed-synthetic"], 0);
        assert!(t.install(snapshot(&t, 3, 1, true), Instant::now()).is_err());
        t.install(
            snapshot(&t, 4, 3, true),
            Instant::now() - Duration::from_secs(LEASE_SECONDS + 1),
        )
        .unwrap();
        assert!(t.enter("managed-synthetic", 3).is_err());
        t.close();
        t.install(snapshot(&t, 5, 3, true), Instant::now()).unwrap();
        assert!(t.enter("managed-synthetic", 3).is_err());
    }
}
