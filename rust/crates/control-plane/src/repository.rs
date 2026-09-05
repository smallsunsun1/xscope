use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use chrono::{DateTime, Datelike, FixedOffset, TimeZone, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, DatabaseConnection,
    DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect, TransactionTrait,
};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use xscope_domain::{
    ApiKey, ApiKeyRequest, BillingAccount, BillingOrder, BillingSummary, CapturePaymentRequest,
    CreateInvoiceRequest, CreateOrderRequest, CreateRefundRequest, GatewayKey, GatewaySnapshot,
    Invoice, IssuedApiKey, KeyStatus, LedgerEntry, LedgerTransaction, MICROS_PER_MINOR_UNIT, Model,
    Money, Payment, PlatformUser, Project, ProjectBilling, Quote, ReconcileRequest,
    ReconciliationReport, Refund, TenantMembership, UsageEvent, ceil_minor_units, key_status,
    usage_cost_microunits, validate_money,
};
use xscope_entities::{
    api_key, billing_account, billing_order, invoice, ledger_entry, ledger_transaction, payment,
    platform_user, project, refund, tenant_membership, usage_event,
};

use crate::error::{ServiceError, ServiceResult};

#[derive(Clone)]
pub struct Repository {
    pub(crate) db: DatabaseConnection,
    pub(crate) model: Model,
}

