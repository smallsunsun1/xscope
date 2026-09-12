//! Internal, account-serialized money protocol and transactional pull outbox.
//! No timer releases ambiguous dispatched requests.
use chrono::Utc;
use sea_orm::sea_query::Alias;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Select, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use xscope_domain::{MICROS_PER_MINOR_UNIT, Model, ceil_minor_units, usage_cost_microunits};
use xscope_entities::{
    api_key, billing_account, billing_event as event, billing_reservation as reservation,
    ledger_entry, usage_event,
};

use crate::{
    error::{ServiceError, ServiceResult},
    repository::{Repository, insert_balanced_entries},
};

const HELD: [&str; 2] = ["reserved", "dispatched"];

pub async fn monitor_pending(
    repository: Repository,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = tick.tick() => {
                for state in HELD {
                    // Two index seeks, not a count/scan of the financial history.
                    let query = reservation::Entity::find()
                        .select_only().column(reservation::Column::CreatedAt)
                        .filter(reservation::Column::State.eq(state))
                        .order_by_asc(reservation::Column::CreatedAt).limit(1)
                        .into_tuple::<chrono::DateTime<chrono::FixedOffset>>().one(&repository.db);
                    let result = tokio::select! { _ = shutdown.changed() => return, result = query => result };
                    match result {
                        Ok(oldest) => xscope_telemetry::pending_hold_age(state, oldest.map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds()).unwrap_or(0)),
                        Err(_) => xscope_telemetry::background_event("billing_pending_monitor", "error"),
                    }
                }
            }
        }
    }
}

