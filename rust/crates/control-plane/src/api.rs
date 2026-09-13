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
    pub alipay: Option<Arc<xscope_payments::Alipay>>,
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
    pub fn new(repository: Repository, config: Config) -> Result<Self, anyhow::Error> {
        let component_targets = config.component_targets.iter().cloned().collect();
        Ok(Self {
            alipay: xscope_payments::Alipay::from_env()?.map(Arc::new),
            repository,
            config: Arc::new(config),
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(3))
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            component_targets: Arc::new(component_targets),
        })
    }
}

pub fn public_router(state: &AppState) -> Router {
    let admin = Router::new()
        .route("/session", get(session))
        .route("/capabilities", get(console_capabilities))
        .route("/operations/alerts", get(ops_alerts))
        .route("/operations/backup", get(backup_status))
        .route(
            "/operations/alerts/{id}/acknowledge",
            post(acknowledge_alert),
        )
        .route("/audit", get(list_operation_audits))
        .route("/users", get(list_users))
        .route(
            "/users/{user_id}/memberships/{tenant_id}",
            put(update_membership),
        )
        .route("/components/{name}/health", get(component_health))
        .route("/projects", get(list_projects).post(create_project))
        .route("/clusters", get(list_clusters).post(register_cluster))
        .route("/clusters/{id}/desired-state", put(put_cluster_desired))
        .route("/clusters/{id}/credential", post(rotate_cluster_credential))
        .route("/clusters/{id}/revoke", post(revoke_cluster))
        .route("/route-pools", get(list_route_pools))
        .route("/model-catalog", get(model_catalog))
        .route("/model-catalog/{id}", put(put_model))
        .route("/serving-endpoints", get(serving_endpoint_versions))
        .route("/managed-pools", get(list_managed_pools).post(bind_pool))
        .route("/managed-pools/{id}", get(managed_pool_status))
        .route("/managed-pools/{id}/{action}", post(pool_transition))
        .route(
            "/projects/{project_id}/models/{model}/route-policy/acks",
            get(route_acks),
        )
        .route("/serving-endpoints/{id}", put(put_serving_endpoint))
        .route("/route-policies", get(list_route_policies))
        .route(
            "/projects/{project_id}/models/{model}/route-policy",
            put(put_route_policy),
        )
        .route(
            "/projects/{project_id}/models/{model}/route-policy/history",
            get(route_history),
        )
        .route(
            "/projects/{project_id}/models/{model}/route-policy/actions",
            post(release_action),
        )
        .route("/api-keys", get(list_api_keys).post(create_api_key))
        .route("/api-keys/{id}", delete(revoke_api_key))
        .route("/quote", get(quote))
        .route("/billing/summary", get(billing_summary))
        .route(
            "/billing/accounts/{project_id}/event-worker",
            get(event_worker_status),
        )
        .route(
            "/billing/accounts/{project_id}/event-worker/retry",
            post(retry_event_worker),
        )
        .route(
            "/billing/accounts/{project_id}/position",
            get(console_billing_position),
        )
        .route(
            "/billing/accounts/{project_id}/pending-reservations",
            get(console_pending_reservations),
        )
        .route(
            "/billing/accounts/{project}/reservations/{id}/reviews",
            get(list_billing_reviews).post(submit_billing_evidence),
        )
        .route(
            "/billing/accounts/{project}/reviews/{id}",
            get(billing_review_detail),
        )
        .route(
            "/billing/accounts/{project}/reservations/{id}/waivers",
            post(submit_billing_waiver),
        )
        .route(
            "/billing/accounts/{project}/reviews/{id}/decision",
            post(decide_billing_review),
        )
        .route(
            "/billing/accounts/{project_id}",
            get(get_billing_account).put(update_balance_policy),
        )
        .route("/billing/orders", get(list_orders).post(create_order))
        .route(
            "/billing/orders/{id}/alipay/checkout",
            post(alipay_checkout),
        )
        .route("/billing/orders/{id}/alipay/sync", post(alipay_sync))
        .route("/billing/orders/{order_id}/payments", post(capture_payment))
        .route("/billing/payments", get(list_payments))
        .route("/billing/refunds", get(list_refunds).post(create_refund))
        .route("/billing/alipay/refunds", post(prepare_alipay_refund))
        .route(
            "/billing/alipay/refunds/{id}/submit",
            post(submit_alipay_refund),
        )
        .route(
            "/billing/alipay/refunds/{id}/sync",
            post(sync_alipay_refund),
        )
        .route(
            "/billing/alipay/refunds/{id}/evidence",
            post(import_refund_evidence).layer(axum::extract::DefaultBodyLimit::max(256 * 1024)),
        )
        .route("/billing/ledger", get(list_ledger))
        .route("/billing/invoices", get(list_invoices).post(create_invoice))
        .route(
            "/billing/invoices/{id}/tax-request",
            get(tax_request_status).post(request_tax_invoice),
        )
        .route("/billing/capabilities", get(payment_capabilities))
        .route("/billing/reconciliation", post(reconcile))
        .route("/model-deployments", get(cluster_proxy).post(cluster_proxy))
        .route(
            "/model-deployments/{namespace}/{name}/scale",
            put(cluster_proxy),
        )
        .route(
            "/model-deployments/{namespace}/{name}",
            delete(cluster_proxy).put(cluster_proxy),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            console_identity,
        ));
    let api = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/v1/models", get(models))
        .route(
            "/payments/alipay/notify",
            post(alipay_notify).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
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
        .route("/internal/v1/billing/reservations", post(reserve_money))
        .route(
            "/internal/v1/billing/projects/{project}/reservations/{id}/unresolved",
            post(unresolved_money),
        )
        .route("/internal/v1/billing/workers/claim", post(claim_event_job))
        .route(
            "/internal/v1/billing/workers/complete",
            post(complete_event_job),
        )
        .route("/internal/v1/billing/workers/fail", post(fail_event_job))
        .route(
            "/internal/v1/billing/projects/{project}/projections/check",
            get(projection_check),
        )
        .route(
            "/internal/v1/billing/projects/{project}/projections/rebuild",
            post(projection_rebuild),
        )
        .route(
            "/internal/v1/billing/projects/{project}/pending-reservations",
            get(pending_reservations),
        )
        .route(
            "/internal/v1/billing/projects/{project}/reservations/{id}",
            get(money_reservation),
        )
        .route(
            "/internal/v1/billing/projects/{project}/reservations/{id}/dispatch",
            post(dispatch_money),
        )
        .route(
            "/internal/v1/billing/projects/{project}/reservations/{id}/release",
            post(release_money),
        )
        .route(
            "/internal/v1/billing/projects/{project}/reservations/{id}/settle",
            post(settle_money),
        )
        .route(
            "/internal/v1/billing/projects/{project}/consumers/{consumer}/poll",
            post(poll_billing_events),
        )
        .route(
            "/internal/v1/billing/projects/{project}/consumers/{consumer}/ack",
            post(ack_billing_events),
        )
        .route("/internal/v1/gateway/traffic-report", post(traffic_report))
        .route_layer(middleware::from_fn_with_state(state.clone(), internal_auth))
        // These handlers verify the cluster-specific credential themselves.
        // A Gateway internal token is deliberately insufficient here.
        .route("/internal/v1/operations/alerts", post(receive_alerts))
        .route("/internal/v1/clusters/{id}/poll", post(poll_cluster))
        .route("/internal/v1/clusters/{id}/report", post(report_cluster))
        .route(
            "/internal/v1/traffic/authorize",
            get(authorize_serving).post(authorize_serving),
        )
        .route(
            "/internal/v1/traffic/authorize/{*rest}",
            get(authorize_serving).post(authorize_serving),
        )
        .route(
            "/internal/v1/clusters/{id}/gateway-proofs",
            get(gateway_proof_tasks).post(gateway_proof),
        )
        .route(
            "/internal/v1/clusters/{id}/observations",
            get(observation_tasks).post(observe_pool),
        )
        .with_state(state)
        .layer(middleware::from_fn(observe_http))
}