impl Repository {
    #[must_use]
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            model: Model::default(),
        }
    }

    #[must_use]
    pub fn models(&self) -> Vec<Model> {
        vec![self.model.clone()]
    }

    pub async fn ping(&self) -> ServiceResult<()> {
        self.db.ping().await?;
        Ok(())
    }

    pub async fn list_projects(&self) -> ServiceResult<Vec<Project>> {
        project::Entity::find()
            .order_by_asc(project::Column::Id)
            .all(&self.db)
            .await
            .map(|rows| rows.into_iter().map(project_from_row).collect())
            .map_err(Into::into)
    }

    pub async fn create_project(&self, value: Project) -> ServiceResult<Project> {
        validate_project(&value)?;
        if project::Entity::find_by_id(&value.id)
            .one(&self.db)
            .await?
            .is_some()
        {
            return Err(ServiceError::Conflict("project already exists".to_owned()));
        }
        let transaction = self.db.begin().await?;
        let now = Utc::now().fixed_offset();
        project::ActiveModel {
            id: Set(value.id.clone()),
            tenant_id: Set(value.tenant_id.clone()),
            name: Set(value.name.clone()),
            created_at: Set(now),
        }
        .insert(&transaction)
        .await
        .map_err(conflict_or_database)?;
        create_billing_account(&transaction, &value, now).await?;
        transaction.commit().await?;
        Ok(value)
    }

    pub async fn backfill_billing_accounts(&self) -> ServiceResult<()> {
        for row in project::Entity::find().all(&self.db).await? {
            if billing_account::Entity::find()
                .filter(billing_account::Column::ProjectId.eq(&row.id))
                .one(&self.db)
                .await?
                .is_none()
            {
                let project = project_from_row(row);
                let now = Utc::now().fixed_offset();
                create_billing_account(&self.db, &project, now)
                    .await
                    .map_err(conflict_or_database)?;
            }
        }
        Ok(())
    }

    pub async fn list_api_keys(&self) -> ServiceResult<Vec<ApiKey>> {
        let now = Utc::now();
        api_key::Entity::find()
            .order_by_asc(api_key::Column::Id)
            .all(&self.db)
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|row| api_key_from_row(row, now))
                    .collect()
            })
            .map_err(Into::into)
    }

    pub async fn issue_api_key(&self, request: ApiKeyRequest) -> ServiceResult<IssuedApiKey> {
        let request = request.with_defaults();
        let now = Utc::now();
        request.validate(now)?;
        if request
            .allowed_models
            .iter()
            .any(|model| model != &self.model.id)
        {
            return Err(ServiceError::Invalid("allowed model not found".to_owned()));
        }
        let project = project::Entity::find_by_id(&request.project_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| ServiceError::Invalid("project not found".to_owned()))?;
        if project.tenant_id != request.tenant_id {
            return Err(ServiceError::Invalid(
                "project does not belong to tenant".to_owned(),
            ));
        }
        if api_key::Entity::find_by_id(&request.id)
            .one(&self.db)
            .await?
            .is_some()
        {
            return Err(ServiceError::Conflict("API key already exists".to_owned()));
        }
        let mut random = [0_u8; 24];
        getrandom::fill(&mut random).map_err(|error| ServiceError::Internal(error.to_string()))?;
        let secret = format!(
            "xs_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random)
        );
        self.store_api_key(request, &secret, now).await
    }

    pub async fn ensure_bootstrap_api_key(
        &self,
        project_value: Project,
        request: ApiKeyRequest,
        secret: &str,
    ) -> ServiceResult<()> {
        if project::Entity::find_by_id(&project_value.id)
            .one(&self.db)
            .await?
            .is_none()
        {
            self.create_project(project_value).await?;
        }
        if api_key::Entity::find_by_id(&request.id)
            .one(&self.db)
            .await?
            .is_none()
        {
            self.store_api_key(request.with_defaults(), secret, Utc::now())
                .await?;
        }
        Ok(())
    }

    async fn store_api_key(
        &self,
        request: ApiKeyRequest,
        secret: &str,
        now: DateTime<Utc>,
    ) -> ServiceResult<IssuedApiKey> {
        request.validate(now)?;
        let digest = Sha256::digest(secret.as_bytes()).to_vec();
        api_key::ActiveModel {
            id: Set(request.id.clone()),
            tenant_id: Set(request.tenant_id.clone()),
            project_id: Set(request.project_id.clone()),
            name: Set(request.name.clone()),
            secret_hash: Set(digest),
            scopes: Set(request.scopes.clone()),
            allowed_models: Set(request.allowed_models.clone()),
            expires_at: Set(request.expires_at.map(|value| value.fixed_offset())),
            rate_limit_rpm: Set(request.rate_limit_rpm),
            rate_limit_tpm: Set(request.rate_limit_tpm),
            monthly_budget_amount: Set(request.monthly_budget.amount),
            currency: Set(request.monthly_budget.currency.clone()),
            created_at: Set(now.fixed_offset()),
            revoked_at: Set(None),
        }
        .insert(&self.db)
        .await
        .map_err(conflict_or_database)?;
        Ok(IssuedApiKey {
            api_key: ApiKey {
                id: request.id,
                tenant_id: request.tenant_id,
                project_id: request.project_id,
                name: request.name,
                scopes: request.scopes,
                allowed_models: request.allowed_models,
                expires_at: request.expires_at,
                rate_limit_rpm: request.rate_limit_rpm,
                rate_limit_tpm: request.rate_limit_tpm,
                monthly_budget: request.monthly_budget,
                created_at: now,
                revoked_at: None,
                status: KeyStatus::Active,
            },
            secret: secret.to_owned(),
        })
    }

    pub async fn revoke_api_key(&self, id: &str) -> ServiceResult<()> {
        let row = api_key::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .filter(|row| row.revoked_at.is_none())
            .ok_or(ServiceError::NotFound)?;
        let mut active: api_key::ActiveModel = row.into();
        active.revoked_at = Set(Some(Utc::now().fixed_offset()));
        active.update(&self.db).await?;
        Ok(())
    }

    pub fn quote(&self, model_id: &str, input: i64, output: i64) -> ServiceResult<Quote> {
        if model_id != self.model.id {
            return Err(ServiceError::Invalid("model not found".to_owned()));
        }
        let microunits = usage_cost_microunits(&self.model, input, output)?;
        Ok(Quote {
            model_id: self.model.id.clone(),
            price_version: self.model.price_version.clone(),
            maximum: Money::cny(ceil_minor_units(microunits)),
        })
    }

    pub async fn gateway_snapshot(&self) -> ServiceResult<GatewaySnapshot> {
        let now = Utc::now();
        let period_start = Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .ok_or_else(|| ServiceError::Internal("could not build billing period".to_owned()))?;
        // Batch the initialized projections: steady-state snapshots scale with
        // keys/accounts, not historical usage and not one DB round-trip per key.
        let spend_cache: HashMap<_, _> = xscope_entities::billing_month_spend::Entity::find()
            .filter(
                xscope_entities::billing_month_spend::Column::Month.eq(period_start.date_naive()),
            )
            .all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.api_key_id, r.spent_microunits))
            .collect();
        let balances: HashMap<_, _> = xscope_entities::billing_balance::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|r| (r.billing_account_id, r.balance_microunits))
            .collect();
        let mut account_cache: HashMap<_, _> = billing_account::Entity::find()
            .all(&self.db)
            .await?
            .into_iter()
            .filter_map(|r| {
                balances
                    .get(&r.id)
                    .map(|balance| (r.project_id, (r.enforce_balance, *balance)))
            })
            .collect();
        let mut keys = Vec::new();
        for row in api_key::Entity::find()
            .order_by_asc(api_key::Column::Id)
            .all(&self.db)
            .await?
        {
            if key_status(
                row.revoked_at.map(|value| value.with_timezone(&Utc)),
                row.expires_at.map(|value| value.with_timezone(&Utc)),
                now,
            ) != KeyStatus::Active
            {
                continue;
            }
            let (balance_enforced, balance_microunits) =
                if let Some(value) = account_cache.get(&row.project_id) {
                    *value
                } else {
                    let value = self.project_balance(&row.project_id).await?;
                    account_cache.insert(row.project_id.clone(), value);
                    value
                };
            let spent = if let Some(value) = spend_cache.get(&row.id) {
                *value
            } else {
                self.projected_spend(&row.project_id, &row.id, period_start.date_naive())
                    .await?
            };
            keys.push(GatewayKey {
                id: row.id.clone(),
                tenant_id: row.tenant_id,
                project_id: row.project_id,
                secret_hash: base64::engine::general_purpose::STANDARD.encode(row.secret_hash),
                scopes: row.scopes,
                allowed_models: row.allowed_models,
                expires_at: row.expires_at.map(|value| value.with_timezone(&Utc)),
                rate_limit_rpm: row.rate_limit_rpm,
                rate_limit_tpm: row.rate_limit_tpm,
                monthly_budget: Money {
                    currency: row.currency.clone(),
                    amount: row.monthly_budget_amount,
                },
                current_month_spend: Money {
                    currency: row.currency.clone(),
                    amount: ceil_minor_units(spent),
                },
                balance_enforced,
                available_balance: Money {
                    currency: row.currency,
                    amount: floor_minor_units(balance_microunits),
                },
            });
        }
        Ok(GatewaySnapshot {
            generated_at: now,
            keys,
            route_policies: self.list_route_policies().await?,
        })
    }

    pub async fn get_project(&self, id: &str) -> ServiceResult<Project> {
        project::Entity::find_by_id(id)
            .one(&self.db)
            .await?
            .map(project_from_row)
            .ok_or(ServiceError::NotFound)
    }

    pub async fn list_route_policies(&self) -> ServiceResult<Vec<xscope_domain::RoutePolicy>> {
        let projects: HashMap<_, _> = self
            .list_projects()
            .await?
            .into_iter()
            .map(|p| (p.id, p.tenant_id))
            .collect();
        xscope_entities::route_policy::Entity::find()
            .order_by_asc(xscope_entities::route_policy::Column::ProjectId)
            .order_by_asc(xscope_entities::route_policy::Column::Model)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| {
                Ok(xscope_domain::RoutePolicy {
                    tenant_id: projects
                        .get(&row.project_id)
                        .cloned()
                        .ok_or(ServiceError::NotFound)?,
                    project_id: row.project_id,
                    model: row.model,
                    revision: row.revision,
                    spec: serde_json::from_value(row.spec)
                        .map_err(|e| ServiceError::Internal(e.to_string()))?,
                })
            })
            .collect()
    }

    pub async fn put_route_policy(
        &self,
        project: &Project,
        model: &str,
        request: xscope_domain::PutRoutePolicy,
        pools: &[xscope_domain::RoutePool],
    ) -> ServiceResult<xscope_domain::RoutePolicy> {
        use sea_orm::sea_query::Expr;
        use xscope_entities::route_policy::{ActiveModel, Column, Entity};
        request
            .spec
            .validate_pools(model, pools)
            .map_err(ServiceError::Invalid)?;
        if model != self.model.id || !(0..i64::MAX).contains(&request.expected_revision) {
            return Err(ServiceError::Invalid(
                "unknown model or invalid expected_revision".into(),
            ));
        }
        let revision = request.expected_revision + 1;
        let spec = serde_json::to_value(&request.spec)
            .map_err(|e| ServiceError::Internal(e.to_string()))?;
        let now = Utc::now().fixed_offset();
        if request.expected_revision == 0 {
            ActiveModel {
                project_id: Set(project.id.clone()),
                model: Set(model.to_owned()),
                revision: Set(revision),
                spec: Set(spec),
                updated_at: Set(now),
            }
            .insert(&self.db)
            .await
            .map_err(conflict_or_database)?;
        } else {
            // Atomic compare-and-swap; rollback also advances revision.
            let result = Entity::update_many()
                .col_expr(Column::Revision, Expr::value(revision))
                .col_expr(Column::Spec, Expr::value(spec))
                .col_expr(Column::UpdatedAt, Expr::value(now))
                .filter(Column::ProjectId.eq(&project.id))
                .filter(Column::Model.eq(model))
                .filter(Column::Revision.eq(request.expected_revision))
                .exec(&self.db)
                .await?;
            if result.rows_affected != 1 {
                return Err(ServiceError::Conflict(
                    "route policy revision changed; reload before saving".into(),
                ));
            }
        }
        Ok(xscope_domain::RoutePolicy {
            tenant_id: project.tenant_id.clone(),
            project_id: project.id.clone(),
            model: model.to_owned(),
            revision,
            spec: request.spec,
        })
    }

    pub async fn record_usage(&self, event: UsageEvent) -> ServiceResult<bool> {
        validate_usage(&event)?;
        let key = api_key::Entity::find_by_id(&event.api_key_id)
            .one(&self.db)
            .await?
            .ok_or_else(|| ServiceError::Invalid("usage event API key is invalid".to_owned()))?;
        if key.project_id != event.project_id || key.tenant_id != event.tenant_id {
            return Err(ServiceError::Invalid(
                "usage event API key boundary is invalid".to_owned(),
            ));
        }
        let chargeable = is_chargeable_usage(&event);
        if chargeable
            && (event.model_id != self.model.id || event.price_version != self.model.price_version)
        {
            return Err(ServiceError::Invalid(
                "usage event price version is not current".to_owned(),
            ));
        }
        let cost_microunits = if chargeable {
            usage_cost_microunits(&self.model, event.input_tokens, event.output_tokens)?
        } else {
            0
        };
        let account = self.ensure_account_for_project(&event.project_id).await?;
        let transaction = self.db.begin().await?;
        crate::billing::lock_account(&transaction, &event.project_id).await?;
        if xscope_entities::billing_reservation::Entity::find()
            .filter(xscope_entities::billing_reservation::Column::ProjectId.eq(&event.project_id))
            .filter(xscope_entities::billing_reservation::Column::RequestId.eq(&event.request_id))
            .one(&transaction)
            .await?
            .is_some()
        {
            return Err(ServiceError::Conflict(
                "reserved requests must use the reservation settlement endpoint".into(),
            ));
        }
        let monthly = crate::billing_projection::monthly(
            &transaction,
            &event.api_key_id,
            crate::billing_projection::month(event.occurred_at.date_naive()),
        )
        .await?;
        let inserted = usage_event::Entity::insert(usage_event::ActiveModel {
            event_id: Set(event.event_id.clone()),
            request_id: Set(event.request_id),
            occurred_at: Set(event.occurred_at.fixed_offset()),
            tenant_id: Set(event.tenant_id.clone()),
            project_id: Set(event.project_id),
            api_key_id: Set(event.api_key_id),
            model_id: Set(event.model_id),
            model_revision: Set(event.model_revision),
            endpoint_id: Set(event.endpoint_id),
            region: Set(event.region),
            price_version: Set(event.price_version),
            input_tokens: Set(event.input_tokens),
            output_tokens: Set(event.output_tokens),
            cached_input_tokens: Set(event.cached_input_tokens),
            latency_ms: Set(event.latency_ms),
            status: Set(event.status),
            cost_amount: Set(ceil_minor_units(cost_microunits)),
            cost_microunits: Set(cost_microunits),
            currency: Set("CNY".to_owned()),
            received_at: Set(Utc::now().fixed_offset()),
        })
        .on_conflict(
            OnConflict::column(usage_event::Column::EventId)
                .do_nothing()
                .to_owned(),
        )
        .exec_without_returning(&transaction)
        .await?;
        if inserted == 0 {
            transaction.rollback().await?;
            return Ok(false);
        }
        crate::billing_projection::change_spend(&transaction, monthly, cost_microunits).await?;
        if cost_microunits > 0 {
            insert_balanced_entries(
                &transaction,
                &account,
                &event.tenant_id,
                "usage",
                "usage_event",
                &event.event_id,
                &format!("usage:{}", event.event_id),
                "Model inference usage",
                -cost_microunits,
                "usage_revenue",
            )
            .await?;
        }
        transaction.commit().await?;
        Ok(true)
    }

    pub async fn billing_summary(
        &self,
        project_id: Option<&str>,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> ServiceResult<BillingSummary> {
        if end <= start {
            return Err(ServiceError::Invalid("to must be after from".to_owned()));
        }
        let mut query = usage_event::Entity::find()
            .filter(usage_event::Column::OccurredAt.gte(start.fixed_offset()))
            .filter(usage_event::Column::OccurredAt.lt(end.fixed_offset()));
        if let Some(project_id) = project_id.filter(|value| !value.is_empty()) {
            query = query.filter(usage_event::Column::ProjectId.eq(project_id));
        }
        let mut projects = BTreeMap::<String, ProjectBillingAccumulator>::new();
        for event in query.all(&self.db).await? {
            let item = projects.entry(event.project_id).or_default();
            item.requests += 1;
            item.input_tokens += event.input_tokens;
            item.output_tokens += event.output_tokens;
            item.cost_microunits += event.cost_microunits;
        }
        let mut summary = BillingSummary {
            period_start: start,
            period_end: end,
            requests: 0,
            input_tokens: 0,
            output_tokens: 0,
            total: Money::cny(0),
            projects: Vec::with_capacity(projects.len()),
        };
        for (project_id, item) in projects {
            let project = ProjectBilling {
                project_id,
                requests: item.requests,
                input_tokens: item.input_tokens,
                output_tokens: item.output_tokens,
                cost: Money::cny(ceil_minor_units(item.cost_microunits)),
            };
            summary.requests += project.requests;
            summary.input_tokens += project.input_tokens;
            summary.output_tokens += project.output_tokens;
            summary.total.amount += project.cost.amount;
            summary.projects.push(project);
        }
        Ok(summary)
    }

    pub async fn billing_account(&self, project_id: &str) -> ServiceResult<BillingAccount> {
        let row = self.ensure_account_for_project(project_id).await?;
        let (_, balance_microunits) = self.project_balance(project_id).await?;
        Ok(billing_account_from_row(row, balance_microunits))
    }

    pub async fn billing_position(&self, project_id: &str) -> ServiceResult<serde_json::Value> {
        let account = self.ensure_account_for_project(project_id).await?;
        let row = match xscope_entities::billing_balance::Entity::find_by_id(&account.id)
            .one(&self.db)
            .await?
        {
            Some(row) => row, // Warm console reads never acquire the money lock.
            None => {
                let tx = self.db.begin().await?;
                crate::billing::lock_account(&tx, project_id).await?;
                let row = crate::billing_projection::account(&tx, &account.id).await?;
                tx.commit().await?;
                row
            }
        };
        let available = row
            .balance_microunits
            .checked_sub(row.held_microunits)
            .ok_or_else(|| ServiceError::Internal("available balance overflow".into()))?;
        // Decimal strings preserve i64 precision across JavaScript clients.
        Ok(
            serde_json::json!({"project_id": project_id, "currency": account.currency,
            "balance_microunits": row.balance_microunits.to_string(),
            "held_microunits": row.held_microunits.to_string(),
            "available_microunits": available.to_string(), "updated_at": row.updated_at}),
        )
    }

    pub async fn update_balance_policy(
        &self,
        project_id: &str,
        enforce_balance: bool,
    ) -> ServiceResult<BillingAccount> {
        let row = self.ensure_account_for_project(project_id).await?;
        let mut active: billing_account::ActiveModel = row.into();
        active.enforce_balance = Set(enforce_balance);
        active.updated_at = Set(Utc::now().fixed_offset());
        active.update(&self.db).await?;
        self.billing_account(project_id).await
    }

    pub async fn create_order(&self, request: CreateOrderRequest) -> ServiceResult<BillingOrder> {
        validate_money(&request.amount)?;
        if request.amount.amount <= 0 {
            return Err(ServiceError::Invalid(
                "order amount must be positive".to_owned(),
            ));
        }
        let project = project::Entity::find_by_id(&request.project_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if project.tenant_id != request.tenant_id {
            return Err(ServiceError::Forbidden);
        }
        let now = Utc::now().fixed_offset();
        let row = billing_order::ActiveModel {
            id: Set(request.id),
            tenant_id: Set(request.tenant_id),
            project_id: Set(request.project_id),
            kind: Set("top_up".to_owned()),
            amount: Set(request.amount.amount),
            currency: Set(request.amount.currency),
            status: Set("pending".to_owned()),
            description: Set(request.description),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&self.db)
        .await
        .map_err(conflict_or_database)?;
        Ok(order_from_row(row))
    }

    pub async fn list_orders(&self) -> ServiceResult<Vec<BillingOrder>> {
        billing_order::Entity::find()
            .order_by_desc(billing_order::Column::CreatedAt)
            .all(&self.db)
            .await
            .map(|rows| rows.into_iter().map(order_from_row).collect())
            .map_err(Into::into)
    }

    pub async fn capture_payment(
        &self,
        order_id: &str,
        request: CapturePaymentRequest,
        idempotency_key: &str,
    ) -> ServiceResult<Payment> {
        if request.provider.trim().is_empty() || request.provider_reference.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "provider and provider_reference are required".to_owned(),
            ));
        }
        if let Some(existing) = payment::Entity::find_by_id(&request.id)
            .one(&self.db)
            .await?
        {
            if existing.order_id == order_id
                && existing.provider == request.provider
                && existing.provider_reference == request.provider_reference
            {
                return Ok(payment_from_row(existing));
            }
            return Err(ServiceError::Conflict(
                "payment id already belongs to a different request".to_owned(),
            ));
        }
        let transaction = self.db.begin().await?;
        let order = billing_order::Entity::find_by_id(order_id)
            .lock_exclusive()
            .one(&transaction)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if order.status != "pending" {
            return Err(ServiceError::Conflict(
                "only pending orders can be paid".to_owned(),
            ));
        }
        let account = ensure_account_for_project(&transaction, &order.project_id).await?;
        let now = Utc::now().fixed_offset();
        let payment = payment::ActiveModel {
            id: Set(request.id),
            order_id: Set(order.id.clone()),
            provider: Set(request.provider),
            provider_reference: Set(request.provider_reference),
            amount: Set(order.amount),
            currency: Set(order.currency.clone()),
            status: Set("succeeded".to_owned()),
            paid_at: Set(Some(now)),
            created_at: Set(now),
        }
        .insert(&transaction)
        .await
        .map_err(conflict_or_database)?;
        let mut active_order: billing_order::ActiveModel = order.clone().into();
        active_order.status = Set("paid".to_owned());
        active_order.updated_at = Set(now);
        active_order.update(&transaction).await?;
        let amount_microunits = order
            .amount
            .checked_mul(MICROS_PER_MINOR_UNIT)
            .ok_or_else(|| ServiceError::Invalid("payment amount is too large".to_owned()))?;
        insert_balanced_entries(
            &transaction,
            &account,
            &order.tenant_id,
            "top_up",
            "payment",
            &payment.id,
            idempotency_key,
            "Wallet top-up",
            amount_microunits,
            "cash_clearing",
        )
        .await?;
        transaction.commit().await?;
        Ok(payment_from_row(payment))
    }

    pub async fn list_payments(&self) -> ServiceResult<Vec<Payment>> {
        payment::Entity::find()
            .order_by_desc(payment::Column::CreatedAt)
            .all(&self.db)
            .await
            .map(|rows| rows.into_iter().map(payment_from_row).collect())
            .map_err(Into::into)
    }

    pub async fn create_refund(
        &self,
        request: CreateRefundRequest,
        idempotency_key: &str,
    ) -> ServiceResult<Refund> {
        validate_money(&request.amount)?;
        if request.amount.amount <= 0 || request.reason.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "refund amount and reason are required".to_owned(),
            ));
        }
        if let Some(existing) = refund::Entity::find_by_id(&request.id)
            .one(&self.db)
            .await?
        {
            if existing.payment_id == request.payment_id
                && existing.amount == request.amount.amount
                && existing.currency == request.amount.currency
                && existing.reason == request.reason
            {
                return Ok(refund_from_row(existing));
            }
            return Err(ServiceError::Conflict(
                "refund id already belongs to a different request".to_owned(),
            ));
        }
        let transaction = self.db.begin().await?;
        let payment = payment::Entity::find_by_id(&request.payment_id)
            .lock_exclusive()
            .one(&transaction)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if payment.status != "succeeded"
            || payment.currency != request.amount.currency
            || request.amount.amount > payment.amount
        {
            return Err(ServiceError::Invalid(
                "refund exceeds the captured payment".to_owned(),
            ));
        }
        let order = billing_order::Entity::find_by_id(&payment.order_id)
            .one(&transaction)
            .await?
            .ok_or(ServiceError::NotFound)?;
        let already_refunded: i64 = refund::Entity::find()
            .filter(refund::Column::PaymentId.eq(&payment.id))
            .filter(refund::Column::Status.eq("succeeded"))
            .all(&transaction)
            .await?
            .into_iter()
            .map(|item| item.amount)
            .sum();
        if already_refunded.saturating_add(request.amount.amount) > payment.amount {
            return Err(ServiceError::Invalid(
                "cumulative refunds exceed the captured payment".to_owned(),
            ));
        }
        let account = ensure_account_for_project(&transaction, &order.project_id).await?;
        let now = Utc::now().fixed_offset();
        let row = refund::ActiveModel {
            id: Set(request.id),
            payment_id: Set(request.payment_id),
            amount: Set(request.amount.amount),
            currency: Set(request.amount.currency),
            reason: Set(request.reason),
            status: Set("succeeded".to_owned()),
            provider_reference: Set(None),
            created_at: Set(now),
            completed_at: Set(Some(now)),
        }
        .insert(&transaction)
        .await
        .map_err(conflict_or_database)?;
        let amount_microunits = row
            .amount
            .checked_mul(MICROS_PER_MINOR_UNIT)
            .ok_or_else(|| ServiceError::Invalid("refund amount is too large".to_owned()))?;
        insert_balanced_entries(
            &transaction,
            &account,
            &order.tenant_id,
            "refund",
            "refund",
            &row.id,
            idempotency_key,
            "Payment refund",
            -amount_microunits,
            "cash_clearing",
        )
        .await?;
        transaction.commit().await?;
        Ok(refund_from_row(row))
    }

    pub async fn list_refunds(&self) -> ServiceResult<Vec<Refund>> {
        refund::Entity::find()
            .order_by_desc(refund::Column::CreatedAt)
            .all(&self.db)
            .await
            .map(|rows| rows.into_iter().map(refund_from_row).collect())
            .map_err(Into::into)
    }

    pub async fn list_ledger(&self) -> ServiceResult<Vec<LedgerTransaction>> {
        let mut result = Vec::new();
        for transaction in ledger_transaction::Entity::find()
            .order_by_desc(ledger_transaction::Column::CreatedAt)
            .all(&self.db)
            .await?
        {
            let entries = ledger_entry::Entity::find()
                .filter(ledger_entry::Column::TransactionId.eq(&transaction.id))
                .order_by_asc(ledger_entry::Column::Id)
                .all(&self.db)
                .await?
                .into_iter()
                .map(ledger_entry_from_row)
                .collect();
            result.push(ledger_transaction_from_row(transaction, entries));
        }
        Ok(result)
    }

    pub async fn create_invoice(&self, request: CreateInvoiceRequest) -> ServiceResult<Invoice> {
        if request.period_end <= request.period_start || request.title.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "invoice period and title are invalid".to_owned(),
            ));
        }
        let project = project::Entity::find_by_id(&request.project_id)
            .one(&self.db)
            .await?
            .ok_or(ServiceError::NotFound)?;
        if project.tenant_id != request.tenant_id {
            return Err(ServiceError::Forbidden);
        }
        let summary = self
            .billing_summary(
                Some(&request.project_id),
                request.period_start,
                request.period_end,
            )
            .await?;
        let row = invoice::ActiveModel {
            id: Set(request.id),
            tenant_id: Set(request.tenant_id),
            project_id: Set(request.project_id),
            period_start: Set(request.period_start.fixed_offset()),
            period_end: Set(request.period_end.fixed_offset()),
            amount: Set(summary.total.amount),
            currency: Set(summary.total.currency),
            status: Set("issued".to_owned()),
            title: Set(request.title),
            issued_at: Set(Utc::now().fixed_offset()),
        }
        .insert(&self.db)
        .await
        .map_err(conflict_or_database)?;
        Ok(invoice_from_row(row))
    }

    pub async fn list_invoices(&self) -> ServiceResult<Vec<Invoice>> {
        invoice::Entity::find()
            .order_by_desc(invoice::Column::IssuedAt)
            .all(&self.db)
            .await
            .map(|rows| rows.into_iter().map(invoice_from_row).collect())
            .map_err(Into::into)
    }

    pub async fn reconcile(
        &self,
        request: ReconcileRequest,
    ) -> ServiceResult<ReconciliationReport> {
        if request.tenant_id.trim().is_empty() || request.provider.trim().is_empty() {
            return Err(ServiceError::Invalid(
                "tenant_id and provider are required".to_owned(),
            ));
        }
        let order_ids = billing_order::Entity::find()
            .filter(billing_order::Column::TenantId.eq(&request.tenant_id))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|order| order.id)
            .collect::<Vec<_>>();
        let mut platform = BTreeMap::<String, i64>::new();
        for row in payment::Entity::find()
            .filter(payment::Column::Provider.eq(&request.provider))
            .filter(payment::Column::Status.eq("succeeded"))
            .filter(payment::Column::OrderId.is_in(order_ids))
            .all(&self.db)
            .await?
        {
            platform.insert(row.provider_reference, row.amount);
        }
        let mut provider = BTreeMap::<String, i64>::new();
        for settlement in request.settlements {
            validate_money(&settlement.amount)?;
            provider.insert(settlement.provider_reference, settlement.amount.amount);
        }
        let platform_only = platform
            .keys()
            .filter(|reference| !provider.contains_key(*reference))
            .cloned()
            .collect();
        let provider_only = provider
            .keys()
            .filter(|reference| !platform.contains_key(*reference))
            .cloned()
            .collect();
        let mut matched = 0_i64;
        let mut amount_mismatches = BTreeMap::new();
        for (reference, platform_amount) in &platform {
            if let Some(provider_amount) = provider.get(reference) {
                if provider_amount == platform_amount {
                    matched += 1;
                } else {
                    amount_mismatches.insert(
                        reference.clone(),
                        Money::cny(provider_amount - platform_amount),
                    );
                }
            }
        }
        Ok(ReconciliationReport {
            generated_at: Utc::now(),
            provider: request.provider,
            matched,
            platform_only,
            provider_only,
            amount_mismatches,
        })
    }

    pub async fn list_users(&self) -> ServiceResult<Vec<PlatformUser>> {
        let mut result = Vec::new();
        for row in platform_user::Entity::find()
            .order_by_asc(platform_user::Column::Username)
            .all(&self.db)
            .await?
        {
            let memberships = tenant_membership::Entity::find()
                .filter(tenant_membership::Column::UserId.eq(&row.id))
                .order_by_asc(tenant_membership::Column::TenantId)
                .all(&self.db)
                .await?
                .into_iter()
                .map(|membership| TenantMembership {
                    tenant_id: membership.tenant_id,
                    role: membership.role,
                })
                .collect();
            result.push(PlatformUser {
                id: row.id,
                external_subject: row.external_subject,
                username: row.username,
                email: row.email,
                status: row.status,
                last_login_at: row.last_login_at.with_timezone(&Utc),
                created_at: row.created_at.with_timezone(&Utc),
                updated_at: row.updated_at.with_timezone(&Utc),
                memberships,
            });
        }
        Ok(result)
    }

    pub async fn sync_user(
        &self,
        external_subject: &str,
        username: &str,
        email: &str,
        default_tenant_id: &str,
        default_role: Option<&str>,
    ) -> ServiceResult<PlatformUser> {
        if external_subject.is_empty() {
            return Err(ServiceError::Unauthorized);
        }
        let transaction = self.db.begin().await?;
        let now = Utc::now().fixed_offset();
        let existing = platform_user::Entity::find()
            .filter(platform_user::Column::ExternalSubject.eq(external_subject))
            .one(&transaction)
            .await?;
        let row = if let Some(existing) = existing {
            let mut active: platform_user::ActiveModel = existing.into();
            active.username = Set(username.to_owned());
            active.email = Set(email.to_owned());
            active.last_login_at = Set(now);
            active.updated_at = Set(now);
            active.update(&transaction).await?
        } else {
            platform_user::ActiveModel {
                id: Set(format!("user-{}", stable_id(external_subject))),
                external_subject: Set(external_subject.to_owned()),
                username: Set(username.to_owned()),
                email: Set(email.to_owned()),
                status: Set("active".to_owned()),
                last_login_at: Set(now),
                created_at: Set(now),
                updated_at: Set(now),
            }
            .insert(&transaction)
            .await
            .map_err(conflict_or_database)?
        };

        if let Some(default_role) = default_role
            && tenant_membership::Entity::find()
                .filter(tenant_membership::Column::UserId.eq(&row.id))
                .filter(tenant_membership::Column::TenantId.eq(default_tenant_id))
                .one(&transaction)
                .await?
                .is_none()
        {
            tenant_membership::ActiveModel {
                id: Set(format!(
                    "membership-{}",
                    stable_id(&format!("{}:{default_tenant_id}", row.id))
                )),
                user_id: Set(row.id.clone()),
                tenant_id: Set(default_tenant_id.to_owned()),
                role: Set(default_role.to_owned()),
                created_at: Set(now),
            }
            .insert(&transaction)
            .await
            .map_err(conflict_or_database)?;
        }
        let memberships = tenant_membership::Entity::find()
            .filter(tenant_membership::Column::UserId.eq(&row.id))
            .order_by_asc(tenant_membership::Column::TenantId)
            .all(&transaction)
            .await?
            .into_iter()
            .map(|membership| TenantMembership {
                tenant_id: membership.tenant_id,
                role: membership.role,
            })
            .collect();
        let user = PlatformUser {
            id: row.id,
            external_subject: row.external_subject,
            username: row.username,
            email: row.email,
            status: row.status,
            last_login_at: row.last_login_at.with_timezone(&Utc),
            created_at: row.created_at.with_timezone(&Utc),
            updated_at: row.updated_at.with_timezone(&Utc),
            memberships,
        };
        transaction.commit().await?;
        Ok(user)
    }

    pub async fn update_membership(
        &self,
        user_id: &str,
        tenant_id: &str,
        role: &str,
    ) -> ServiceResult<PlatformUser> {
        if !matches!(role, "owner" | "member") {
            return Err(ServiceError::Invalid(
                "membership role must be owner or member".to_owned(),
            ));
        }
        if platform_user::Entity::find_by_id(user_id)
            .one(&self.db)
            .await?
            .is_none()
        {
            return Err(ServiceError::NotFound);
        }
        let now = Utc::now().fixed_offset();
        if let Some(existing) = tenant_membership::Entity::find()
            .filter(tenant_membership::Column::UserId.eq(user_id))
            .filter(tenant_membership::Column::TenantId.eq(tenant_id))
            .one(&self.db)
            .await?
        {
            if existing.role == "owner"
                && role != "owner"
                && tenant_membership::Entity::find()
                    .filter(tenant_membership::Column::TenantId.eq(tenant_id))
                    .filter(tenant_membership::Column::Role.eq("owner"))
                    .all(&self.db)
                    .await?
                    .len()
                    <= 1
            {
                return Err(ServiceError::Conflict(
                    "a tenant must keep at least one owner".to_owned(),
                ));
            }
            let mut active: tenant_membership::ActiveModel = existing.into();
            active.role = Set(role.to_owned());
            active.update(&self.db).await?;
        } else {
            tenant_membership::ActiveModel {
                id: Set(format!(
                    "membership-{}",
                    stable_id(&format!("{user_id}:{tenant_id}"))
                )),
                user_id: Set(user_id.to_owned()),
                tenant_id: Set(tenant_id.to_owned()),
                role: Set(role.to_owned()),
                created_at: Set(now),
            }
            .insert(&self.db)
            .await
            .map_err(conflict_or_database)?;
        }
        self.list_users()
            .await?
            .into_iter()
            .find(|user| user.id == user_id)
            .ok_or(ServiceError::NotFound)
    }

    async fn ensure_account_for_project(
        &self,
        project_id: &str,
    ) -> ServiceResult<billing_account::Model> {
        ensure_account_for_project(&self.db, project_id).await
    }

    async fn project_balance(&self, project_id: &str) -> ServiceResult<(bool, i64)> {
        let account = self.ensure_account_for_project(project_id).await?;
        if let Some(row) = xscope_entities::billing_balance::Entity::find_by_id(&account.id)
            .one(&self.db)
            .await?
        {
            return Ok((account.enforce_balance, row.balance_microunits));
        }
        let tx = self.db.begin().await?;
        crate::billing::lock_account(&tx, project_id).await?;
        let balance = crate::billing_projection::account(&tx, &account.id)
            .await?
            .balance_microunits;
        tx.commit().await?;
        Ok((account.enforce_balance, balance))
    }
}

