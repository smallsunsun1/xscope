use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, put},
};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, ListParams, PostParams},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use xscope_kubernetes::{
    Error,
    api::{ModelDeployment, validate, validate_name},
};

#[derive(Clone)]
struct App {
    client: Client,
    token: Arc<String>,
}
struct ApiError(Error);
impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}
impl From<kube::Error> for ApiError {
    fn from(e: kube::Error) -> Self {
        Self(Error::Kube(e))
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self.0 {
            Error::Invalid(message) => (400, "invalid_request", message),
            Error::Kube(kube::Error::Api(ref e)) if e.code == 404 => (
                404,
                "not_found",
                "model deployment not found or CRD not installed".into(),
            ),
            Error::Kube(kube::Error::Api(ref e)) if e.code == 409 => (
                409,
                "conflict",
                "resource already exists or was concurrently modified".into(),
            ),
            Error::Kube(kube::Error::Api(ref e)) if matches!(e.code, 400 | 422) => {
                (400, "invalid_request", e.message.clone())
            }
            error => {
                tracing::warn!(%error,"Kubernetes request failed");
                (
                    503,
                    "cluster_unavailable",
                    "Kubernetes API is unavailable".into(),
                )
            }
        };
        (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}
fn router(app: App) -> Router {
    let secured = Router::new()
        .route("/v1/model-deployments", get(list).post(create))
        .route("/v1/model-deployments/{namespace}/{name}/scale", put(scale))
        .route("/v1/model-deployments/{namespace}/{name}", delete(remove))
        .route_layer(middleware::from_fn_with_state(app.clone(), authenticate));
    Router::new()
        .merge(secured)
        .route(
            "/healthz",
            get(|| async { Json(json!({"status":"ok","component":"cluster-agent"})) }),
        )
        .route("/readyz", get(|| async { Json(json!({"status":"ready"})) }))
        .layer(DefaultBodyLimit::max(1 << 20))
        .layer(middleware::from_fn(observe))
        .with_state(app)
}
async fn authenticate(State(app): State<App>, request: Request, next: Next) -> Response {
    if app.token.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":{"code":"internal_auth_unavailable"}})),
        )
            .into_response();
    }
    let expected = Sha256::digest(format!("Bearer {}", app.token));
    let candidate = Sha256::digest(
        request
            .headers()
            .get("authorization")
            .map_or(&b""[..], |h| h.as_bytes()),
    );
    if !bool::from(expected.ct_eq(&candidate)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"code":"invalid_internal_token"}})),
        )
            .into_response();
    }
    next.run(request).await
}
async fn observe(request: Request, next: Next) -> Response {
    use tracing::Instrument;
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |p| p.as_str());
    if matches!(route, "/healthz" | "/readyz") {
        return next.run(request).await;
    }
    let mut trace =
        xscope_telemetry::RequestTrace::start(request.headers(), request.method().as_str(), route);
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        next.run(request).instrument(trace.span.clone()),
    )
    .await;
    let mut response = result.unwrap_or_else(|_| StatusCode::GATEWAY_TIMEOUT.into_response());
    let status = response.status().as_u16();
    if let Ok(value) = trace.trace_id().parse() {
        response.headers_mut().insert("x-trace-id", value);
    }
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
#[derive(Deserialize)]
struct Namespace {
    namespace: Option<String>,
}
async fn list(
    State(app): State<App>,
    Query(query): Query<Namespace>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let ns = query
        .namespace
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| "xscope-system".into());
    validate_name(&ns, true)?;
    let api: Api<ModelDeployment> = Api::namespaced(app.client, &ns);
    let mut items = api.list(&ListParams::default()).await?.items;
    items.sort_by(|a, b| {
        b.metadata
            .creation_timestamp
            .cmp(&a.metadata.creation_timestamp)
    });
    Ok(Json(json!({"object":"list","data":items})))
}
async fn create(
    State(app): State<App>,
    body: Result<Json<ModelDeployment>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<ModelDeployment>), ApiError> {
    let Json(mut model) = body.map_err(|_| Error::Invalid("invalid JSON body".into()))?;
    if model.namespace().is_none_or(|ns| ns.is_empty()) {
        model.metadata.namespace = Some("xscope-system".into());
    }
    validate(&mut model)?;
    // Only accept a new top-level resource, never client-supplied status or ownership.
    model.status = None;
    model.metadata.uid = None;
    model.metadata.resource_version = None;
    model.metadata.owner_references = None;
    model.metadata.managed_fields = None;
    let api: Api<ModelDeployment> = Api::namespaced(app.client, &model.namespace().unwrap());
    Ok((
        StatusCode::CREATED,
        Json(api.create(&PostParams::default(), &model).await?),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scale {
    replicas: i32,
}
async fn scale(
    State(app): State<App>,
    Path((ns, name)): Path<(String, String)>,
    body: Result<Json<Scale>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ModelDeployment>, ApiError> {
    let Json(request) = body.map_err(|_| Error::Invalid("invalid JSON body".into()))?;
    validate_name(&ns, true)?;
    validate_name(&name, false)?;
    if request.replicas < 0 {
        return Err(Error::Invalid("replicas must be non-negative".into()).into());
    }
    let api: Api<ModelDeployment> = Api::namespaced(app.client, &ns);
    let mut model = api.get(&name).await?;
    model.spec.replicas = request.replicas;
    Ok(Json(
        api.replace(&name, &PostParams::default(), &model).await?,
    ))
}
async fn remove(
    State(app): State<App>,
    Path((ns, name)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    validate_name(&ns, true)?;
    validate_name(&name, false)?;
    let api: Api<ModelDeployment> = Api::namespaced(app.client, &ns);
    match api.delete(&name, &DeleteParams::default()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    Ok(StatusCode::NO_CONTENT)
}
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _telemetry = xscope_telemetry::init("xscope-cluster-agent")?;
    let app = App {
        client: Client::try_default().await?,
        token: Arc::new(std::env::var("XSCOPE_INTERNAL_TOKEN").unwrap_or_default()),
    };
    let address =
        std::env::var("XSCOPE_CLUSTER_AGENT_ADDRESS").unwrap_or_else(|_| "0.0.0.0:8083".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    tracing::info!(%address,"cluster agent listening");
    axum::serve(listener, router(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request as HttpRequest,
    };
    use std::sync::Mutex;
    use tower::{ServiceExt, service_fn};

    fn app(token: &str) -> Router {
        let stored = Arc::new(Mutex::new(None::<serde_json::Value>));
        let client = Client::new(
            service_fn(move |request: HttpRequest<kube::client::Body>| {
                let stored = stored.clone();
                async move {
                    let method = request.method().clone();
                    let path = request.uri().path().to_owned();
                    assert!(path.starts_with("/apis/platform.xscope.io/v1alpha1/namespaces/xscope-system/modeldeployments"));
                    let bytes = to_bytes(Body::new(request.into_body()), 1 << 20)
                        .await
                        .unwrap();
                    let mut store = stored.lock().unwrap();
                    let value = match method.as_str() {
                        "POST" | "PUT" => {
                            let mut value: serde_json::Value =
                                serde_json::from_slice(&bytes).unwrap();
                            if method == "POST" {
                                assert!(value.get("status").is_none_or(|v| v.is_null()));
                            }
                            if method == "PUT" {
                                assert_eq!(value["metadata"]["resourceVersion"], "1");
                            }
                            value["metadata"]["uid"] = json!("test-uid");
                            value["metadata"]["resourceVersion"] = json!("1");
                            *store = Some(value.clone());
                            value
                        }
                        "DELETE" => {
                            *store = None;
                            json!({"apiVersion":"v1","kind":"Status","status":"Success"})
                        }
                        _ if path.ends_with("/demo") => store.clone().unwrap(),
                        _ => {
                            json!({"apiVersion":"platform.xscope.io/v1alpha1","kind":"ModelDeploymentList","metadata":{},"items":store.clone().into_iter().collect::<Vec<_>>()})
                        }
                    };
                    Ok::<_, std::convert::Infallible>(
                        axum::http::Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from(value.to_string()))
                            .unwrap(),
                    )
                }
            }),
            "xscope-system",
        );
        router(App {
            client,
            token: Arc::new(token.into()),
        })
    }
    fn model() -> serde_json::Value {
        json!({
            "apiVersion":"platform.xscope.io/v1alpha1","kind":"ModelDeployment","metadata":{"name":"demo"},
            "spec":{"model":{"id":"demo","revision":"v1","uri":"s3://demo","checksum":format!("sha256:{}","a".repeat(64))},"runtime":{"image":"example/runtime:v1","protocol":"openai"},"replicas":1,"resources":{"limits":{"cpu":"200m"}}}
        })
    }
    async fn call(app: &Router, method: &str, path: &str, body: serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                HttpRequest::builder()
                    .method(method)
                    .uri(path)
                    .header("authorization", "Bearer test-token")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn preserves_deployment_crud_contract() {
        let app = app("test-token");
        let response = call(&app, "POST", "/v1/model-deployments", model()).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let response = call(&app, "GET", "/v1/model-deployments", json!(null)).await;
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(body["object"], "list");
        assert_eq!(body["data"][0]["spec"]["runtime"]["port"], 8000);
        let response = call(
            &app,
            "PUT",
            "/v1/model-deployments/xscope-system/demo/scale",
            json!({"replicas":3}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1 << 20).await.unwrap())
                .unwrap();
        assert_eq!(body["spec"]["replicas"], 3);
        assert_eq!(
            call(
                &app,
                "DELETE",
                "/v1/model-deployments/xscope-system/demo",
                json!(null)
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
    }
    #[tokio::test]
    async fn rejects_before_kubernetes_access() {
        let app = app("test-token");
        let response = app
            .clone()
            .oneshot(
                HttpRequest::builder()
                    .uri("/v1/model-deployments")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let mut invalid = model();
        invalid["spec"]["model"]["checksum"] = json!("bad");
        assert_eq!(
            call(&app, "POST", "/v1/model-deployments", invalid)
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &app,
                "PUT",
                "/v1/model-deployments/xscope-system/demo/scale",
                json!({"replicas":-1})
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    #[tokio::test]
    async fn missing_internal_token_fails_closed() {
        assert_eq!(
            call(&app(""), "GET", "/v1/model-deployments", json!(null))
                .await
                .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
