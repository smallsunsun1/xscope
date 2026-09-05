//! Rebuildable financial projections, never an eventually consistent cache.
//! All ensure/mutation helpers require the owning account lock through COMMIT.
use chrono::{Datelike, Months, NaiveDate, NaiveTime, Utc};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    TransactionTrait,
};
use serde::Deserialize;
use serde_json::{Value, json};
use xscope_entities::{
    api_key, billing_balance as balance, billing_key_hold as hold, billing_month_spend as spend,
    billing_reservation as reservation, usage_event,
};

use crate::{
    billing::{invalid, lock_account, total},
    error::{ServiceError, ServiceResult},
    repository::Repository,
};

// Every NaiveDate's year/month has a valid first day by construction.
#[allow(clippy::expect_used)]
pub fn month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).expect("valid first day")
}

fn add(value: i64, delta: i64, nonnegative: bool) -> ServiceResult<i64> {
    value
        .checked_add(delta)
        .filter(|v| !nonnegative || *v >= 0)
        .ok_or_else(|| {
            ServiceError::Internal("billing projection overflow or negative hold/spend".into())
        })
}

pub async fn account(tx: &DatabaseTransaction, id: &str) -> ServiceResult<balance::Model> {
    if let Some(row) = balance::Entity::find_by_id(id).one(tx).await? {
        return Ok(row);
    }
    let (booked, held) = crate::billing::historical_balance_and_held(tx, id).await?;
    Ok(balance::ActiveModel {
        billing_account_id: Set(id.into()),
        balance_microunits: Set(booked),
        held_microunits: Set(held),
        updated_at: Set(Utc::now().fixed_offset()),
    }
    .insert(tx)
    .await?)
}

async fn historical_key_hold(tx: &DatabaseTransaction, id: &str) -> ServiceResult<i64> {
    total(
        reservation::Entity::find()
            .filter(reservation::Column::ApiKeyId.eq(id))
            .filter(reservation::Column::State.is_in(["reserved", "dispatched"])),
        reservation::Column::ReservedMicrounits,
        tx,
    )
    .await
}

pub async fn key_hold(tx: &DatabaseTransaction, id: &str) -> ServiceResult<hold::Model> {
    if let Some(row) = hold::Entity::find_by_id(id).one(tx).await? {
        return Ok(row);
    }
    Ok(hold::ActiveModel {
        api_key_id: Set(id.into()),
        held_microunits: Set(historical_key_hold(tx, id).await?),
        updated_at: Set(Utc::now().fixed_offset()),
    }
    .insert(tx)
    .await?)
}

async fn historical_spend(
    tx: &DatabaseTransaction,
    id: &str,
    period: NaiveDate,
) -> ServiceResult<i64> {
    let end = period
        .checked_add_months(Months::new(1))
        .ok_or_else(|| invalid("invalid month"))?;
    total(
        usage_event::Entity::find()
            .filter(usage_event::Column::ApiKeyId.eq(id))
            .filter(
                usage_event::Column::OccurredAt
                    .gte(period.and_time(NaiveTime::MIN).and_utc().fixed_offset()),
            )
            .filter(
                usage_event::Column::OccurredAt
                    .lt(end.and_time(NaiveTime::MIN).and_utc().fixed_offset()),
            ),
        usage_event::Column::CostMicrounits,
        tx,
    )
    .await
}

pub async fn monthly(
    tx: &DatabaseTransaction,
    id: &str,
    period: NaiveDate,
) -> ServiceResult<spend::Model> {
    if let Some(row) = spend::Entity::find_by_id((id.to_owned(), period))
        .one(tx)
        .await?
    {
        return Ok(row);
    }
    Ok(spend::ActiveModel {
        api_key_id: Set(id.into()),
        month: Set(period),
        spent_microunits: Set(historical_spend(tx, id, period).await?),
        updated_at: Set(Utc::now().fixed_offset()),
    }
    .insert(tx)
    .await?)
}

/// Call BEFORE changing reservation state, so lazy initialization sees the old
/// truth. Both account and key totals change in the same transaction.
pub async fn change_hold(
    tx: &DatabaseTransaction,
    account_id: &str,
    key: &str,
    delta: i64,
) -> ServiceResult<()> {
    let row = account(tx, account_id).await?;
    let amount = add(row.held_microunits, delta, true)?;
    let mut active: balance::ActiveModel = row.into();
    active.held_microunits = Set(amount);
    active.updated_at = Set(Utc::now().fixed_offset());
    active.update(tx).await?;
    let row = key_hold(tx, key).await?;
    let amount = add(row.held_microunits, delta, true)?;
    let mut active: hold::ActiveModel = row.into();
    active.held_microunits = Set(amount);
    active.updated_at = Set(Utc::now().fixed_offset());
    active.update(tx).await?;
    Ok(())
}