async fn ensure_account_for_project<C>(
    connection: &C,
    project_id: &str,
) -> ServiceResult<billing_account::Model>
where
    C: ConnectionTrait,
{
    if let Some(account) = billing_account::Entity::find()
        .filter(billing_account::Column::ProjectId.eq(project_id))
        .one(connection)
        .await?
    {
        return Ok(account);
    }
    let project = project::Entity::find_by_id(project_id)
        .one(connection)
        .await?
        .ok_or(ServiceError::NotFound)?;
    create_billing_account(
        connection,
        &project_from_row(project),
        Utc::now().fixed_offset(),
    )
    .await
    .map_err(conflict_or_database)
}

async fn create_billing_account<C>(
    connection: &C,
    project: &Project,
    now: DateTime<FixedOffset>,
) -> Result<billing_account::Model, sea_orm::DbErr>
where
    C: ConnectionTrait,
{
    billing_account::ActiveModel {
        id: Set(format!("account-{}", project.id)),
        tenant_id: Set(project.tenant_id.clone()),
        project_id: Set(project.id.clone()),
        currency: Set("CNY".to_owned()),
        enforce_balance: Set(false),
        status: Set("active".to_owned()),
        created_at: Set(now),
        updated_at: Set(now),
    }
    .insert(connection)
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_balanced_entries(
    transaction: &DatabaseTransaction,
    account: &billing_account::Model,
    tenant_id: &str,
    kind: &str,
    reference_type: &str,
    reference_id: &str,
    idempotency_key: &str,
    description: &str,
    customer_delta_microunits: i64,
    counter_account: &str,
) -> ServiceResult<()> {
    if customer_delta_microunits == i64::MIN {
        return Err(ServiceError::Invalid(
            "ledger amount is too large".to_owned(),
        ));
    }
    let locked = crate::billing::lock_account(transaction, &account.project_id).await?;
    if customer_delta_microunits < 0 && locked.enforce_balance {
        let (balance, held) = crate::billing::balance_and_held(transaction, &locked.id).await?;
        if balance
            .checked_sub(held)
            .and_then(|v| v.checked_add(customer_delta_microunits))
            .is_none_or(|v| v < 0)
        {
            return Err(ServiceError::InsufficientFunds(
                "debit would consume unavailable or reserved balance".into(),
            ));
        }
    }
    crate::billing_projection::change_balance(transaction, &account.id, customer_delta_microunits)
        .await?;
    let transaction_id = format!("txn-{}", Uuid::now_v7());
    let now = Utc::now().fixed_offset();
    ledger_transaction::ActiveModel {
        id: Set(transaction_id.clone()),
        tenant_id: Set(tenant_id.to_owned()),
        kind: Set(kind.to_owned()),
        reference_type: Set(reference_type.to_owned()),
        reference_id: Set(reference_id.to_owned()),
        idempotency_key: Set(idempotency_key.to_owned()),
        currency: Set(account.currency.clone()),
        description: Set(description.to_owned()),
        created_at: Set(now),
    }
    .insert(transaction)
    .await
    .map_err(conflict_or_database)?;
    for (suffix, ledger_account, amount) in [
        ("customer", "customer_balance", customer_delta_microunits),
        ("counter", counter_account, -customer_delta_microunits),
    ] {
        ledger_entry::ActiveModel {
            id: Set(format!("entry-{suffix}-{}", Uuid::now_v7())),
            transaction_id: Set(transaction_id.clone()),
            billing_account_id: Set(account.id.clone()),
            ledger_account: Set(ledger_account.to_owned()),
            amount_microunits: Set(amount),
            currency: Set(account.currency.clone()),
            created_at: Set(now),
        }
        .insert(transaction)
        .await?;
    }
    crate::billing::append_event(transaction, &account.id, "ledger.posted", &transaction_id,
        serde_json::json!({"kind": kind, "reference_type": reference_type, "reference_id": reference_id, "customer_delta_microunits": customer_delta_microunits, "currency": account.currency})).await?;
    Ok(())
}

fn validate_project(value: &Project) -> ServiceResult<()> {
    if value.id.trim().is_empty()
        || value.tenant_id.trim().is_empty()
        || value.name.trim().is_empty()
    {
        return Err(ServiceError::Invalid(
            "id, tenant_id and name are required".to_owned(),
        ));
    }
    Ok(())
}

fn validate_usage(event: &UsageEvent) -> ServiceResult<()> {
    if [
        &event.schema_version,
        &event.event_id,
        &event.request_id,
        &event.tenant_id,
        &event.project_id,
        &event.api_key_id,
        &event.model_id,
        &event.model_revision,
        &event.region,
        &event.price_version,
        &event.status,
    ]
    .iter()
    .any(|value| value.trim().is_empty())
        || event.input_tokens < 0
        || event.output_tokens < 0
        || event.cached_input_tokens < 0
        || event.latency_ms < 0
    {
        return Err(ServiceError::Invalid(
            "usage event is missing required fields or contains negative values".to_owned(),
        ));
    }
    Ok(())
}

fn is_chargeable_usage(event: &UsageEvent) -> bool {
    event.status == "succeeded" && (event.input_tokens > 0 || event.output_tokens > 0)
}

pub(crate) fn conflict_or_database(error: sea_orm::DbErr) -> ServiceError {
    if error.to_string().contains("duplicate key")
        || error.to_string().contains("unique constraint")
    {
        ServiceError::Conflict("resource already exists".to_owned())
    } else {
        ServiceError::Database(error)
    }
}

fn stable_id(value: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))[..22]
        .to_owned()
}