fn platform_admin(state: &AppState, context: &UserContext) -> ServiceResult<()> {
    if context.auth_disabled
        || state
            .config
            .platform_admin_subjects
            .contains(&context.user.external_subject)
    {
        Ok(())
    } else {
        Err(ServiceError::Forbidden)
    }
}
async fn console_capabilities(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> Json<Value> {
    Json(
        json!({"platform_admin":platform_admin(&state, &context).is_ok(), "billing_reviewer":is_billing_reviewer(&state, &context), "evidence_submission":!context.auth_disabled, "alert_receiver_configured":state.config.alert_webhook_token.is_some()}),
    )
}
async fn receive_alerts(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ServiceResult<StatusCode> {
    use subtle::ConstantTimeEq;
    let token = state
        .config
        .alert_webhook_token
        .as_ref()
        .ok_or(ServiceError::InternalAuthUnavailable)?;
    let supplied = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ServiceError::Unauthorized)?;
    if !bool::from(token.as_bytes().ct_eq(supplied.as_bytes())) {
        return Err(ServiceError::Unauthorized);
    }
    if body.len() > 262144 {
        return Err(ServiceError::Invalid("alert payload too large".into()));
    }
    let request = serde_json::from_slice(&body)
        .map_err(|_| ServiceError::Invalid("invalid alert payload".into()))?;
    state.repository.receive_alerts(request).await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn ops_alerts(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Query(query): Query<crate::ops::AlertQuery>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(state.repository.ops_alerts(query).await?))
}
async fn backup_status(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        crate::ops::backup_status(state.config.ops_prometheus_url.as_deref()).await?,
    ))
}
async fn acknowledge_alert(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    Json(request): Json<crate::ops::Acknowledge>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        state
            .repository
            .acknowledge_alert(&id, &context.user.id, request)
            .await?,
    ))
}
async fn list_clusters(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(state.repository.list_clusters().await?))
}

