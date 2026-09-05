//! Internal, account-serialized money protocol and transactional pull outbox.
//! No timer releases ambiguous dispatched requests.
use chrono::{Datelike, TimeZone, Utc};
use sea_orm::sea_query::Alias;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Select, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use xscope_domain::{MICROS_PER_MINOR_UNIT, Model, ceil_minor_units, usage_cost_microunits};
use xscope_entities::{
    api_key, billing_account, billing_event as event,
    billing_reservation as reservation, ledger_entry, usage_event,
};

use crate::{
    error::{ServiceError, ServiceResult},
    repository::{Repository, insert_balanced_entries},
};

const HELD: [&str; 2] = ["reserved", "dispatched"];

pub use xscope_domain::billing::{ReserveRequest, SettleRequest};
pub use crate::billing_feed::{AckRequest, PollRequest};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseRequest {
    pub reason: String,
}
pub(crate) fn conflict(message: &str) -> ServiceError {
    ServiceError::Conflict(message.into())
}
pub(crate) fn invalid(message: &str) -> ServiceError {
    ServiceError::Invalid(message.into())
}
pub(crate) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
}
fn json_value<T: Serialize>(value: &T) -> ServiceResult<Value> {
    serde_json::to_value(value).map_err(|e| ServiceError::Internal(e.to_string()))
}
async fn total<E: EntityTrait>(
    query: Select<E>,
    column: E::Column,
    tx: &DatabaseTransaction,
) -> ServiceResult<i64> {
    use sea_orm::sea_query::ExprTrait;
    // PostgreSQL SUM(bigint) produces numeric. Cast with overflow checking and
    // return a single scalar rather than loading the account's ledger into RAM.
    Ok(query
        .select_only()
        .column_as(column.sum().cast_as(Alias::new("bigint")), "total")
        .into_tuple::<Option<i64>>()
        .one(tx)
        .await?
        .flatten()
        .unwrap_or_default())
}

pub(crate) async fn lock_account(
    tx: &DatabaseTransaction,
    project_id: &str,
) -> ServiceResult<billing_account::Model> {
    billing_account::Entity::find()
        .filter(billing_account::Column::ProjectId.eq(project_id))
        .lock_exclusive()
        .one(tx)
        .await?
        .ok_or(ServiceError::NotFound)
}

pub(crate) async fn balance_and_held(
    tx: &DatabaseTransaction,
    account_id: &str,
) -> ServiceResult<(i64, i64)> {
    let balance = total(
        ledger_entry::Entity::find()
            .filter(ledger_entry::Column::BillingAccountId.eq(account_id))
            .filter(ledger_entry::Column::LedgerAccount.eq("customer_balance")),
        ledger_entry::Column::AmountMicrounits,
        tx,
    )
    .await?;
    let held = total(
        reservation::Entity::find()
            .filter(reservation::Column::BillingAccountId.eq(account_id))
            .filter(reservation::Column::State.is_in(HELD)),
        reservation::Column::ReservedMicrounits,
        tx,
    )
    .await?;
    Ok((balance, held))
}

/// Caller must hold the account row lock. Sequences are account-local and are
/// allocated while that lock is held until COMMIT (not a global DB sequence).
pub(crate) async fn append_event(
    tx: &DatabaseTransaction,
    account_id: &str,
    kind: &str,
    aggregate: &str,
    payload: Value,
) -> ServiceResult<()> {
    let prior = event::Entity::find()
        .filter(event::Column::BillingAccountId.eq(account_id))
        .order_by_desc(event::Column::Sequence)
        .one(tx)
        .await?;
    let sequence = prior
        .map_or(0, |e| e.sequence)
        .checked_add(1)
        .ok_or_else(|| invalid("event sequence exhausted"))?;
    event::ActiveModel {
        billing_account_id: Set(account_id.into()),
        sequence: Set(sequence),
        kind: Set(kind.into()),
        aggregate_id: Set(aggregate.into()),
        payload: Set(payload),
        created_at: Set(Utc::now().fixed_offset()),
    }
    .insert(tx)
    .await?;
    Ok(())
}