fn floor_minor_units(microunits: i64) -> i64 {
    microunits.div_euclid(MICROS_PER_MINOR_UNIT)
}

fn project_from_row(row: project::Model) -> Project {
    Project {
        id: row.id,
        tenant_id: row.tenant_id,
        name: row.name,
    }
}

fn api_key_from_row(row: api_key::Model, now: DateTime<Utc>) -> ApiKey {
    let expires_at = row.expires_at.map(|value| value.with_timezone(&Utc));
    let revoked_at = row.revoked_at.map(|value| value.with_timezone(&Utc));
    ApiKey {
        id: row.id,
        tenant_id: row.tenant_id,
        project_id: row.project_id,
        name: row.name,
        scopes: row.scopes,
        allowed_models: row.allowed_models,
        expires_at,
        rate_limit_rpm: row.rate_limit_rpm,
        rate_limit_tpm: row.rate_limit_tpm,
        monthly_budget: Money {
            currency: row.currency,
            amount: row.monthly_budget_amount,
        },
        created_at: row.created_at.with_timezone(&Utc),
        revoked_at,
        status: key_status(revoked_at, expires_at, now),
    }
}

fn billing_account_from_row(
    row: billing_account::Model,
    balance_microunits: i64,
) -> BillingAccount {
    BillingAccount {
        id: row.id,
        tenant_id: row.tenant_id,
        project_id: row.project_id,
        currency: row.currency.clone(),
        balance: Money {
            currency: row.currency,
            amount: floor_minor_units(balance_microunits),
        },
        enforce_balance: row.enforce_balance,
        status: row.status,
        created_at: row.created_at.with_timezone(&Utc),
        updated_at: row.updated_at.with_timezone(&Utc),
    }
}