async fn list_operation_audits(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Query(query): Query<crate::audit::AuditQuery>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(state.repository.list_operation_audits(query).await?))
}
async fn register_cluster(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Json(request): Json<crate::clusters::RegisterCluster>,
) -> ServiceResult<(StatusCode, Json<Value>)> {
    platform_admin(&state, &context)?;
    Ok((
        StatusCode::CREATED,
        Json(
            state
                .repository
                .register_cluster(request, &context.user.id)
                .await?,
        ),
    ))
}
async fn put_cluster_desired(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    Json(request): Json<xscope_domain::cluster::DesiredState>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        state
            .repository
            .put_cluster_desired(&id, request, &context.user.id)
            .await?,
    ))
}
async fn rotate_cluster_credential(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    Json(request): Json<crate::clusters::RotateCredential>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        state
            .repository
            .rotate_cluster_credential(&id, request, &context.user.id)
            .await?,
    ))
}
async fn revoke_cluster(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        state
            .repository
            .revoke_cluster(&id, &context.user.id)
            .await?,
    ))
}
fn cluster_bearer(headers: &HeaderMap) -> ServiceResult<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| token.len() == 43)
        .ok_or(ServiceError::Unauthorized)
}
async fn poll_cluster(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .poll_cluster(&id, cluster_bearer(&headers)?)
            .await?,
    ))
}
async fn report_cluster(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<xscope_domain::cluster::ClusterReport>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .report_cluster(&id, cluster_bearer(&headers)?, request)
            .await?,
    ))
}

async fn projection_check(
    State(state): State<AppState>,
    Path(project): Path<String>,
    Query(request): Query<crate::billing_projection::CheckRequest>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .projection_check(&project, request, false)
            .await?,
    ))
}

async fn projection_rebuild(
    State(state): State<AppState>,
    Path(project): Path<String>,
    Json(request): Json<crate::billing_projection::CheckRequest>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .projection_check(&project, request, true)
            .await?,
    ))
}

async fn reserve_money(
    State(state): State<AppState>,
    Json(request): Json<crate::billing::ReserveRequest>,
) -> ServiceResult<Json<xscope_entities::billing_reservation::Model>> {
    Ok(Json(state.repository.reserve_money(request).await?))
}

