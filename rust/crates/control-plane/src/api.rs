use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, OriginalUri, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use xscope_domain::{
    ApiKeyRequest, CapturePaymentRequest, CreateInvoiceRequest, CreateOrderRequest,
    CreateRefundRequest, PlatformUser, Project, ReconcileRequest, UpdateBalancePolicy,
    UpdateTenantMembership, UsageEvent,
};

use crate::config::Config;
use crate::error::{ServiceError, ServiceResult};
use crate::repository::Repository;

#[derive(Clone)]
pub struct AppState {
    pub repository: Repository,
    pub config: Arc<Config>,
    pub http: reqwest::Client,
    component_targets: Arc<HashMap<String, String>>,
}

#[derive(Clone, Debug)]
struct UserContext {
    user: PlatformUser,
    auth_disabled: bool,
}

impl UserContext {
    fn can_access(&self, tenant_id: &str) -> bool {
        self.auth_disabled
            || self
                .user
                .memberships
                .iter()
                .any(|membership| membership.tenant_id == tenant_id)
    }

    fn is_owner(&self) -> bool {
        self.auth_disabled
            || self
                .user
                .memberships
                .iter()
                .any(|membership| membership.role == "owner")
    }

    fn can_manage(&self, tenant_id: &str) -> bool {
        self.auth_disabled
            || self
                .user
                .memberships
                .iter()
                .any(|membership| membership.tenant_id == tenant_id && membership.role == "owner")
    }
}

impl AppState {
    #[must_use]
    pub fn new(repository: Repository, config: Config) -> Self {
        let component_targets = config.component_targets.iter().cloned().collect();
        Self {
            repository,
            config: Arc::new(config),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("static HTTP client configuration must be valid"),
            component_targets: Arc::new(component_targets),
        }
    }
}

pub fn public_router(state: &AppState) -> Router {
    let admin = Router::new()
        .route("/session", get(session))
        .route("/users", get(list_users))
        .route(
            "/users/{user_id}/memberships/{tenant_id}",
            put(update_membership),
        )
        .route("/components/{name}/health", get(component_health))
        .route("/projects", get(list_projects).post(create_project))
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{id}", delete(revoke_api_key))
        .route("/quote", get(quote))
        .route("/billing/summary", get(billing_summary))
        .route(
            "/billing/accounts/{project_id}",
            get(get_billing_account).put(update_balance_policy),
        )
        .route("/billing/orders", get(list_orders).post(create_order))
        .route("/billing/orders/{order_id}/payments", post(capture_payment))
        .route("/billing/payments", get(list_payments))
        .route("/billing/refunds", get(list_refunds).post(create_refund))
        .route("/billing/ledger", get(list_ledger))
        .route("/billing/invoices", get(list_invoices).post(create_invoice))
        .route("/billing/reconciliation", post(reconcile))
        .route("/model-deployments", get(cluster_proxy).post(cluster_proxy))
        .route(
            "/model-deployments/{namespace}/{name}/scale",
            put(cluster_proxy),
        )
        .route(
            "/model-deployments/{namespace}/{name}",
            delete(cluster_proxy),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            console_identity,
        ));
    let api = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/v1/models", get(models))
        .nest("/admin/v1", admin);
    let mut router = Router::new()
        .merge(api.clone())
        .nest("/api", api)
        .with_state(state.clone());
    if let Some(directory) = &state.config.console_directory {
        let console = tower::ServiceBuilder::new()
            .layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ))
            .service(
                ServeDir::new(directory).fallback(ServeFile::new(directory.join("index.html"))),
            );
        router = router.fallback_service(console);
    }
    router.layer(middleware::from_fn(observe_http))
}

pub fn internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/v1/gateway/snapshot", get(gateway_snapshot))
        .route("/internal/v1/usage-events", post(record_usage))
        .route_layer(middleware::from_fn_with_state(state.clone(), internal_auth))
        .with_state(state)
        .layer(middleware::from_fn(observe_http))
}