fn order_from_row(row: billing_order::Model) -> BillingOrder {
    BillingOrder {
        id: row.id,
        tenant_id: row.tenant_id,
        project_id: row.project_id,
        kind: row.kind,
        amount: Money {
            currency: row.currency,
            amount: row.amount,
        },
        status: row.status,
        description: row.description,
        created_at: row.created_at.with_timezone(&Utc),
        updated_at: row.updated_at.with_timezone(&Utc),
    }
}

fn payment_from_row(row: payment::Model) -> Payment {
    Payment {
        id: row.id,
        order_id: row.order_id,
        provider: row.provider,
        provider_reference: row.provider_reference,
        amount: Money {
            currency: row.currency,
            amount: row.amount,
        },
        status: row.status,
        paid_at: row.paid_at.map(|value| value.with_timezone(&Utc)),
        created_at: row.created_at.with_timezone(&Utc),
    }
}

fn refund_from_row(row: refund::Model) -> Refund {
    Refund {
        id: row.id,
        payment_id: row.payment_id,
        amount: Money {
            currency: row.currency,
            amount: row.amount,
        },
        reason: row.reason,
        status: row.status,
        provider_reference: row.provider_reference,
        created_at: row.created_at.with_timezone(&Utc),
        completed_at: row.completed_at.map(|value| value.with_timezone(&Utc)),
    }
}