async fn claim_event_job(
    State(state): State<AppState>,
    Json(request): Json<crate::event_worker::ClaimRequest>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(state.repository.claim_event_job(request).await?))
}
async fn complete_event_job(
    State(state): State<AppState>,
    Json(request): Json<crate::event_worker::Lease>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(state.repository.complete_event_job(request).await?))
}
async fn fail_event_job(
    State(state): State<AppState>,
    Json(request): Json<crate::event_worker::FailRequest>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(state.repository.fail_event_job(request).await?))
}
async fn money_reservation(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
) -> ServiceResult<Json<xscope_entities::billing_reservation::Model>> {
    Ok(Json(
        state.repository.money_reservation(&project, &id).await?,
    ))
}
async fn dispatch_money(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
) -> ServiceResult<Json<xscope_entities::billing_reservation::Model>> {
    Ok(Json(state.repository.dispatch_money(&project, &id).await?))
}
async fn release_money(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<crate::billing::ReleaseRequest>,
) -> ServiceResult<Json<xscope_entities::billing_reservation::Model>> {
    Ok(Json(
        state
            .repository
            .release_money(&project, &id, request)
            .await?,
    ))
}
async fn settle_money(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<crate::billing::SettleRequest>,
) -> ServiceResult<Json<xscope_entities::billing_reservation::Model>> {
    Ok(Json(
        state
            .repository
            .settle_money(&project, &id, request)
            .await?,
    ))
}
async fn poll_billing_events(
    State(state): State<AppState>,
    Path((project, consumer)): Path<(String, String)>,
    Json(request): Json<crate::billing::PollRequest>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .poll_billing_events(&project, &consumer, request)
            .await?,
    ))
}

async fn pending_reservations(
    State(state): State<AppState>,
    Path(project): Path<String>,
    Query(request): Query<crate::billing_feed::PendingRequest>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .pending_reservations(&project, request)
            .await?,
    ))
}
async fn ack_billing_events(
    State(state): State<AppState>,
    Path((project, consumer)): Path<(String, String)>,
    Json(request): Json<crate::billing::AckRequest>,
) -> ServiceResult<Json<xscope_entities::billing_consumer::Model>> {
    Ok(Json(
        state
            .repository
            .ack_billing_events(&project, &consumer, request)
            .await?,
    ))
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

async fn models(State(state): State<AppState>) -> ServiceResult<Json<Value>> {
    Ok(Json(
        json!({"object": "list", "data": state.repository.models().await?}),
    ))
}

async fn model_catalog(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(json!({"data": state.repository.catalog().await?})))
}
async fn put_model(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    Json(request): Json<xscope_domain::catalog::PutModel>,
) -> ServiceResult<Json<xscope_domain::catalog::CatalogModel>> {
    platform_admin(&state, &context)?;
    Ok(Json(state.repository.put_model(&id, request).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutServingEndpoint {
    expected_generation: i64,
    endpoint: xscope_domain::catalog::ServingEndpoint,
}
async fn serving_endpoint_versions(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(state.repository.serving_endpoint_versions().await?))
}
async fn put_serving_endpoint(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    Json(request): Json<PutServingEndpoint>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    let generation = state
        .repository
        .put_serving_endpoint(&id, request.expected_generation, request.endpoint)
        .await?;
    Ok(Json(json!({"id":id,"generation":generation})))
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

async fn list_route_pools(State(state): State<AppState>) -> ServiceResult<Json<Value>> {
    Ok(Json(
        json!({"object": "list", "data": state.repository.route_pools(&state.config.route_pools).await?}),
    ))
}

async fn list_route_policies(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    let policies: Vec<_> = state
        .repository
        .list_route_policies()
        .await?
        .into_iter()
        .filter(|policy| context.can_access(&policy.tenant_id))
        .collect();
    Ok(Json(json!({"object": "list", "data": policies})))
}

async fn put_route_policy(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project_id, model)): Path<(String, String)>,
    Json(request): Json<xscope_domain::PutRoutePolicy>,
) -> ServiceResult<Json<xscope_domain::RoutePolicy>> {
    let project = state.repository.get_project(&project_id).await?;
    if !context.can_manage(&project.tenant_id) {
        return Err(ServiceError::Forbidden);
    }
    let pools = state
        .repository
        .route_pools(&state.config.route_pools)
        .await?;
    let policy = state
        .repository
        .put_route_policy(&project, &model, request, &pools, &context.user.id, "put")
        .await?;
    tracing::info!(
        project_id,
        model,
        revision = policy.revision,
        user_id = context.user.id,
        "route policy updated"
    );
    Ok(Json(policy))
}

async fn route_history(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project_id, model)): Path<(String, String)>,
    Query(query): Query<crate::releases::HistoryQuery>,
) -> ServiceResult<Json<Value>> {
    let project = state.repository.get_project(&project_id).await?;
    require_tenant(&context, &project.tenant_id)?;
    Ok(Json(
        state
            .repository
            .route_history(&project_id, &model, query)
            .await?,
    ))
}