pub use crate::billing_feed::{AckRequest, PollRequest};
pub use xscope_domain::billing::{ReserveRequest, SettleRequest};

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
pub(crate) async fn total<E: EntityTrait>(
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

pub(crate) async fn historical_balance_and_held(
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

pub(crate) async fn balance_and_held(
    tx: &DatabaseTransaction,
    account_id: &str,
) -> ServiceResult<(i64, i64)> {
    let row = crate::billing_projection::account(tx, account_id).await?;
    Ok((row.balance_microunits, row.held_microunits))
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
        .select_only()
        .column(event::Column::Sequence)
        .filter(event::Column::BillingAccountId.eq(account_id))
        .order_by_desc(event::Column::Sequence)
        .into_tuple::<i64>()
        .one(tx)
        .await?;
    let sequence = prior
        .unwrap_or(0)
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
    crate::event_worker::schedule(tx, account_id, sequence).await?;
    Ok(())
}

impl Repository {
    pub async fn unresolved_money(
        &self,
        project: &str,
        id: &str,
        request: xscope_domain::billing::UnresolvedRequest,
    ) -> ServiceResult<reservation::Model> {
        if !identifier(&request.event_id)
            || !matches!(
                request.reason.as_str(),
                "usage_missing" | "usage_over_limit"
            )
            || (request.reason == "usage_missing"
                && (request.input_tokens.is_some() || request.output_tokens.is_some()))
            || (request.reason == "usage_over_limit"
                && (request.input_tokens.is_none() || request.output_tokens.is_none()))
        {
            return Err(invalid("invalid unresolved usage report"));
        }
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let row = reservation::Entity::find_by_id(id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if matches!(row.state.as_str(), "settled" | "waived") {
            tx.commit().await?;
            return Ok(row);
        }
        if row.state != "dispatched" {
            return Err(conflict("unresolved usage requires dispatched reservation"));
        }
        let payload = json_value(&request)?;
        if let Some(previous) = &row.completion {
            if previous != &payload {
                return Err(conflict("unresolved evidence changed"));
            }
            tx.commit().await?;
            return Ok(row);
        }
        let mut active: reservation::ActiveModel = row.into();
        active.completion = Set(Some(payload.clone()));
        active.updated_at = Set(Utc::now().fixed_offset());
        let row = active.update(&tx).await?;
        append_event(&tx, &account.id, "reservation.unresolved", id, payload).await?;
        tx.commit().await?;
        Ok(row)
    }

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
            || !key
                .scopes
                .iter()
                .any(|s| matches!(s.as_str(), "chat.completions" | "completions"))
            || !key.allowed_models.contains(&request.model_id)
        {
            return Err(ServiceError::Forbidden);
        }
        let model = crate::catalog::active_model(&tx, &request.model_id).await?;
        if request.price_version != model.price_version
            || request
                .input_token_limit
                .checked_add(request.output_token_limit)
                .is_none_or(|v| v == 0 || v > model.max_context_tokens)
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
            &model,
            request.input_token_limit,
            request.output_token_limit,
        )?;
        // Postpaid admission does not need a balance-history scan. Monthly
        // key budgets below remain independently enforced when configured.
        if account.enforce_balance {
            let (balance, held) = balance_and_held(&tx, &account.id).await?;
            if balance
                .checked_sub(held)
                .and_then(|v| v.checked_sub(amount))
                .is_none_or(|v| v < 0)
            {
                return Err(ServiceError::InsufficientFunds(
                    "available balance cannot cover the reservation".into(),
                ));
            }
        }
        if key.monthly_budget_amount > 0 {
            let month = crate::billing_projection::month(now.date_naive());
            let spent = crate::billing_projection::monthly(&tx, &key.id, month)
                .await?
                .spent_microunits;
            // Include unresolved older holds conservatively; no timer refunds them.
            let key_held = crate::billing_projection::key_hold(&tx, &key.id)
                .await?
                .held_microunits;
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
        crate::billing_projection::change_hold(&tx, &account.id, &key.id, amount).await?;
        let row = reservation::ActiveModel {
            id: Set(request.id),
            billing_account_id: Set(account.id.clone()),
            project_id: Set(request.project_id),
            tenant_id: Set(request.tenant_id),
            api_key_id: Set(request.api_key_id),
            request_id: Set(request.request_id),
            spec: Set(spec),
            price: Set(json_value(&model)?),
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
            json!({"reserved_microunits": amount, "price_version": model.price_version}),
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
        crate::billing_projection::change_hold(
            &tx,
            &account.id,
            &row.api_key_id,
            -row.reserved_microunits,
        )
        .await?;
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
        let tx = self.db.begin().await?;
        let account = lock_account(&tx, project).await?;
        let row = reservation::Entity::find_by_id(id)
            .filter(reservation::Column::ProjectId.eq(project))
            .one(&tx)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let result = settle_locked(&tx, &account, row, request).await?;
        tx.commit().await?;
        Ok(result)
    }
}

/// Shared by Gateway settlement and reviewed recovery. Caller holds the account
/// lock and commits settlement, review decision and audit in the SAME transaction.
pub(crate) async fn settle_locked(
    tx: &DatabaseTransaction,
    account: &billing_account::Model,
    row: reservation::Model,
    request: SettleRequest,
) -> ServiceResult<reservation::Model> {
    let id = row.id.clone();
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
    let payload = json_value(&request)?;
    if row.state == "settled" {
        if row.completion.as_ref() != Some(&payload) {
            return Err(conflict("settlement payload changed"));
        }
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
    let monthly = crate::billing_projection::monthly(
        tx,
        &row.api_key_id,
        crate::billing_projection::month(now.date_naive()),
    )
    .await?;
    crate::billing_projection::change_hold(
        tx,
        &account.id,
        &row.api_key_id,
        -row.reserved_microunits,
    )
    .await?;
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
    .insert(tx)
    .await?;
    crate::billing_projection::change_spend(tx, monthly, amount).await?;
    let mut active: reservation::ActiveModel = row.into();
    active.state = Set("settled".into());
    active.settled_microunits = Set(Some(amount));
    active.completion = Set(Some(payload));
    active.updated_at = Set(now);
    let row = active.update(tx).await?;
    if amount > 0 {
        insert_balanced_entries(
            tx,
            account,
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
        tx,
        &account.id,
        "reservation.settled",
        &id,
        json!({"usage_event_id": usage_id, "settled_microunits": amount}),
    )
    .await?;
    Ok(row)
}