fn ledger_entry_from_row(row: ledger_entry::Model) -> LedgerEntry {
    LedgerEntry {
        id: row.id,
        transaction_id: row.transaction_id,
        billing_account_id: row.billing_account_id,
        ledger_account: row.ledger_account,
        amount_microunits: row.amount_microunits,
        currency: row.currency,
        created_at: row.created_at.with_timezone(&Utc),
    }
}

fn ledger_transaction_from_row(
    row: ledger_transaction::Model,
    entries: Vec<LedgerEntry>,
) -> LedgerTransaction {
    LedgerTransaction {
        id: row.id,
        tenant_id: row.tenant_id,
        kind: row.kind,
        reference_type: row.reference_type,
        reference_id: row.reference_id,
        description: row.description,
        created_at: row.created_at.with_timezone(&Utc),
        entries,
    }
}

fn invoice_from_row(row: invoice::Model) -> Invoice {
    Invoice {
        id: row.id,
        tenant_id: row.tenant_id,
        project_id: row.project_id,
        period_start: row.period_start.with_timezone(&Utc),
        period_end: row.period_end.with_timezone(&Utc),
        amount: Money {
            currency: row.currency,
            amount: row.amount,
        },
        status: row.status,
        title: row.title,
        issued_at: row.issued_at.with_timezone(&Utc),
    }
}