impl Repository {
    pub async fn reserve_money(
        &self,
        request: ReserveRequest,
    ) -> ServiceResult<reservation::Model> {
        if [
            &request.id,
            &request.tenant_id,
            &request.project_id,
            &request.api_key_id,
            &request.request_id,
            &request.model_id,
            &request.model_revision,
            &request.price_version,
        ]
        .iter()
        .any(|s| !identifier(s))
            || request.input_token_limit < 0
            || request.output_token_limit < 0
        {
            return Err(invalid("invalid reservation identity or token bounds"));
        }
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, &request.project_id).await?;
        if account.tenant_id != request.tenant_id {
            return Err(ServiceError::Forbidden);
        }
        let spec = json_value(&request)?;
        if let Some(existing) = reservation::Entity::find_by_id(&request.id)
            .one(&tx)
            .await?
        {
            if existing.spec != spec {
                return Err(conflict("reservation ID is bound to a different payload"));
            }
            // Frozen terms survive price changes/key revocation for recovery.
            tx.commit().await?;
            return Ok(existing);
        }
        if account.status != "active" || account.currency != "CNY" {
            return Err(ServiceError::Forbidden);
        }
        let key = api_key::Entity::find_by_id(&request.api_key_id)
            .one(&tx)
            .await?
            .ok_or(ServiceError::Forbidden)?;
        let now = Utc::now();
        if key.project_id != request.project_id
            || key.tenant_id != request.tenant_id
            || key.revoked_at.is_some()
            || key.expires_at.is_some_and(|t| t <= now.fixed_offset())
            || !key.scopes.iter().any(|s| s == "chat.completions")
            || !key.allowed_models.contains(&request.model_id)
        {
            return Err(ServiceError::Forbidden);
        }
        if request.model_id != self.model.id
            || request.price_version != self.model.price_version
            || request
                .input_token_limit
                .checked_add(request.output_token_limit)
                .is_none_or(|v| v == 0 || v > self.model.max_context_tokens)
        {
            return Err(invalid("unknown price/model or invalid context bounds"));
        }
        if reservation::Entity::find()
            .filter(reservation::Column::ProjectId.eq(&request.project_id))
            .filter(reservation::Column::RequestId.eq(&request.request_id))
            .one(&tx)
            .await?
            .is_some()
            || usage_event::Entity::find()
                .filter(usage_event::Column::ProjectId.eq(&request.project_id))
                .filter(usage_event::Column::RequestId.eq(&request.request_id))
                .one(&tx)
                .await?
                .is_some()
        {
            return Err(conflict("request identity has already been used"));
        }
        let amount = usage_cost_microunits(
            &self.model,
            request.input_token_limit,
            request.output_token_limit,
        )?;
        let (balance, held) = balance_and_held(&tx, &account.id).await?;
        if account.enforce_balance
            && balance
                .checked_sub(held)
                .and_then(|v| v.checked_sub(amount))
                .is_none_or(|v| v < 0)
        {
            return Err(ServiceError::InsufficientFunds(
                "available balance cannot cover the reservation".into(),
            ));
        }
        if key.monthly_budget_amount > 0 {
            let month = Utc
                .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
                .single()
                .unwrap()
                .fixed_offset();
            let spent = total(
                usage_event::Entity::find()
                    .filter(usage_event::Column::ApiKeyId.eq(&key.id))
                    .filter(usage_event::Column::OccurredAt.gte(month)),
                usage_event::Column::CostMicrounits,
                &tx,
            )
            .await?;
            // Include unresolved older holds conservatively; no timer refunds them.
            let key_held = total(
                reservation::Entity::find()
                    .filter(reservation::Column::ApiKeyId.eq(&key.id))
                    .filter(reservation::Column::State.is_in(HELD)),
                reservation::Column::ReservedMicrounits,
                &tx,
            )
            .await?;
            let budget = key
                .monthly_budget_amount
                .checked_mul(MICROS_PER_MINOR_UNIT)
                .ok_or_else(|| invalid("budget overflow"))?;
            if spent
                .checked_add(key_held)
                .and_then(|v| v.checked_add(amount))
                .is_none_or(|v| v > budget)
            {
                return Err(ServiceError::InsufficientFunds(
                    "monthly budget cannot cover the reservation".into(),
                ));
            }
        }
        let row = reservation::ActiveModel {
            id: Set(request.id),
            billing_account_id: Set(account.id.clone()),
            project_id: Set(request.project_id),
            tenant_id: Set(request.tenant_id),
            api_key_id: Set(request.api_key_id),
            request_id: Set(request.request_id),
            spec: Set(spec),
            price: Set(json_value(&self.model)?),
            reserved_microunits: Set(amount),
            settled_microunits: Set(None),
            state: Set("reserved".into()),
            completion: Set(None),
            created_at: Set(now.fixed_offset()),
            updated_at: Set(now.fixed_offset()),
        }
        .insert(&tx)
        .await
        .map_err(crate::repository::conflict_or_database)?;
        append_event(
            &tx,
            &account.id,
            "reservation.created",
            &row.id,
            json!({"reserved_microunits": amount, "price_version": self.model.price_version}),
        )
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    pub async fn money_reservation(
        &self,
        project: &str,
        id: &str,
    ) -> ServiceResult<reservation::Model> {
        reservation::Entity::find_by_id(id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)
    }