/// Call BEFORE inserting the matching ledger entries, never on an idempotent replay.
pub async fn change_balance(tx: &DatabaseTransaction, id: &str, delta: i64) -> ServiceResult<()> {
    let row = account(tx, id).await?;
    let amount = add(row.balance_microunits, delta, false)?;
    let mut active: balance::ActiveModel = row.into();
    active.balance_microunits = Set(amount);
    active.updated_at = Set(Utc::now().fixed_offset());
    active.update(tx).await?;
    Ok(())
}

/// Initialize BEFORE inserting usage. Apply only after dedup accepts the event.
pub async fn change_spend(
    tx: &DatabaseTransaction,
    row: spend::Model,
    delta: i64,
) -> ServiceResult<()> {
    let amount = add(row.spent_microunits, delta, true)?;
    let mut active: spend::ActiveModel = row.into();
    active.spent_microunits = Set(amount);
    active.updated_at = Set(Utc::now().fixed_offset());
    active.update(tx).await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    pub api_key_id: String,
    pub month: NaiveDate,
}

impl Repository {
    /// Snapshot readers do not lock initialized counters. The cold path
    /// serializes backfill with writers; no missing projection is treated as 0.
    pub async fn projected_spend(
        &self,
        project: &str,
        key: &str,
        period: NaiveDate,
    ) -> ServiceResult<i64> {
        if let Some(row) = spend::Entity::find_by_id((key.to_owned(), period))
            .one(&self.db)
            .await?
        {
            return Ok(row.spent_microunits);
        }
        let tx = self.db.begin().await?;
        lock_account(&tx, project).await?;
        let result = monthly(&tx, key, period).await?.spent_microunits;
        tx.commit().await?;
        Ok(result)
    }

    /// Explicit maintenance, not a hot-path/background history scan. Rebuild
    /// changes derived counters only, preserving all source records and holds.
    pub async fn projection_check(
        &self,
        project: &str,
        request: CheckRequest,
        rebuild: bool,
    ) -> ServiceResult<Value> {
        if request.month.day() != 1 {
            return Err(invalid("month must be the first UTC calendar day"));
        }
        let tx = self.db.begin().await?;
        let owner = lock_account(&tx, project).await?;
        api_key::Entity::find_by_id(&request.api_key_id)
            .filter(api_key::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let (booked, held) = crate::billing::historical_balance_and_held(&tx, &owner.id).await?;
        let key_held = historical_key_hold(&tx, &request.api_key_id).await?;
        let spent = historical_spend(&tx, &request.api_key_id, request.month).await?;
        let a = balance::Entity::find_by_id(&owner.id).one(&tx).await?;
        let k = hold::Entity::find_by_id(&request.api_key_id)
            .one(&tx)
            .await?;
        let m = spend::Entity::find_by_id((request.api_key_id.clone(), request.month))
            .one(&tx)
            .await?;
        let consistent = a
            .as_ref()
            .is_some_and(|v| v.balance_microunits == booked && v.held_microunits == held)
            && k.as_ref().is_some_and(|v| v.held_microunits == key_held)
            && m.as_ref().is_some_and(|v| v.spent_microunits == spent);
        let before = json!({"account": a, "key": k, "month": m});
        if rebuild {
            // Initialization may scan once more on absent counters; explicit
            // maintenance is bounded to one account/key/month, not all tenants.
            let mut active: balance::ActiveModel = account(&tx, &owner.id).await?.into();
            active.balance_microunits = Set(booked);
            active.held_microunits = Set(held);
            active.updated_at = Set(Utc::now().fixed_offset());
            active.update(&tx).await?;
            let mut active: hold::ActiveModel = key_hold(&tx, &request.api_key_id).await?.into();
            active.held_microunits = Set(key_held);
            active.updated_at = Set(Utc::now().fixed_offset());
            active.update(&tx).await?;
            let mut active: spend::ActiveModel = monthly(&tx, &request.api_key_id, request.month)
                .await?
                .into();
            active.spent_microunits = Set(spent);
            active.updated_at = Set(Utc::now().fixed_offset());
            active.update(&tx).await?;
            crate::billing::append_event(&tx, &owner.id, "projection.rebuilt", &request.api_key_id,
                json!({"month": request.month, "before": before, "balance": booked, "held": held, "key_held": key_held, "spent": spent})).await?;
        }
        tx.commit().await?;
        Ok(
            json!({"consistent_before": consistent, "rebuilt": rebuild, "stored": before,
            "expected": {"balance_microunits": booked, "held_microunits": held, "key_held_microunits": key_held, "spent_microunits": spent}}),
        )
    }
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn checked_projection_arithmetic() {
        assert_eq!(add(10, -10, true).unwrap(), 0);
        assert_eq!(add(0, -10, false).unwrap(), -10);
        assert!(add(0, -1, true).is_err());
        assert!(add(i64::MAX, 1, false).is_err());
        assert_eq!(
            month(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()).day(),
            1
        );
    }
}