#[derive(Default)]
struct ProjectBillingAccumulator {
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
    cost_microunits: i64,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use xscope_domain::{Money, UsageEvent};

    use super::is_chargeable_usage;

    fn usage(status: &str, input_tokens: i64, output_tokens: i64) -> UsageEvent {
        UsageEvent {
            schema_version: "v1".to_owned(),
            event_id: "event".to_owned(),
            request_id: "request".to_owned(),
            occurred_at: Utc::now(),
            tenant_id: "tenant".to_owned(),
            project_id: "project".to_owned(),
            api_key_id: "key".to_owned(),
            model_id: "unknown".to_owned(),
            model_revision: "development".to_owned(),
            endpoint_id: String::new(),
            region: "local".to_owned(),
            price_version: "legacy".to_owned(),
            input_tokens,
            output_tokens,
            cached_input_tokens: 0,
            latency_ms: 0,
            status: status.to_owned(),
            cost: Money::default(),
        }
    }

    #[test]
    fn failed_zero_token_events_are_audited_without_charge() {
        assert!(!is_chargeable_usage(&usage("provider_error", 0, 0)));
    }

    #[test]
    fn successful_token_usage_is_chargeable() {
        assert!(is_chargeable_usage(&usage("succeeded", 2, 4)));
    }
}
