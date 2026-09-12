//! Account-scoped leased consumption. No money lock, no automatic settlement,
//! and no acknowledgement unless the idempotent effect commits in the same TX.
use crate::{
    billing::{conflict, invalid},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};
use chrono::{DateTime, FixedOffset, Utc};
use sea_orm::sea_query::{Alias, Expr, ExprTrait, Func, LockBehavior, LockType, OnConflict, Query};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, ConnectionTrait,
    DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xscope_entities::{
    billing_effect as effect, billing_event as event, billing_job as job, ledger_entry,
    operation_audit,
};

pub const CONSUMER: &str = "ledger.audit.v1";

async fn now(tx: &DatabaseTransaction) -> ServiceResult<DateTime<FixedOffset>> {
    let statement = Query::select()
        .expr_as(Func::cust(Alias::new("clock_timestamp")), "now")
        .to_owned();
    let row = tx
        .query_one(&statement)
        .await?
        .ok_or_else(|| invalid("database clock unavailable"))?;
    Ok(row.try_get("", "now")?)
}

/// Called inside the existing account-serialized financial transaction. Idle
/// work is awakened once; new events never clear a failure backoff or dead state.
pub async fn schedule(tx: &DatabaseTransaction, account: &str, sequence: i64) -> ServiceResult<()> {
    let time = Utc::now().fixed_offset();
    job::Entity::insert(job::ActiveModel {
        billing_account_id: Set(account.into()),
        consumer: Set(CONSUMER.into()),
        acknowledged: Set(0),
        target_sequence: Set(sequence),
        lease_epoch: Set(0),
        lease_through: Set(0),
        lease_token: Set(None),
        lease_until: Set(None),
        available_at: Set(time),
        state: Set("pending".into()),
        attempts: Set(0),
        last_error: Set(None),
        updated_at: Set(time),
    })
    .on_conflict(
        OnConflict::columns([job::Column::BillingAccountId, job::Column::Consumer])
            .update_column(job::Column::TargetSequence)
            .value(
                job::Column::State,
                Expr::case(
                    Expr::col((job::Entity, job::Column::State)).eq("idle"),
                    "pending",
                )
                .finally(Expr::col((job::Entity, job::Column::State))),
            )
            .to_owned(),
    )
    .exec_without_returning(tx)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    pub limit: u64,
    pub lease_seconds: i64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub billing_account_id: String,
    pub lease_epoch: i64,
    pub lease_token: String,
    pub expected_sequence: i64,
    pub through_sequence: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailRequest {
    pub lease: Lease,
    pub reason: String,
}

async fn locked(tx: &DatabaseTransaction, lease: &Lease) -> ServiceResult<job::Model> {
    let row = job::Entity::find_by_id((lease.billing_account_id.clone(), CONSUMER.to_owned()))
        .lock_exclusive()
        .one(tx)
        .await?
        .ok_or(ServiceError::NotFound)?;
    let time = now(tx).await?;
    if row.state != "running"
        || row.lease_epoch != lease.lease_epoch
        || row.lease_token.as_deref() != Some(&lease.lease_token)
        || row.lease_until.is_none_or(|until| until <= time)
        || row.acknowledged != lease.expected_sequence
        || lease.through_sequence <= lease.expected_sequence
        || row.lease_through != lease.through_sequence
        || lease.through_sequence > row.target_sequence
    {
        return Err(conflict("stale, expired, or mismatched worker lease"));
    }
    Ok(row)
}

impl Repository {
    pub async fn event_job_status(&self, account: &str) -> ServiceResult<Value> {
        let row = job::Entity::find_by_id((account.to_owned(), CONSUMER.to_owned()))
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        // Never return lease credentials to console users.
        Ok(
            json!({"consumer":CONSUMER,"state":row.state,"acknowledged":row.acknowledged,
            "target_sequence":row.target_sequence,"backlog":row.target_sequence - row.acknowledged,
            "attempts":row.attempts,"last_error":row.last_error,"available_at":row.available_at,"updated_at":row.updated_at}),
        )
    }

    pub async fn event_worker_metrics(&self) -> ServiceResult<()> {
        use sea_orm::sea_query::JoinType;
        let j = Alias::new("j");
        let e = Alias::new("e");
        let query = Query::select()
            .column((j.clone(), Alias::new("state")))
            .expr_as(
                Expr::col((j.clone(), Alias::new("billing_account_id"))).count(),
                "jobs",
            )
            .expr_as(
                Expr::col((j.clone(), Alias::new("target_sequence")))
                    .sub(Expr::col((j.clone(), Alias::new("acknowledged"))))
                    .sum()
                    .cast_as(Alias::new("bigint")),
                "backlog",
            )
            .expr_as(
                Expr::col((e.clone(), Alias::new("created_at"))).min(),
                "oldest",
            )
            .from_as(
                (Alias::new("xscope"), Alias::new("billing_jobs")),
                j.clone(),
            )
            .join_as(
                JoinType::LeftJoin,
                (Alias::new("xscope"), Alias::new("billing_events")),
                e.clone(),
                Expr::col((j.clone(), Alias::new("billing_account_id")))
                    .equals((e.clone(), Alias::new("billing_account_id")))
                    .and(
                        Expr::col((e, Alias::new("sequence"))).eq(Expr::col((
                            j.clone(),
                            Alias::new("acknowledged"),
                        ))
                        .add(1)),
                    ),
            )
            .group_by_col((j, Alias::new("state")))
            .to_owned();
        let rows = self.db.query_all(&query).await?;
        for state in ["pending", "running", "idle", "dead"] {
            xscope_telemetry::event_worker_state(state, 0, 0, 0);
        }
        for row in rows {
            let state: String = row.try_get("", "state")?;
            let jobs: i64 = row.try_get("", "jobs")?;
            let backlog: i64 = row.try_get("", "backlog")?;
            let oldest: Option<DateTime<FixedOffset>> = row.try_get("", "oldest")?;
            let age = oldest.map_or(0, |t| {
                std::cmp::max((Utc::now() - t.with_timezone(&Utc)).num_seconds(), 0)
            });
            xscope_telemetry::event_worker_state(&state, jobs, backlog, age);
        }
        Ok(())
    }
    pub async fn claim_event_job(&self, request: ClaimRequest) -> ServiceResult<Value> {
        if !(1..=100).contains(&request.limit) || !(1..=60).contains(&request.lease_seconds) {
            return Err(invalid("invalid event batch or lease duration"));
        }
        let tx = self.db.begin().await?;
        let time = now(&tx).await?;
        let row = job::Entity::find()
            .filter(job::Column::Consumer.eq(CONSUMER))
            .filter(Expr::col(job::Column::TargetSequence).gt(Expr::col(job::Column::Acknowledged)))
            .filter(job::Column::AvailableAt.lte(time))
            .filter(
                Condition::any().add(job::Column::State.eq("pending")).add(
                    Condition::all()
                        .add(job::Column::State.eq("running"))
                        .add(job::Column::LeaseUntil.lte(time)),
                ),
            )
            .order_by_asc(job::Column::AvailableAt)
            .order_by_asc(job::Column::BillingAccountId)
            .lock_with_behavior(LockType::Update, LockBehavior::SkipLocked)
            .one(&tx)
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(json!({"lease":null,"data":[],"retry_after_ms":1000}));
        };
        if row.state == "running" {
            // A crashed worker receives the same bounded retry policy as an
            // explicit failure. Never silently ACK its unfinished batch.
            let attempts = row.attempts.saturating_add(1);
            let mut active: job::ActiveModel = row.into();
            active.state = Set(if attempts >= 8 { "dead" } else { "pending" }.into());
            active.attempts = Set(attempts);
            active.last_error = Set(Some("worker_crashed".into()));
            active.lease_token = Set(None);
            active.lease_until = Set(None);
            active.available_at = Set(time + chrono::Duration::seconds(backoff(attempts)));
            active.updated_at = Set(time);
            active.update(&tx).await?;
            tx.commit().await?;
            xscope_telemetry::background_event("event_worker", "lease_expired");
            return Ok(json!({"lease":null,"data":[],"retry_after_ms":1000}));
        }
        let events = event::Entity::find()
            .filter(event::Column::BillingAccountId.eq(&row.billing_account_id))
            .filter(event::Column::Sequence.gt(row.acknowledged))
            .filter(event::Column::Sequence.lte(row.target_sequence))
            .order_by_asc(event::Column::Sequence)
            .limit(request.limit)
            .all(&tx)
            .await?;
        let last = events
            .last()
            .ok_or_else(|| conflict("job high watermark has no committed source events"))?;
        let lease = Lease {
            billing_account_id: row.billing_account_id.clone(),
            lease_epoch: row
                .lease_epoch
                .checked_add(1)
                .ok_or_else(|| invalid("lease epoch exhausted"))?,
            lease_token: uuid::Uuid::now_v7().to_string(),
            expected_sequence: row.acknowledged,
            through_sequence: last.sequence,
        };
        let mut active: job::ActiveModel = row.into();
        active.state = Set("running".into());
        active.lease_epoch = Set(lease.lease_epoch);
        active.lease_through = Set(lease.through_sequence);
        active.lease_token = Set(Some(lease.lease_token.clone()));
        active.lease_until = Set(Some(
            time + chrono::Duration::seconds(request.lease_seconds),
        ));
        active.updated_at = Set(time);
        active.update(&tx).await?;
        tx.commit().await?;
        xscope_telemetry::background_event("event_worker", "claimed");
        Ok(json!({"lease":lease,"data":events,"retry_after_ms":0}))
    }

    pub async fn complete_event_job(&self, lease: Lease) -> ServiceResult<Value> {
        let tx = self.db.begin().await?;
        // Lost completion response can be acknowledged without repeating effects.
        let row = job::Entity::find_by_id((lease.billing_account_id.clone(), CONSUMER.to_owned()))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.acknowledged >= lease.through_sequence
            && lease.through_sequence > lease.expected_sequence
        {
            tx.commit().await?;
            return Ok(json!({"acknowledged":row.acknowledged,"duplicate":true}));
        }
        let row = locked(&tx, &lease).await?;
        let events = event::Entity::find()
            .filter(event::Column::BillingAccountId.eq(&row.billing_account_id))
            .filter(event::Column::Sequence.gt(row.acknowledged))
            .filter(event::Column::Sequence.lte(lease.through_sequence))
            .order_by_asc(event::Column::Sequence)
            .limit(101)
            .all(&tx)
            .await?;
        if events.is_empty() || events.len() > 100 {
            return Err(conflict("invalid worker batch"));
        }
        let prior_effects = effect::Entity::find()
            .filter(effect::Column::BillingAccountId.eq(&row.billing_account_id))
            .filter(effect::Column::Consumer.eq(CONSUMER))
            .filter(effect::Column::Sequence.gt(row.acknowledged))
            .filter(effect::Column::Sequence.lte(lease.through_sequence))
            .all(&tx)
            .await?
            .into_iter()
            .map(|item| (item.sequence, item.payload_sha256))
            .collect::<std::collections::BTreeMap<_, _>>();
        let processed_at = now(&tx).await?;
        let mut effects = Vec::with_capacity(events.len());
        let mut expected = row.acknowledged;
        for event in &events {
            expected = expected
                .checked_add(1)
                .ok_or_else(|| invalid("event sequence exhausted"))?;
            if event.sequence != expected {
                return Err(conflict("event source has a sequence gap"));
            }
            validate_event(&tx, event).await?;
            let hash = format!(
                "{:x}",
                Sha256::digest(
                    serde_json::to_vec(event).map_err(|_| invalid("event serialization failed"))?
                )
            );
            if let Some(prior) = prior_effects.get(&event.sequence) {
                if *prior != hash {
                    return Err(conflict("processed event changed"));
                }
            } else {
                effects.push(effect::ActiveModel {
                    billing_account_id: Set(row.billing_account_id.clone()),
                    consumer: Set(CONSUMER.into()),
                    sequence: Set(event.sequence),
                    kind: Set(event.kind.clone()),
                    payload_sha256: Set(hash),
                    processed_at: Set(processed_at),
                });
            }
        }
        if expected != lease.through_sequence {
            return Err(conflict("event source did not reach requested boundary"));
        }
        if !effects.is_empty() {
            effect::Entity::insert_many(effects).exec(&tx).await?;
        }
        // Fence again immediately before commit, including slow validation.
        let time = now(&tx).await?;
        if row.lease_until.is_none_or(|until| until <= time) {
            return Err(conflict("lease expired during processing"));
        }
        let target = row.target_sequence;
        let mut active: job::ActiveModel = row.into();
        active.acknowledged = Set(expected);
        active.state = Set(if expected < target { "pending" } else { "idle" }.into());
        active.lease_token = Set(None);
        active.lease_until = Set(None);
        active.attempts = Set(0);
        active.last_error = Set(None);
        active.available_at = Set(time);
        active.updated_at = Set(time);
        active.update(&tx).await?;
        tx.commit().await?;
        xscope_telemetry::background_event("event_worker", "completed");
        Ok(json!({"acknowledged":expected,"duplicate":false}))
    }

    pub async fn fail_event_job(&self, request: FailRequest) -> ServiceResult<Value> {
        if !["handler_failed", "source_invalid", "dependency_unavailable"]
            .contains(&request.reason.as_str())
        {
            return Err(invalid("invalid bounded failure code"));
        }
        let tx = self.db.begin().await?;
        let row = locked(&tx, &request.lease).await?;
        let attempts = row.attempts.saturating_add(1);
        let time = now(&tx).await?;
        let delay = backoff(attempts);
        let state = if attempts >= 8 { "dead" } else { "pending" };
        let mut active: job::ActiveModel = row.into();
        active.state = Set(state.into());
        active.attempts = Set(attempts);
        active.last_error = Set(Some(request.reason));
        active.lease_token = Set(None);
        active.lease_until = Set(None);
        active.available_at = Set(time + chrono::Duration::seconds(delay));
        active.updated_at = Set(time);
        active.update(&tx).await?;
        tx.commit().await?;
        xscope_telemetry::background_event(
            "event_worker",
            if state == "dead" { "dead" } else { "retry" },
        );
        Ok(json!({"state":state,"retry_after_seconds":delay,"attempts":attempts}))
    }

    pub async fn retry_event_job(
        &self,
        account: &str,
        actor: &str,
        reason: &str,
    ) -> ServiceResult<Value> {
        if reason.trim().is_empty() || reason.len() > 256 {
            return Err(invalid("retry requires a bounded reason"));
        }
        let tx = self.db.begin().await?;
        let row = job::Entity::find_by_id((account.to_owned(), CONSUMER.to_owned()))
            .lock_exclusive()
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.state != "dead" {
            return Err(conflict("only dead jobs may be explicitly retried"));
        }
        let time = now(&tx).await?;
        let mut active: job::ActiveModel = row.into();
        active.state = Set("pending".into());
        active.available_at = Set(time);
        active.attempts = Set(0);
        active.updated_at = Set(time);
        active.update(&tx).await?;
        operation_audit::ActiveModel {
            id: Set(uuid::Uuid::now_v7().to_string()),
            actor_id: Set(actor.into()),
            action: Set("event_job.retry".into()),
            resource_id: Set(account.into()),
            payload: Set(json!({"consumer":CONSUMER,"reason":reason})),
            created_at: Set(time),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(json!({"state":"pending","acknowledgement_unchanged":true}))
    }
}

fn backoff(attempt: i32) -> i64 {
    std::cmp::min(1_i64 << attempt.clamp(1, 8), 300)
}

async fn validate_event(tx: &DatabaseTransaction, event: &event::Model) -> ServiceResult<()> {
    match event.kind.as_str() {
        "ledger.posted" => {
            let rows = ledger_entry::Entity::find()
                .filter(ledger_entry::Column::TransactionId.eq(&event.aggregate_id))
                .limit(101)
                .all(tx)
                .await?;
            if rows.len() < 2
                || rows.len() > 100
                || rows
                    .iter()
                    .any(|r| r.billing_account_id != event.billing_account_id)
                || rows
                    .iter()
                    .map(|r| i128::from(r.amount_microunits))
                    .sum::<i128>()
                    != 0
            {
                return Err(conflict("ledger integrity verification failed"));
            }
        }
        "reservation.created"
        | "reservation.dispatched"
        | "reservation.released"
        | "reservation.unresolved"
        | "reservation.waived"
        | "reservation.settled" => {
            let reservation =
                xscope_entities::billing_reservation::Entity::find_by_id(&event.aggregate_id)
                    .one(tx)
                    .await?
                    .ok_or_else(|| conflict("reservation source missing"))?;
            if reservation.billing_account_id != event.billing_account_id {
                return Err(conflict("reservation source account mismatch"));
            }
        }
        "review.submitted" | "review.settled" | "review.rejected" | "review.waived" => {
            let review = xscope_entities::billing_review::Entity::find_by_id(&event.aggregate_id)
                .one(tx)
                .await?
                .ok_or_else(|| conflict("review source missing"))?;
            let reservation =
                xscope_entities::billing_reservation::Entity::find_by_id(&review.reservation_id)
                    .one(tx)
                    .await?
                    .ok_or_else(|| conflict("review reservation missing"))?;
            if reservation.billing_account_id != event.billing_account_id {
                return Err(conflict("review source account mismatch"));
            }
        }
        // Rebuild events are audit notifications, not financial postings.
        "projection.rebuilt" => {}
        _ => return Err(invalid("unsupported financial event kind; source retained")),
    }
    Ok(())
}

pub async fn run(repository: Repository, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    use tracing::Instrument;
    let mut last_metrics = std::time::Instant::now() - std::time::Duration::from_secs(30);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let work = async {
            if last_metrics.elapsed() >= std::time::Duration::from_secs(30) {
                repository.event_worker_metrics().await?;
                last_metrics = std::time::Instant::now();
            }
            let claim = repository
                .claim_event_job(ClaimRequest {
                    limit: 100,
                    lease_seconds: 30,
                })
                .await?;
            if Value::is_null(&claim["lease"]) {
                return Ok::<bool, ServiceError>(false);
            }
            let lease: Lease = serde_json::from_value(claim["lease"].clone())
                .map_err(|_| invalid("invalid internal lease"))?;
            if let Err(error) = repository.complete_event_job(lease.clone()).await {
                tracing::warn!(error = %error, "event batch failed; source and ACK retained");
                repository
                    .fail_event_job(FailRequest {
                        lease,
                        reason: "handler_failed".into(),
                    })
                    .await?;
            }
            Ok(true)
        }
        .instrument(tracing::info_span!("billing.consume_batch"));
        let busy = tokio::select! { _ = shutdown.changed() => return, result = work => match result {
            Ok(busy) => busy,
            Err(error) => { xscope_telemetry::background_event("event_worker", "error"); tracing::warn!(error = %error, "event worker retrying"); false }
        }};
        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(std::time::Duration::from_millis(if busy { 10 } else { 1000 })) => {} }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn retries_are_positive_bounded_and_monotonic() {
        let delays: Vec<_> = (1..=20).map(super::backoff).collect();
        assert!(delays.iter().all(|delay| *delay > 0 && *delay <= 300));
        assert!(delays.windows(2).all(|w| w[0] <= w[1]));
    }
}
