//! Consumer cursors are independent of monetary admission locks. Event writers
//! retain account-serialized sequences, so committed pages have no commit gaps.
use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, Condition, DatabaseTransaction, EntityTrait,
    QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use xscope_entities::{
    billing_account, billing_consumer as consumer, billing_event as event,
    billing_reservation as reservation,
};

use crate::{
    billing::{conflict, identifier, invalid},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollRequest {
    pub limit: u64,
    /// Optionally ACK the prior completed batch while fetching the next one.
    /// Only already-delivered events may be acknowledged, never this new page.
    pub ack: Option<AckRequest>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AckRequest {
    pub expected_sequence: i64,
    pub sequence: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingRequest {
    #[serde(default = "page_limit")]
    pub limit: u64,
    pub created_before: Option<DateTime<Utc>>,
    pub after_created_at: Option<DateTime<Utc>>,
    pub after_id: Option<String>,
}

fn page_limit() -> u64 {
    50
}

fn acknowledged(row: &consumer::Model, ack: Option<&AckRequest>) -> ServiceResult<i64> {
    let Some(ack) = ack else {
        return Ok(row.acknowledged);
    };
    if ack.sequence < 0 || ack.expected_sequence < 0 {
        return Err(invalid("invalid ACK"));
    }
    // Exact duplicate ACK remains idempotent even after its response was lost.
    if ack.sequence == row.acknowledged {
        return Ok(row.acknowledged);
    }
    if ack.expected_sequence != row.acknowledged
        || ack.sequence < row.acknowledged
        || ack.sequence > row.delivered
    {
        return Err(conflict(
            "stale ACK or acknowledging events that were not delivered",
        ));
    }
    Ok(ack.sequence)
}

async fn account_id(tx: &DatabaseTransaction, project: &str) -> ServiceResult<String> {
    // A plain MVCC read, NEVER FOR UPDATE on the money account.
    billing_account::Entity::find()
        .select_only()
        .column(billing_account::Column::Id)
        .filter(billing_account::Column::ProjectId.eq(project))
        .into_tuple::<String>()
        .one(tx)
        .await?
        .ok_or(ServiceError::NotFound)
}

async fn cursor(
    tx: &DatabaseTransaction,
    account: &str,
    name: &str,
    exclusive: bool,
) -> ServiceResult<Option<consumer::Model>> {
    let query = consumer::Entity::find_by_id((account.to_owned(), name.to_owned()));
    if exclusive {
        query.lock_exclusive().one(tx).await
    } else {
        query.one(tx).await
    }
    .map_err(ServiceError::from)
}

struct Page {
    events: Vec<event::Model>,
    acknowledged: i64,
    delivered: i64,
    has_more: bool,
}

impl Page {
    fn changed(&self, row: &consumer::Model) -> bool {
        self.acknowledged != row.acknowledged || self.delivered != row.delivered
    }

    fn response(self) -> Value {
        json!({"retry_after_ms": if self.events.is_empty() { 1000 } else { 0 },
            "data": self.events, "acknowledged": self.acknowledged,
            "delivered": self.delivered, "has_more": self.has_more})
    }
}

async fn page(
    tx: &DatabaseTransaction,
    row: &consumer::Model,
    request: &PollRequest,
) -> ServiceResult<Page> {
    let ack = acknowledged(row, request.ack.as_ref())?;
    let mut events = event::Entity::find()
        .filter(event::Column::BillingAccountId.eq(&row.billing_account_id))
        .filter(event::Column::Sequence.gt(ack))
        .order_by_asc(event::Column::Sequence)
        .limit(request.limit + 1)
        .all(tx)
        .await?;
    let has_more = events.len() as u64 > request.limit;
    if has_more {
        events.pop();
    }
    let delivered = events
        .last()
        .map_or(row.delivered, |e| row.delivered.max(e.sequence));
    Ok(Page {
        events,
        acknowledged: ack,
        delivered,
        has_more,
    })
}

impl Repository {
    /// Read-only reconciliation discovery. Dispatched includes potentially
    /// in-flight work: age alone is NEVER permission to settle or release it.
    pub async fn pending_reservations(
        &self,
        project: &str,
        request: PendingRequest,
    ) -> ServiceResult<Value> {
        if !(1..=100).contains(&request.limit)
            || request.after_id.is_some() != request.after_created_at.is_some()
            || request.after_id.as_ref().is_some_and(|id| !identifier(id))
        {
            return Err(invalid("invalid pending-reservation limit or cursor"));
        }
        let cutoff = request
            .created_before
            .unwrap_or_else(|| Utc::now() - chrono::Duration::minutes(5));
        if cutoff > Utc::now() {
            return Err(invalid("created_before must not be in the future"));
        }
        let tx = self.db.begin().await?;
        let account = account_id(&tx, project).await?;
        let mut query = reservation::Entity::find()
            .filter(reservation::Column::BillingAccountId.eq(account))
            .filter(reservation::Column::State.eq("dispatched"))
            .filter(reservation::Column::CreatedAt.lte(cutoff.fixed_offset()));
        if let (Some(after), Some(id)) = (request.after_created_at, request.after_id) {
            query = query.filter(
                Condition::any()
                    .add(reservation::Column::CreatedAt.gt(after.fixed_offset()))
                    .add(
                        Condition::all()
                            .add(reservation::Column::CreatedAt.eq(after.fixed_offset()))
                            .add(reservation::Column::Id.gt(id)),
                    ),
            );
        }
        let mut rows = query
            .order_by_asc(reservation::Column::CreatedAt)
            .order_by_asc(reservation::Column::Id)
            .limit(request.limit + 1)
            .all(&tx)
            .await?;
        let has_more = rows.len() as u64 > request.limit;
        if has_more {
            rows.pop();
        }
        let next =
            if has_more {
                rows.last().map(|r| json!({
            "after_created_at": r.created_at, "after_id": r.id, "created_before": cutoff,
        }))
            } else {
                None
            };
        tx.commit().await?;
        Ok(json!({"data": rows, "created_before": cutoff, "next": next,
            "requires_usage_evidence": true}))
    }

    pub async fn poll_billing_events(
        &self,
        project: &str,
        name: &str,
        request: PollRequest,
    ) -> ServiceResult<Value> {
        if !identifier(name) || !(1..=100).contains(&request.limit) {
            return Err(invalid("consumer or batch limit invalid"));
        }
        let tx = self.db.begin().await?;
        let account = account_id(&tx, project).await?;
        let row = if let Some(row) = cursor(&tx, &account, name, false).await? {
            row
        } else {
            // Concurrent first polls converge on one row. No global/account
            // lock; only first registration may wait on the FK parent key lock.
            consumer::Entity::insert(consumer::ActiveModel {
                billing_account_id: Set(account.clone()),
                consumer: Set(name.into()),
                acknowledged: Set(0),
                delivered: Set(0),
                updated_at: Set(Utc::now().fixed_offset()),
            })
            .on_conflict(
                OnConflict::columns([
                    consumer::Column::BillingAccountId,
                    consumer::Column::Consumer,
                ])
                .do_nothing()
                .to_owned(),
            )
            .exec_without_returning(&tx)
            .await?;
            cursor(&tx, &account, name, false)
                .await?
                .ok_or(ServiceError::NotFound)?
        };
        let initial = page(&tx, &row, &request).await?;
        if !initial.changed(&row) {
            // Empty polls AND redeliveries are read-only. Even FOR UPDATE can
            // dirty PostgreSQL heap pages, so do not acquire a row lock here.
            // Concurrent progress may make this response stale: duplicate
            // delivery is allowed; subsequent mutations still check CAS.
            tx.commit().await?;
            xscope_telemetry::background_event("billing_poll", "unchanged");
            return Ok(initial.response());
        }
        let row = cursor(&tx, &account, name, true)
            .await?
            .ok_or(ServiceError::NotFound)?;
        // Re-read after acquiring this consumer's lock. A concurrent ACK/poll
        // may have progressed since the optimistic read; never overwrite it.
        let result = page(&tx, &row, &request).await?;
        if result.changed(&row) {
            let mut active: consumer::ActiveModel = row.into();
            active.acknowledged = Set(result.acknowledged);
            active.delivered = Set(result.delivered);
            active.updated_at = Set(Utc::now().fixed_offset());
            active.update(&tx).await?;
        }
        tx.commit().await?;
        xscope_telemetry::background_event("billing_poll", "progress");
        Ok(result.response())
    }

    pub async fn ack_billing_events(
        &self,
        project: &str,
        name: &str,
        request: AckRequest,
    ) -> ServiceResult<consumer::Model> {
        if !identifier(name) {
            return Err(invalid("invalid consumer"));
        }
        let tx = self.db.begin().await?;
        let account = account_id(&tx, project).await?;
        let row = cursor(&tx, &account, name, false)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if acknowledged(&row, Some(&request))? == row.acknowledged {
            tx.commit().await?;
            return Ok(row);
        }
        let row = cursor(&tx, &account, name, true)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let next = acknowledged(&row, Some(&request))?;
        if next == row.acknowledged {
            tx.commit().await?;
            return Ok(row);
        }
        let mut active: consumer::ActiveModel = row.into();
        active.acknowledged = Set(next);
        active.updated_at = Set(Utc::now().fixed_offset());
        let row = active.update(&tx).await?;
        tx.commit().await?;
        Ok(row)
    }
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn cumulative_ack_preserves_idempotence_and_delivery_boundary() {
        let row = consumer::Model {
            billing_account_id: "a".into(),
            consumer: "c".into(),
            acknowledged: 10,
            delivered: 20,
            updated_at: Utc::now().fixed_offset(),
        };
        assert_eq!(acknowledged(&row, None).unwrap(), 10);
        for (expected_sequence, sequence, ok) in [
            (0, 10, true),
            (10, 20, true),
            (0, 20, false),
            (10, 9, false),
            (10, 21, false),
            (-1, 10, false),
        ] {
            assert_eq!(
                acknowledged(
                    &row,
                    Some(&AckRequest {
                        expected_sequence,
                        sequence
                    })
                )
                .is_ok(),
                ok
            );
        }
    }
}