async fn release_action(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project_id, model)): Path<(String, String)>,
    Json(action): Json<xscope_domain::routing::ReleaseAction>,
) -> ServiceResult<Json<xscope_domain::RoutePolicy>> {
    let project = state.repository.get_project(&project_id).await?;
    if !context.can_manage(&project.tenant_id) {
        return Err(ServiceError::Forbidden);
    }
    let pools = state
        .repository
        .route_pools(&state.config.route_pools)
        .await?;
    Ok(Json(
        state
            .repository
            .release_action(&project, &model, action, &pools, &context.user.id)
            .await?,
    ))
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
    Ok(Json(
        state
            .repository
            .quote(&query.model, query.input_tokens, query.output_tokens)
            .await?,
    ))
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
    let start = if let Some(start) = query.from {
        start
    } else {
        Utc.with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .ok_or_else(|| ServiceError::Internal("could not construct current month".into()))?
    };
    let end = if let Some(end) = query.to {
        end
    } else {
        let (year, month) = if start.month() == 12 {
            (start.year() + 1, 1)
        } else {
            (start.year(), start.month() + 1)
        };
        Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0)
            .single()
            .ok_or_else(|| ServiceError::Internal("could not construct next month".into()))?
    };
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

async fn event_worker_status(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(project_id): Path<String>,
) -> ServiceResult<Json<Value>> {
    authorize_project_owner(&state.repository, &context, &project_id).await?;
    let account = state.repository.billing_account(&project_id).await?;
    Ok(Json(state.repository.event_job_status(&account.id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetryEventJob {
    reason: String,
}

async fn retry_event_worker(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(project_id): Path<String>,
    Json(request): Json<RetryEventJob>,
) -> ServiceResult<Json<Value>> {
    authorize_project_owner(&state.repository, &context, &project_id).await?;
    let account = state.repository.billing_account(&project_id).await?;
    Ok(Json(
        state
            .repository
            .retry_event_job(&account.id, &context.user.id, &request.reason)
            .await?,
    ))
}

async fn console_billing_position(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(project_id): Path<String>,
) -> ServiceResult<Json<Value>> {
    authorize_project(&state.repository, &context, &project_id).await?;
    Ok(Json(state.repository.billing_position(&project_id).await?))
}

async fn console_pending_reservations(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(project_id): Path<String>,
    Query(request): Query<crate::billing_feed::PendingRequest>,
) -> ServiceResult<Json<Value>> {
    authorize_billing_review_read(&state, &context, &project_id).await?;
    let page = state
        .repository
        .pending_reservations(&project_id, request)
        .await?;
    // Explicit public DTO: never expose internal spec, frozen prices or
    // completion payloads wholesale through the console endpoint.
    let rows = page["data"]
        .as_array()
        .ok_or_else(|| ServiceError::Internal("invalid pending page".into()))?;
    let data: Vec<Value> = rows.iter().map(|row| json!({
        "id": row["id"], "request_id": row["request_id"], "api_key_id": row["api_key_id"],
        "state": row["state"], "created_at": row["created_at"], "updated_at": row["updated_at"],
        "reserved_microunits": row["reserved_microunits"].as_i64().map(|v| v.to_string()),
        "unresolved_reason": row["completion"]["reason"].as_str().filter(|reason| matches!(*reason, "usage_missing" | "usage_over_limit")),
    })).collect();
    Ok(Json(
        json!({"data": data, "next": page["next"], "created_before": page["created_before"], "requires_usage_evidence": true,
            "resolution_options": ["usage_evidence", "reviewed_loss_waiver"]}),
    ))
}

fn is_billing_reviewer(state: &AppState, context: &UserContext) -> bool {
    !context.auth_disabled
        && state
            .config
            .billing_reviewer_subjects
            .contains(&context.user.external_subject)
}

async fn authorize_billing_review_read(
    state: &AppState,
    context: &UserContext,
    project: &str,
) -> ServiceResult<()> {
    if is_billing_reviewer(state, context) {
        return Ok(());
    }
    authorize_project_owner(&state.repository, context, project).await
}

async fn submit_billing_evidence(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<crate::billing_review::EvidenceRequest>,
) -> ServiceResult<Json<Value>> {
    if context.auth_disabled {
        return Err(ServiceError::Forbidden);
    }
    authorize_project_owner(&state.repository, &context, &project).await?;
    Ok(Json(
        state
            .repository
            .submit_billing_evidence(
                &project,
                &id,
                &context.user.id,
                &context.user.external_subject,
                request,
            )
            .await?,
    ))
}

async fn submit_billing_waiver(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<crate::billing_review::WaiverRequest>,
) -> ServiceResult<Json<Value>> {
    // Loss waivers are an operator decision, never a customer self-refund API.
    if !is_billing_reviewer(&state, &context) {
        return Err(ServiceError::Forbidden);
    }
    Ok(Json(
        state
            .repository
            .submit_billing_waiver(
                &project,
                &id,
                &context.user.id,
                &context.user.external_subject,
                request,
            )
            .await?,
    ))
}

async fn unresolved_money(
    State(state): State<AppState>,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<xscope_domain::billing::UnresolvedRequest>,
) -> ServiceResult<Json<xscope_entities::billing_reservation::Model>> {
    Ok(Json(
        state
            .repository
            .unresolved_money(&project, &id, request)
            .await?,
    ))
}

async fn list_billing_reviews(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project, id)): Path<(String, String)>,
    Query(request): Query<crate::billing_review::ListRequest>,
) -> ServiceResult<Json<Value>> {
    authorize_billing_review_read(&state, &context, &project).await?;
    Ok(Json(
        state
            .repository
            .billing_reviews(&project, &id, request)
            .await?,
    ))
}

async fn billing_review_detail(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project, id)): Path<(String, String)>,
) -> ServiceResult<Json<Value>> {
    authorize_billing_review_read(&state, &context, &project).await?;
    Ok(Json(
        state
            .repository
            .billing_review_detail(&project, &id)
            .await?,
    ))
}