    pub async fn dispatch_money(
        &self,
        project: &str,
        id: &str,
    ) -> ServiceResult<reservation::Model> {
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let row = reservation::Entity::find_by_id(id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.state == "dispatched" {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state != "reserved" {
            return Err(conflict("only reserved requests may be dispatched"));
        }
        let mut active: reservation::ActiveModel = row.into();
        active.state = Set("dispatched".into());
        active.updated_at = Set(Utc::now().fixed_offset());
        let row = active.update(&tx).await?;
        append_event(&tx, &account.id, "reservation.dispatched", id, json!({})).await?;
        tx.commit().await?;
        Ok(row)
    }

    pub async fn release_money(
        &self,
        project: &str,
        id: &str,
        request: ReleaseRequest,
    ) -> ServiceResult<reservation::Model> {
        if request.reason != "not_dispatched" {
            return Err(invalid(
                "only verified not_dispatched requests can be released",
            ));
        }
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let row = reservation::Entity::find_by_id(id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if row.state == "released" {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state != "reserved" {
            return Err(conflict(
                "dispatched/settled requests cannot be released; reconcile unknown usage",
            ));
        }
        let mut active: reservation::ActiveModel = row.into();
        active.state = Set("released".into());
        active.completion = Set(Some(json!({"reason": request.reason})));
        active.updated_at = Set(Utc::now().fixed_offset());
        let row = active.update(&tx).await?;
        append_event(
            &tx,
            &account.id,
            "reservation.released",
            id,
            json!({"reason": "not_dispatched"}),
        )
        .await?;
        tx.commit().await?;
        Ok(row)
    }

    pub async fn settle_money(
        &self,
        project: &str,
        id: &str,
        request: SettleRequest,
    ) -> ServiceResult<reservation::Model> {
        if request.input_tokens < 0
            || request.output_tokens < 0
            || request.latency_ms < 0
            || !identifier(&request.endpoint_id)
            || !identifier(&request.region)
            || !["succeeded", "cancelled", "provider_error"].contains(&request.status.as_str())
        {
            return Err(invalid(
                "settlement requires known, nonnegative usage and a final outcome",
            ));
        }
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let row = reservation::Entity::find_by_id(id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let payload = json_value(&request)?;
        if row.state == "settled" {
            if row.completion.as_ref() != Some(&payload) {
                return Err(conflict("settlement payload changed"));
            }
            tx.commit().await?;
            return Ok(row);
        }
        if row.state != "dispatched" {
            return Err(conflict("settlement requires a dispatched reservation"));
        }
        let spec: ReserveRequest = serde_json::from_value(row.spec.clone())
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        if request.input_tokens > spec.input_token_limit
            || request.output_tokens > spec.output_token_limit
        {
            return Err(conflict(
                "actual usage exceeds reserved bounds; hold retained for reconciliation",
            ));
        }
        let price: Model = serde_json::from_value(row.price.clone())
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let amount = usage_cost_microunits(&price, request.input_tokens, request.output_tokens)?;
        let now = Utc::now().fixed_offset();
        let usage_id = format!("evt-reservation-{id}");
        usage_event::ActiveModel {
            event_id: Set(usage_id.clone()),
            request_id: Set(row.request_id.clone()),
            occurred_at: Set(now),
            tenant_id: Set(row.tenant_id.clone()),
            project_id: Set(row.project_id.clone()),
            api_key_id: Set(row.api_key_id.clone()),
            model_id: Set(spec.model_id),
            model_revision: Set(spec.model_revision),
            endpoint_id: Set(request.endpoint_id),
            region: Set(request.region),
            price_version: Set(price.price_version),
            input_tokens: Set(request.input_tokens),
            output_tokens: Set(request.output_tokens),
            cached_input_tokens: Set(0),
            latency_ms: Set(request.latency_ms),
            status: Set(request.status),
            cost_amount: Set(ceil_minor_units(amount)),
            cost_microunits: Set(amount),
            currency: Set(account.currency.clone()),
            received_at: Set(now),
        }
        .insert(&tx)
        .await?;
        let mut active: reservation::ActiveModel = row.into();
        active.state = Set("settled".into());
        active.settled_microunits = Set(Some(amount));
        active.completion = Set(Some(payload));
        active.updated_at = Set(now);
        let row = active.update(&tx).await?;
        if amount > 0 {
            insert_balanced_entries(
                &tx,
                &account,
                &row.tenant_id,
                "usage",
                "usage_event",
                &usage_id,
                &format!("reservation:{id}"),
                "Reserved model inference usage",
                -amount,
                "usage_revenue",
            )
            .await?;
        }
        append_event(
            &tx,
            &account.id,
            "reservation.settled",
            id,
            json!({"usage_event_id": usage_id, "settled_microunits": amount}),
        )
        .await?;
        tx.commit().await?;
        Ok(row)
    }

}