async fn observe_http(
    request: axum::extract::Request,
    next: middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument;
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", axum::extract::MatchedPath::as_str);
    if matches!(
        route,
        "/healthz" | "/readyz" | "/api/healthz" | "/api/readyz"
    ) {
        return next.run(request).await;
    }
    let mut trace =
        xscope_telemetry::RequestTrace::start(request.headers(), request.method().as_str(), route);
    let mut response = next.run(request).instrument(trace.span.clone()).await;
    if let Ok(value) = trace.trace_id().parse() {
        response.headers_mut().insert("x-trace-id", value);
    }
    let status = response.status().as_u16();
    trace.finish(
        status,
        if status >= 500 {
            "provider_error"
        } else if status >= 400 {
            "rejected"
        } else {
            "succeeded"
        },
    );
    response
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok", "component": "control-plane"}))
}

async fn ready(State(state): State<AppState>) -> ServiceResult<Json<Value>> {
    state.repository.ping().await?;
    Ok(Json(json!({"status": "ready"})))
}

async fn models(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"object": "list", "data": state.repository.models()}))
}

async fn session(Extension(context): Extension<UserContext>) -> Json<PlatformUser> {
    Json(context.user)
}

async fn list_users(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    require_owner(&context)?;
    let users = state
        .repository
        .list_users()
        .await?
        .into_iter()
        .filter_map(|mut user| {
            user.memberships
                .retain(|membership| context.can_manage(&membership.tenant_id));
            (!user.memberships.is_empty()).then_some(user)
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": users})))
}

async fn update_membership(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((user_id, tenant_id)): Path<(String, String)>,
    Json(request): Json<UpdateTenantMembership>,
) -> ServiceResult<Json<PlatformUser>> {
    require_tenant_owner(&context, &tenant_id)?;
    Ok(Json(
        state
            .repository
            .update_membership(&user_id, &tenant_id, &request.role)
            .await?,
    ))
}

async fn list_projects(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    let projects = state
        .repository
        .list_projects()
        .await?
        .into_iter()
        .filter(|project| context.can_access(&project.tenant_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": projects})))
}

async fn create_project(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    headers: HeaderMap,
    Json(project): Json<Project>,
) -> ServiceResult<(StatusCode, Json<Project>)> {
    require_idempotency_key(&headers)?;
    require_tenant(&context, &project.tenant_id)?;
    let project = state.repository.create_project(project).await?;
    Ok((StatusCode::CREATED, Json(project)))
}

async fn list_api_keys(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    let keys = state
        .repository
        .list_api_keys()
        .await?
        .into_iter()
        .filter(|key| context.can_access(&key.tenant_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": keys})))
}

async fn create_api_key(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    headers: HeaderMap,
    Json(request): Json<ApiKeyRequest>,
) -> ServiceResult<(StatusCode, Json<xscope_domain::IssuedApiKey>)> {
    require_idempotency_key(&headers)?;
    require_tenant(&context, &request.tenant_id)?;
    let key = state.repository.issue_api_key(request).await?;
    Ok((StatusCode::CREATED, Json(key)))
}

async fn revoke_api_key(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<StatusCode> {
    let key = state
        .repository
        .list_api_keys()
        .await?
        .into_iter()
        .find(|key| key.id == id)
        .ok_or(ServiceError::NotFound)?;
    require_tenant(&context, &key.tenant_id)?;
    state.repository.revoke_api_key(&id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct QuoteQuery {
    model: String,
    input_tokens: i64,
    output_tokens: i64,
}

async fn quote(
    State(state): State<AppState>,
    Query(query): Query<QuoteQuery>,
) -> ServiceResult<Json<xscope_domain::Quote>> {
    Ok(Json(state.repository.quote(
        &query.model,
        query.input_tokens,
        query.output_tokens,
    )?))
}

#[derive(Default, Deserialize)]
struct BillingQuery {
    project_id: Option<String>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

async fn billing_summary(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Query(query): Query<BillingQuery>,
) -> ServiceResult<Json<xscope_domain::BillingSummary>> {
    if let Some(project_id) = query
        .project_id
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        authorize_project(&state.repository, &context, project_id).await?;
    }
    let now = Utc::now();
    let start = query.from.unwrap_or_else(|| {
        Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .expect("valid current month")
    });
    let end = query.to.unwrap_or_else(|| {
        let (year, month) = if start.month() == 12 {
            (start.year() + 1, 1)
        } else {
            (start.year(), start.month() + 1)
        };
        Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
            .single()
            .expect("valid next month")
    });
    let mut summary = state
        .repository
        .billing_summary(query.project_id.as_deref(), start, end)
        .await?;
    let allowed_projects = state
        .repository
        .list_projects()
        .await?
        .into_iter()
        .filter(|project| context.can_access(&project.tenant_id))
        .map(|project| project.id)
        .collect::<HashSet<_>>();
    summary
        .projects
        .retain(|item| allowed_projects.contains(&item.project_id));
    summary.requests = summary.projects.iter().map(|item| item.requests).sum();
    summary.input_tokens = summary.projects.iter().map(|item| item.input_tokens).sum();
    summary.output_tokens = summary.projects.iter().map(|item| item.output_tokens).sum();
    summary.total.amount = summary.projects.iter().map(|item| item.cost.amount).sum();
    Ok(Json(summary))
}

async fn get_billing_account(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(project_id): Path<String>,
) -> ServiceResult<Json<xscope_domain::BillingAccount>> {
    authorize_project(&state.repository, &context, &project_id).await?;
    Ok(Json(state.repository.billing_account(&project_id).await?))
}

async fn update_balance_policy(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(project_id): Path<String>,
    Json(request): Json<UpdateBalancePolicy>,
) -> ServiceResult<Json<xscope_domain::BillingAccount>> {
    authorize_project_owner(&state.repository, &context, &project_id).await?;
    Ok(Json(
        state
            .repository
            .update_balance_policy(&project_id, request.enforce_balance)
            .await?,
    ))
}

async fn list_orders(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    require_owner(&context)?;
    let orders = state
        .repository
        .list_orders()
        .await?
        .into_iter()
        .filter(|order| context.can_manage(&order.tenant_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": orders})))
}

async fn create_order(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    headers: HeaderMap,
    Json(request): Json<CreateOrderRequest>,
) -> ServiceResult<(StatusCode, Json<xscope_domain::BillingOrder>)> {
    require_tenant_owner(&context, &request.tenant_id)?;
    require_idempotency_key(&headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.repository.create_order(request).await?),
    ))
}

async fn capture_payment(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(order_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<CapturePaymentRequest>,
) -> ServiceResult<(StatusCode, Json<xscope_domain::Payment>)> {
    let order = state
        .repository
        .list_orders()
        .await?
        .into_iter()
        .find(|order| order.id == order_id)
        .ok_or(ServiceError::NotFound)?;
    require_tenant_owner(&context, &order.tenant_id)?;
    let idempotency_key = require_idempotency_key(&headers)?;
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .repository
                .capture_payment(&order_id, request, &idempotency_key)
                .await?,
        ),
    ))
}

async fn list_payments(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    require_owner(&context)?;
    let order_ids = state
        .repository
        .list_orders()
        .await?
        .into_iter()
        .filter(|order| context.can_manage(&order.tenant_id))
        .map(|order| order.id)
        .collect::<HashSet<_>>();
    let payments = state
        .repository
        .list_payments()
        .await?
        .into_iter()
        .filter(|payment| order_ids.contains(&payment.order_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": payments})))
}

async fn list_refunds(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    require_owner(&context)?;
    let order_ids = state
        .repository
        .list_orders()
        .await?
        .into_iter()
        .filter(|order| context.can_manage(&order.tenant_id))
        .map(|order| order.id)
        .collect::<HashSet<_>>();
    let payment_ids = state
        .repository
        .list_payments()
        .await?
        .into_iter()
        .filter(|payment| order_ids.contains(&payment.order_id))
        .map(|payment| payment.id)
        .collect::<HashSet<_>>();
    let refunds = state
        .repository
        .list_refunds()
        .await?
        .into_iter()
        .filter(|refund| payment_ids.contains(&refund.payment_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": refunds})))
}

async fn create_refund(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    headers: HeaderMap,
    Json(request): Json<CreateRefundRequest>,
) -> ServiceResult<(StatusCode, Json<xscope_domain::Refund>)> {
    let payment = state
        .repository
        .list_payments()
        .await?
        .into_iter()
        .find(|payment| payment.id == request.payment_id)
        .ok_or(ServiceError::NotFound)?;
    let order = state
        .repository
        .list_orders()
        .await?
        .into_iter()
        .find(|order| order.id == payment.order_id)
        .ok_or(ServiceError::NotFound)?;
    require_tenant_owner(&context, &order.tenant_id)?;
    let idempotency_key = require_idempotency_key(&headers)?;
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .repository
                .create_refund(request, &idempotency_key)
                .await?,
        ),
    ))
}

async fn list_ledger(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    require_owner(&context)?;
    let transactions = state
        .repository
        .list_ledger()
        .await?
        .into_iter()
        .filter(|transaction| context.can_manage(&transaction.tenant_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": transactions})))
}

async fn list_invoices(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    require_owner(&context)?;
    let invoices = state
        .repository
        .list_invoices()
        .await?
        .into_iter()
        .filter(|invoice| context.can_manage(&invoice.tenant_id))
        .collect::<Vec<_>>();
    Ok(Json(json!({"object": "list", "data": invoices})))
}

async fn create_invoice(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    headers: HeaderMap,
    Json(request): Json<CreateInvoiceRequest>,
) -> ServiceResult<(StatusCode, Json<xscope_domain::Invoice>)> {
    require_tenant_owner(&context, &request.tenant_id)?;
    require_idempotency_key(&headers)?;
    Ok((
        StatusCode::CREATED,
        Json(state.repository.create_invoice(request).await?),
    ))
}

async fn reconcile(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Json(request): Json<ReconcileRequest>,
) -> ServiceResult<Json<xscope_domain::ReconciliationReport>> {
    require_tenant_owner(&context, &request.tenant_id)?;
    Ok(Json(state.repository.reconcile(request).await?))
}

async fn gateway_snapshot(
    State(state): State<AppState>,
) -> ServiceResult<Json<xscope_domain::GatewaySnapshot>> {
    Ok(Json(state.repository.gateway_snapshot().await?))
}

async fn record_usage(
    State(state): State<AppState>,
    Json(event): Json<UsageEvent>,
) -> ServiceResult<(StatusCode, Json<Value>)> {
    let created = state.repository.record_usage(event).await?;
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({"accepted": true, "duplicate": !created})),
    ))
}

async fn component_health(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ServiceResult<Json<Value>> {
    let target = state
        .component_targets
        .get(&name)
        .ok_or(ServiceError::NotFound)?;
    let response = state
        .http
        .get(target)
        .send()
        .await
        .map_err(|error| ServiceError::Dependency(error.to_string()))?;
    if !response.status().is_success() {
        return Err(ServiceError::Dependency(format!(
            "{name} returned {}",
            response.status()
        )));
    }
    let body = response.text().await.map_err(|error| {
        ServiceError::Dependency(format!("could not read {name} health response: {error}"))
    })?;
    let payload = serde_json::from_str::<Value>(&body)
        .unwrap_or_else(|_| json!({"status": "ok", "component": name}));
    Ok(Json(payload))
}

async fn cluster_proxy(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> ServiceResult<Response> {
    require_owner(&context)?;
    let path = uri.path().strip_prefix("/api").unwrap_or(uri.path());
    let path = path.strip_prefix("/admin").unwrap_or(path);
    let mut target = format!(
        "{}{}",
        state.config.cluster_agent_url.trim_end_matches('/'),
        path
    );
    if let Some(query) = uri.query() {
        target.push('?');
        target.push_str(query);
    }
    let request_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let mut request = state
        .http
        .request(request_method, target)
        .bearer_auth(&state.config.internal_token)
        .body(body);
    let mut trace_headers = HeaderMap::new();
    xscope_telemetry::inject(&tracing::Span::current(), &mut trace_headers);
    request = request.headers(trace_headers);
    if let Some(content_type) = headers.get(header::CONTENT_TYPE) {
        request = request.header(header::CONTENT_TYPE, content_type);
    }
    let response = request
        .send()
        .await
        .map_err(|error| ServiceError::Dependency(error.to_string()))?;
    let status = StatusCode::from_u16(response.status().as_u16())
        .map_err(|error| ServiceError::Internal(error.to_string()))?;
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| ServiceError::Dependency(error.to_string()))?;
    let mut result = (status, bytes).into_response();
    if let Some(content_type) = content_type {
        result
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
    }
    Ok(result)
}

async fn console_identity(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> ServiceResult<Response> {
    let (subject, username, email, auth_disabled) = if state.config.console_auth {
        let username = first_header(
            request.headers(),
            &[
                "x-auth-request-preferred-username",
                "x-forwarded-preferred-username",
                "x-auth-request-user",
                "x-forwarded-user",
            ],
        )
        .ok_or(ServiceError::Unauthorized)?;
        let email = first_header(
            request.headers(),
            &["x-auth-request-email", "x-forwarded-email"],
        )
        .unwrap_or_default();
        let subject = first_header(
            request.headers(),
            &[
                "x-auth-request-sub",
                "x-forwarded-sub",
                "x-auth-request-user",
                "x-forwarded-user",
            ],
        )
        .unwrap_or_else(|| username.clone());
        (subject, username, email, false)
    } else {
        (
            "development-user".to_owned(),
            "development-user".to_owned(),
            "development@local.xscope".to_owned(),
            true,
        )
    };
    let is_bootstrap_admin = auth_disabled
        || state.config.bootstrap_admin_users.contains(&username)
        || state.config.bootstrap_admin_users.contains(&email);
    let default_role = if is_bootstrap_admin {
        Some("owner")
    } else if state.config.auto_join_default_tenant {
        Some("member")
    } else {
        None
    };
    let user = state
        .repository
        .sync_user(
            &subject,
            &username,
            &email,
            &state.config.default_tenant_id,
            default_role,
        )
        .await?;
    request.extensions_mut().insert(UserContext {
        user,
        auth_disabled,
    });
    Ok(next.run(request).await)
}

async fn internal_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> ServiceResult<Response> {
    if state.config.internal_token.is_empty() {
        return Err(ServiceError::InternalAuthUnavailable);
    }
    let expected = Sha256::digest(format!("Bearer {}", state.config.internal_token).as_bytes());
    let candidate = Sha256::digest(
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .as_bytes(),
    );
    if !bool::from(expected.ct_eq(&candidate)) {
        return Err(ServiceError::InvalidInternalToken);
    }
    Ok(next.run(request).await)
}

fn first_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn require_idempotency_key(headers: &HeaderMap) -> ServiceResult<String> {
    let key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !(16..=128).contains(&key.len()) {
        return Err(ServiceError::Invalid(
            "Idempotency-Key must be 16 to 128 bytes".to_owned(),
        ));
    }
    Ok(key.to_owned())
}

fn require_tenant(context: &UserContext, tenant_id: &str) -> ServiceResult<()> {
    if context.can_access(tenant_id) {
        Ok(())
    } else {
        Err(ServiceError::Forbidden)
    }
}

fn require_owner(context: &UserContext) -> ServiceResult<()> {
    if context.is_owner() {
        Ok(())
    } else {
        Err(ServiceError::Forbidden)
    }
}

fn require_tenant_owner(context: &UserContext, tenant_id: &str) -> ServiceResult<()> {
    if context.can_manage(tenant_id) {
        Ok(())
    } else {
        Err(ServiceError::Forbidden)
    }
}

async fn authorize_project(
    repository: &Repository,
    context: &UserContext,
    project_id: &str,
) -> ServiceResult<()> {
    let project = repository
        .list_projects()
        .await?
        .into_iter()
        .find(|project| project.id == project_id)
        .ok_or(ServiceError::NotFound)?;
    require_tenant(context, &project.tenant_id)
}

async fn authorize_project_owner(
    repository: &Repository,
    context: &UserContext,
    project_id: &str,
) -> ServiceResult<()> {
    let project = repository
        .list_projects()
        .await?
        .into_iter()
        .find(|project| project.id == project_id)
        .ok_or(ServiceError::NotFound)?;
    require_tenant_owner(context, &project.tenant_id)
}