async fn decide_billing_review(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project, id)): Path<(String, String)>,
    Json(request): Json<crate::billing_review::DecisionRequest>,
) -> ServiceResult<Json<Value>> {
    if !is_billing_reviewer(&state, &context) {
        return Err(ServiceError::Forbidden);
    }
    Ok(Json(
        state
            .repository
            .decide_billing_review(
                &project,
                &id,
                &context.user.id,
                &context.user.external_subject,
                request,
            )
            .await?,
    ))
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
    // This endpoint is a development fixture, never proof of external payment.
    if state.config.console_auth || state.alipay.is_some() || request.provider != "manual-test" {
        return Err(ServiceError::Forbidden);
    }
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

fn alipay_provider(state: &AppState) -> ServiceResult<&xscope_payments::Alipay> {
    state
        .alipay
        .as_deref()
        .ok_or_else(|| ServiceError::Dependency("Alipay is not configured".into()))
}
async fn alipay_checkout(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    let order = state.repository.payment_order(&id).await?;
    require_tenant_owner(&context, &order.tenant_id)?;
    Ok(Json(
        state
            .repository
            .alipay_checkout(&id, alipay_provider(&state)?)
            .await?,
    ))
}
async fn alipay_sync(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    let order = state.repository.payment_order(&id).await?;
    require_tenant_owner(&context, &order.tenant_id)?;
    let provider = alipay_provider(&state)?;
    let trade = provider
        .query(&id)
        .await
        .map_err(crate::payment_service::provider_error)?;
    Ok(Json(
        state
            .repository
            .accept_alipay_trade(provider, trade)
            .await?,
    ))
}
async fn alipay_notify(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ServiceResult<&'static str> {
    if body.len() > 64 * 1024
        || !headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("application/x-www-form-urlencoded"))
    {
        return Err(ServiceError::Invalid(
            "invalid payment notification format".into(),
        ));
    }
    let provider = alipay_provider(&state)?;
    let trade = provider
        .verify_notification(&body)
        .map_err(crate::payment_service::provider_error)?;
    state
        .repository
        .accept_alipay_trade(provider, trade)
        .await?;
    Ok("success") // Only after durable evidence/payment transaction commit.
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

async fn prepare_alipay_refund(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Json(request): Json<CreateRefundRequest>,
) -> ServiceResult<Json<Value>> {
    use sea_orm::EntityTrait;
    let payment = xscope_entities::payment::Entity::find_by_id(&request.payment_id)
        .one(&state.repository.db)
        .await?
        .ok_or(ServiceError::NotFound)?;
    let order = state.repository.payment_order(&payment.order_id).await?;
    require_tenant_owner(&context, &order.tenant_id)?;
    Ok(Json(
        state
            .repository
            .prepare_alipay_refund(request, alipay_provider(&state)?)
            .await?,
    ))
}
async fn submit_alipay_refund(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    alipay_refund_action(state, context, id, true).await
}
async fn sync_alipay_refund(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    alipay_refund_action(state, context, id, false).await
}

async fn import_refund_evidence(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    body: Bytes,
) -> ServiceResult<Json<Value>> {
    let payment = state.repository.refund_payment(&id).await?;
    let order = state.repository.payment_order(&payment.order_id).await?;
    require_tenant_owner(&context, &order.tenant_id)?;
    Ok(Json(
        state
            .repository
            .import_refund_evidence(&id, &body, alipay_provider(&state)?)
            .await?,
    ))
}
async fn alipay_refund_action(
    state: AppState,
    context: UserContext,
    id: String,
    submit: bool,
) -> ServiceResult<Json<Value>> {
    let payment = state.repository.refund_payment(&id).await?;
    let order = state.repository.payment_order(&payment.order_id).await?;
    require_tenant_owner(&context, &order.tenant_id)?;
    Ok(Json(
        state
            .repository
            .alipay_refund_operation(&id, alipay_provider(&state)?, submit)
            .await?,
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

async fn payment_capabilities(State(state): State<AppState>) -> Json<Value> {
    Json(
        json!({"alipay":{"configured":state.alipay.is_some(),"environment":state.alipay.as_ref().map(|p|p.environment()),"currency":"CNY","sandbox_credits_balance":false},"tax":{"jurisdiction":"CN","provider_configured":false,"issuance_enabled":false},"invoice_records":"billing_statements_not_tax_invoices"}),
    )
}
async fn tax_request_status(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    let statement = state.repository.statement(&id).await?;
    require_tenant_owner(&context, &statement.tenant_id)?;
    Ok(Json(state.repository.tax_request_status(&id).await?))
}
async fn request_tax_invoice(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
    Json(request): Json<crate::tax::TaxRequest>,
) -> ServiceResult<Json<Value>> {
    let statement = state.repository.statement(&id).await?;
    require_tenant_owner(&context, &statement.tenant_id)?;
    Ok(Json(
        state.repository.request_tax_invoice(&id, request).await?,
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
    Query(query): Query<GatewayQuery>,
) -> ServiceResult<Json<xscope_domain::GatewaySnapshot>> {
    Ok(Json(if let Some(id) = query.session {
        state
            .repository
            .traffic_snapshot(
                &id,
                query
                    .identity
                    .map(|value| {
                        serde_json::from_str(&value)
                            .map_err(|_| ServiceError::Invalid("invalid gateway identity".into()))
                    })
                    .transpose()?,
            )
            .await?
    } else {
        state.repository.gateway_snapshot().await?
    }))
}

#[derive(Deserialize)]
struct GatewayQuery {
    session: Option<String>,
    identity: Option<String>,
}

async fn authorize_serving(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ServiceResult<StatusCode> {
    state.repository.authorize_serving(&headers).await?;
    Ok(StatusCode::OK)
}
async fn gateway_proof_tasks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .gateway_proof_tasks(&id, cluster_bearer(&headers)?)
            .await?,
    ))
}
async fn gateway_proof(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(proof): Json<xscope_domain::traffic::GatewayProof>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .gateway_proof(&id, cluster_bearer(&headers)?, proof)
            .await?,
    ))
}
async fn route_acks(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((project, model)): Path<(String, String)>,
) -> ServiceResult<Json<Value>> {
    let project = state.repository.get_project(&project).await?;
    if !context.can_manage(&project.tenant_id) {
        return Err(ServiceError::Forbidden);
    }
    Ok(Json(
        state.repository.route_acks(&project.id, &model).await?,
    ))
}

async fn traffic_report(
    State(state): State<AppState>,
    Json(report): Json<xscope_domain::traffic::TrafficReport>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(state.repository.traffic_report(report).await?))
}
async fn list_managed_pools(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
) -> ServiceResult<Json<Value>> {
    use sea_orm::{EntityTrait, QueryOrder, QuerySelect};
    platform_admin(&state, &context)?;
    let rows = xscope_entities::managed_pool::Entity::find()
        .order_by_asc(xscope_entities::managed_pool::Column::Id)
        .limit(1000)
        .all(&state.repository.db)
        .await?;
    Ok(Json(
        json!({"data":rows.into_iter().map(|r| json!({"id":r.id,"cluster_id":r.cluster_id,"deployment":r.deployment,"generation":r.generation,"state":r.state})).collect::<Vec<_>>()}),
    ))
}
async fn bind_pool(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Json(request): Json<xscope_domain::traffic::PoolBinding>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        state
            .repository
            .bind_pool(request, &context.user.id, &state.config.route_pools)
            .await?,
    ))
}
async fn managed_pool_status(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path(id): Path<String>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(state.repository.managed_pool_status(&id).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PoolTransition {
    expected_generation: i64,
}
async fn pool_transition(
    State(state): State<AppState>,
    Extension(context): Extension<UserContext>,
    Path((id, action)): Path<(String, String)>,
    Json(request): Json<PoolTransition>,
) -> ServiceResult<Json<Value>> {
    platform_admin(&state, &context)?;
    Ok(Json(
        state
            .repository
            .pool_transition(&id, request.expected_generation, &action, &context.user.id)
            .await?,
    ))
}
async fn observation_tasks(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .observation_tasks(&id, cluster_bearer(&headers)?)
            .await?,
    ))
}
async fn observe_pool(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(report): Json<xscope_domain::traffic::PoolObservation>,
) -> ServiceResult<Json<Value>> {
    Ok(Json(
        state
            .repository
            .observe_pool(&id, cluster_bearer(&headers)?, report)
            .await?,
    ))
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
    let mutation = matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    );
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |route| route.as_str())
        .to_owned();
    let actor = user.id.clone();
    let intent = if mutation {
        Some(
            state
                .repository
                .audit_operation(
                    &actor,
                    "console.intent",
                    &route,
                    json!({"method":request.method().as_str()}),
                )
                .await?,
        )
    } else {
        None
    };
    request.extensions_mut().insert(UserContext {
        user,
        auth_disabled,
    });
    let mut response = next.run(request).await;
    // API-key/cluster credentials and financial responses must not be cached.
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(intent) = intent {
        if state
            .repository
            .audit_operation(
                &actor,
                "console.completed",
                &route,
                json!({"intent_id":intent,"status":response.status().as_u16()}),
            )
            .await
            .is_err()
        {
            // The operation may already be committed. Never misreport it as a
            // rollback; retain the durable intent and expose an audit-gap alert.
            xscope_telemetry::background_event("audit", "completion_missing");
            tracing::error!("console completion audit unavailable; durable intent retained");
        }
        if let Ok(value) = intent.parse() {
            response.headers_mut().insert("x-xscope-audit-id", value);
        }
    }
    Ok(response)
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
