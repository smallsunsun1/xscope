use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::prelude::HttpPeer;
use pingora_error::{Error, ErrorSource, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{LoadBalancer, selection::RoundRobin};
use pingora_proxy::{ProxyHttp, Session};
use serde::Deserialize;

use crate::auth::{DynamicKeySet, Principal};
use crate::billing::{BillingAdmission, Ticket};
use crate::config::ServingConfig;
use crate::quota::{QuotaDecision, QuotaDenial, QuotaManager, QuotaReservation};
use crate::streaming::StreamMeter;
use crate::usage::{CompletionResponse, CompletionUsage, UsageRecord, UsageSink};

const MAX_CAPTURE_BYTES: usize = 1 << 20;

pub struct Gateway {
    pub transports: HashMap<String, ServingTransport>,
    pub serving: ServingConfig,
    pub keys: Arc<DynamicKeySet>,
    pub quota: Arc<QuotaManager>,
    pub usage: UsageSink,
    pub billing: Option<BillingAdmission>,
}

pub struct ServingTransport {
    pub config: ServingConfig,
    pub transport: Arc<LoadBalancer<RoundRobin>>,
}

pub struct RequestContext {
    started: Instant,
    request_id: String,
    principal: Option<Principal>,
    endpoint_id: String,
    model_revision: String,
    policy_revision: i64,
    selected_model: Option<xscope_domain::Model>,
    selected_endpoint: Option<xscope_domain::catalog::ServingEndpoint>,
    body_sent: bool,
    traffic_permit: Option<crate::traffic::Permit>,
    model_id: String,
    request_body: Vec<u8>,
    response_body: Vec<u8>,
    response_overflow: bool,
    stream_requested: bool,
    text_completion: bool,
    stream_response: bool,
    stream_meter: StreamMeter,
    response_complete: bool,
    response_status: u16,
    meterable: bool,
    quota_reservation: Option<QuotaReservation>,
    billing_ticket: Option<Ticket>,
    usage_permit: Option<Arc<crate::delivery::Permit>>,
    usage_emitted: bool,
    telemetry: Option<xscope_telemetry::RequestTrace>,
}

impl Default for RequestContext {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            request_id: String::new(),
            principal: None,
            endpoint_id: String::new(),
            model_revision: String::new(),
            policy_revision: 0,
            selected_model: None,
            selected_endpoint: None,
            body_sent: false,
            traffic_permit: None,
            model_id: "unknown".into(),
            request_body: Vec::new(),
            response_body: Vec::new(),
            response_overflow: false,
            stream_requested: false,
            text_completion: false,
            stream_response: false,
            stream_meter: StreamMeter::default(),
            response_complete: false,
            response_status: 0,
            meterable: false,
            quota_reservation: None,
            billing_ticket: None,
            usage_permit: None,
            usage_emitted: false,
            telemetry: None,
        }
    }
}

#[derive(Deserialize)]
struct ModelRequest {
    model: String,
    #[serde(default)]
    messages: serde_json::Value,
    prompt: Option<serde_json::Value>,
    max_tokens: Option<u64>,
    max_completion_tokens: Option<u64>,
    n: Option<u64>,
    #[serde(default)]
    stream: bool,
}

fn parse_request(bytes: &[u8], text_completion: bool) -> Result<ModelRequest> {
    let mut request: ModelRequest = serde_json::from_slice(bytes).map_err(|error| {
        Error::because(ErrorType::HTTPStatus(400), "invalid request body", error)
    })?;
    if text_completion {
        if !request.messages.is_null()
            || request.max_completion_tokens.is_some()
            || !request
                .prompt
                .as_ref()
                .is_some_and(|p| p.as_str().is_some_and(|s| !s.is_empty()))
        {
            return Err(Error::explain(
                ErrorType::HTTPStatus(400),
                "text completions require one nonempty string prompt and max_tokens",
            ));
        }
        request.messages = request
            .prompt
            .take()
            .ok_or_else(|| Error::explain(ErrorType::HTTPStatus(400), "missing prompt"))?;
    } else if request.prompt.is_some() || !request.messages.is_array() {
        return Err(Error::explain(
            ErrorType::HTTPStatus(400),
            "chat completions require messages array",
        ));
    }
    Ok(request)
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = RequestContext;

    fn new_ctx(&self) -> Self::CTX {
        RequestContext::default()
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> {
        ctx.request_id = request_id(session);
        let method = session.req_header().method.as_str();
        let path = session.req_header().uri.path();
        if !matches!(path, "/healthz" | "/readyz") {
            let route = match path {
                "/v1/chat/completions" => "/v1/chat/completions",
                "/v1/completions" => "/v1/completions",
                "/v1/models" => "/v1/models",
                _ => "unmatched",
            };
            ctx.telemetry = Some(xscope_telemetry::RequestTrace::start(
                &session.req_header().headers,
                method,
                route,
            ));
        }

        match (method, path) {
            ("GET", "/healthz") => {
                respond_json(
                    session,
                    200,
                    serde_json::json!({"status":"ok","component":"gateway"}),
                    &ctx.request_id,
                )
                .await?;
                return Ok(true);
            }
            ("GET", "/readyz") => {
                let (status, body) = if !self.delivery_healthy() {
                    (
                        503,
                        serde_json::json!({"error":{"code":"not_ready","message":"usage reporting unavailable"}}),
                    )
                } else if self.keys.is_empty() {
                    (
                        503,
                        serde_json::json!({"error":{"code":"not_ready","message":"no API keys configured"}}),
                    )
                } else if !self.keys.has_catalog()
                    && !self.transports.get(&self.serving.id).is_some_and(|entry| {
                        entry
                            .transport
                            .backends()
                            .get_backend()
                            .iter()
                            .any(|backend| entry.transport.backends().ready(backend))
                    })
                {
                    (
                        503,
                        serde_json::json!({"error":{"code":"not_ready","message":"no healthy serving endpoint"}}),
                    )
                } else {
                    // Catalog-mode readiness is Gateway readiness. One model's
                    // outage must not remove every other model from K8s Service.
                    (200, serde_json::json!({"status":"ready"}))
                };
                respond_json(session, status, body, &ctx.request_id).await?;
                return Ok(true);
            }
            ("GET", "/v1/models") => {
                let principal = self.keys.authenticate(
                    session
                        .req_header()
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok()),
                );
                let Some(principal) = principal else {
                    respond_json(
                        session,
                        401,
                        serde_json::json!({"error":{"code":"invalid_api_key"}}),
                        &ctx.request_id,
                    )
                    .await?;
                    return Ok(true);
                };
                let models: Vec<_> = if let Some(catalog) = &principal.catalog {
                    catalog
                        .models
                        .iter()
                        .filter(|m| m.enabled && principal.allows_model(&m.model.id))
                        .map(|m| serde_json::json!({"id":m.model.id,"object":"model"}))
                        .collect()
                } else {
                    let ids: std::collections::BTreeSet<_> = self
                        .transports
                        .values()
                        .filter(|t| principal.allows_model(&t.config.model))
                        .map(|t| &t.config.model)
                        .collect();
                    ids.into_iter()
                        .map(|id| serde_json::json!({"id":id,"object":"model"}))
                        .collect()
                };
                respond_json(
                    session,
                    200,
                    serde_json::json!({"object":"list","data":models}),
                    &ctx.request_id,
                )
                .await?;
                return Ok(true);
            }
            ("POST", "/v1/chat/completions") => {}
            ("POST", "/v1/completions") => ctx.text_completion = true,
            _ => {
                respond_json(
                    session,
                    404,
                    serde_json::json!({"error":{"code":"not_found","message":"route not found"}}),
                    &ctx.request_id,
                )
                .await?;
                return Ok(true);
            }
        }

        if !self.delivery_healthy() {
            respond_json(session, 503, serde_json::json!({"error":{"code":"usage_delivery_unavailable","message":"usage delivery is full, stale or unhealthy"}}), &ctx.request_id).await?;
            return Ok(true);
        }
        let authorization = session
            .req_header()
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok());
        let Some(principal) = self.keys.authenticate(authorization) else {
            respond_json(session, 401, serde_json::json!({
                "error":{"code":"invalid_api_key","message":"a valid Bearer API key is required"}
            }), &ctx.request_id).await?;
            return Ok(true);
        };
        let scope = if ctx.text_completion {
            "completions"
        } else {
            "chat.completions"
        };
        if !principal.has_scope(scope) {
            respond_json(session, 403, serde_json::json!({
                "error":{"code":"insufficient_scope","message":"API key does not allow this inference API"}
            }), &ctx.request_id).await?;
            return Ok(true);
        }
        if principal.budget_exhausted() {
            respond_json(session, 402, serde_json::json!({
                "error":{"code":"budget_exhausted","message":"API key monthly budget is exhausted"}
            }), &ctx.request_id).await?;
            return Ok(true);
        }
        if principal.funds_exhausted() {
            respond_json(session, 402, serde_json::json!({
                "error":{"code":"insufficient_balance","message":"the project's prepaid balance is exhausted"}
            }), &ctx.request_id).await?;
            return Ok(true);
        }
        if !self.quota.is_distributed() && !self.keys.check_rate_limit(&principal) {
            respond_json(session, 429, serde_json::json!({
                "error":{"code":"rate_limit_exceeded","message":"API key request-per-minute limit exceeded"}
            }), &ctx.request_id).await?;
            return Ok(true);
        }
        match self.usage.reserve() {
            Ok(permit) => ctx.usage_permit = Some(permit),
            Err(_) => {
                respond_json(session, 503, serde_json::json!({"error":{"code":"usage_reporting_unavailable","message":"usage reporting capacity unavailable"}}), &ctx.request_id).await?;
                return Ok(true);
            }
        }
        // Read the complete bounded JSON before choosing a peer. Pingora's
        // request buffer supplies the later body hook; POST retries remain off.
        session.as_mut().enable_retry_buffering();
        tokio::time::timeout(Duration::from_secs(15), async {
            while let Some(chunk) = session.read_request_body().await? {
                if ctx.request_body.len() + chunk.len() > MAX_CAPTURE_BYTES {
                    return Err(Error::explain(
                        ErrorType::HTTPStatus(413),
                        "request body exceeds 1 MiB",
                    ));
                }
                ctx.request_body.extend_from_slice(&chunk);
            }
            Ok(())
        })
        .await
        .map_err(|_| {
            Error::explain(ErrorType::HTTPStatus(408), "request body deadline exceeded")
        })??;
        if session.as_ref().retry_buffer_truncated() {
            return Err(Error::explain(
                ErrorType::HTTPStatus(413),
                "request exceeds transport buffer",
            ));
        }
        let request = parse_request(&ctx.request_body, ctx.text_completion)?;
        if !principal.allows_model(&request.model) {
            return Err(Error::explain(
                ErrorType::HTTPStatus(403),
                "API key does not allow the requested model",
            ));
        }
        let default_pool = if let Some(catalog) = &principal.catalog {
            let entry = catalog
                .models
                .iter()
                .find(|m| m.model.id == request.model && m.enabled)
                .ok_or_else(|| {
                    Error::explain(ErrorType::HTTPStatus(404), "model is unknown or disabled")
                })?;
            ctx.selected_model = Some(entry.model.clone());
            entry.default_pool.as_deref()
        } else if self.serving.model == request.model {
            Some(self.serving.id.as_str())
        } else {
            let mut entries = self
                .transports
                .values()
                .filter(|t| t.config.model == request.model);
            let first = entries.next();
            if entries.next().is_some() {
                None
            } else {
                first.map(|t| t.config.id.as_str())
            }
        };
        let (pool_id, reason) = if let Some(policy) = principal.route_policy(&request.model) {
            ctx.policy_revision = policy.revision;
            let roll = ((uuid::Uuid::now_v7().as_u128() & u128::from(u64::MAX)) % 100) as u8;
            policy.spec.select(
                |rule| header_matches(&session.req_header().headers, rule),
                roll,
            )
        } else {
            (
                default_pool.ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(503),
                        "model has no default serving pool",
                    )
                })?,
                "default",
            )
        };
        if let Some(endpoint) = principal
            .catalog
            .as_ref()
            .and_then(|c| c.endpoints.iter().find(|e| e.id == pool_id))
        {
            if endpoint.model != request.model {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(503),
                    "pool model does not match request",
                ));
            }
            ctx.model_revision.clone_from(&endpoint.revision);
            ctx.selected_endpoint = Some(endpoint.clone());
        } else {
            let pool = self
                .transports
                .get(pool_id)
                .filter(|p| p.config.model == request.model)
                .ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(503),
                        "selected model pool is unavailable",
                    )
                })?;
            ctx.model_revision.clone_from(&pool.config.revision);
        }
        ctx.endpoint_id = pool_id.to_owned();
        if xscope_domain::traffic::managed(pool_id) {
            let generation = principal
                .traffic
                .as_ref()
                .and_then(|s| s.pools.iter().find(|p| p.id == pool_id))
                .map(|p| p.generation)
                .ok_or_else(|| {
                    Error::explain(
                        ErrorType::HTTPStatus(503),
                        "managed pool requires a traffic grant",
                    )
                })?;
            ctx.traffic_permit = Some(
                self.keys
                    .traffic
                    .enter(pool_id, generation)
                    .map_err(|reason| Error::explain(ErrorType::HTTPStatus(503), reason))?,
            );
        }
        xscope_telemetry::route_selected(pool_id, reason);
        if let Some(trace) = &ctx.telemetry {
            trace.selected_pool(pool_id, &ctx.model_revision, ctx.policy_revision);
        }
        tracing::info!(parent: ctx.telemetry.as_ref().and_then(|trace| trace.span.id()),
            request_id = %ctx.request_id, pool = pool_id, policy_revision = ctx.policy_revision,
            model_revision = %ctx.model_revision, reason, "model pool selected");
        ctx.principal = Some(principal);
        self.admit_request(ctx).await?;
        Ok(false)
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if !end_of_stream || ctx.body_sent || !ctx.meterable {
            return Err(Error::explain(
                ErrorType::InternalError,
                "unexpected admitted request body state",
            ));
        }
        // Ignore Pingora's original replay buffer: admission may have rewritten
        // output bounds and stream_options. Forward the prepared body once.
        *body = Some(Bytes::from(std::mem::take(&mut ctx.request_body)));
        ctx.body_sent = true;
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let mut peer = if let Some(endpoint) = &ctx.selected_endpoint {
            // Resolve the registered Envoy entry, never enumerate model Pods.
            let mut addresses = tokio::time::timeout(
                Duration::from_secs(3),
                tokio::net::lookup_host(endpoint.address.as_str()),
            )
            .await
            .map_err(|_| {
                Error::explain(ErrorType::HTTPStatus(503), "serving DNS deadline exceeded")
            })?
            .map_err(|_| Error::explain(ErrorType::HTTPStatus(503), "serving DNS unavailable"))?;
            let address = addresses.next().ok_or_else(|| {
                Error::explain(
                    ErrorType::HTTPStatus(503),
                    "serving DNS returned no addresses",
                )
            })?;
            HttpPeer::new(address, endpoint.tls, endpoint.server_name.clone())
        } else {
            let entry = self.transports.get(&ctx.endpoint_id).ok_or_else(|| {
                Error::explain(
                    ErrorType::HTTPStatus(503),
                    "selected serving pool is unavailable",
                )
            })?;
            let backend = entry
                .transport
                .select(ctx.request_id.as_bytes(), 256)
                .ok_or_else(|| {
                    Error::explain(ErrorType::HTTPStatus(503), "no healthy serving entry")
                })?;
            HttpPeer::new(backend, entry.config.tls, entry.config.server_name.clone())
        };
        // A stale service address must not leave admission requests hanging on
        // the OS TCP timeout. These are connection/idle limits, not an SSE
        // total-duration deadline.
        peer.options.connection_timeout = Some(Duration::from_secs(3));
        peer.options.total_connection_timeout = Some(Duration::from_secs(5));
        peer.options.read_timeout = Some(Duration::from_secs(300));
        peer.options.write_timeout = Some(Duration::from_secs(30));
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        request: &mut RequestHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        strip_serving_control_headers(request);
        if ctx.traffic_permit.is_some() {
            request.insert_header("x-xscope-managed-pool", &ctx.endpoint_id)?;
        }
        request.insert_header("x-request-id", &ctx.request_id)?;
        if let Some(trace) = &ctx.telemetry {
            // Pingora maintains a parallel header-case map. All mutations must
            // use RequestHeader APIs, never write its HeaderMap directly.
            for name in ["traceparent", "tracestate", "baggage"] {
                request.remove_header(name);
            }
            let mut headers = http::HeaderMap::new();
            xscope_telemetry::inject(&trace.span, &mut headers);
            for (name, value) in &headers {
                request.insert_header(name, value)?;
            }
        }
        // The complete body is released only after admission, and streaming
        // requests may gain stream_options.include_usage. Preserve framing.
        request.remove_header("content-length");
        if request.version != http::Version::HTTP_2 {
            request.insert_header("transfer-encoding", "chunked")?;
        }
        Ok(())
    }

    fn error_while_proxy(
        &self,
        _peer: &HttpPeer,
        _session: &mut Session,
        mut error: Box<Error>,
        _ctx: &mut Self::CTX,
        _client_reused: bool,
    ) -> Box<Error> {
        // A completion is not idempotent. Once connected, do not replay it on
        // another runtime (or reserve quota twice), even on a reused connection.
        error.set_retry(false);
        error
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.response_status = response.status.as_u16();
        if let Some(ticket) = &ctx.billing_ticket {
            response.insert_header("x-xscope-billing-request-id", &ticket.id)?;
        }
        response.insert_header("x-xscope-pool", &ctx.endpoint_id)?;
        response.insert_header("x-xscope-model-revision", &ctx.model_revision)?;
        response.insert_header("x-xscope-route-revision", ctx.policy_revision.to_string())?;
        if let Some(trace) = &ctx.telemetry {
            response.insert_header("x-trace-id", trace.trace_id())?;
        }
        ctx.stream_response = response
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/event-stream"))
            });
        if ctx.stream_response {
            response.insert_header("cache-control", "no-cache")?;
            response.insert_header("x-accel-buffering", "no")?;
        }
        response
            .insert_header("x-request-id", &ctx.request_id)
            .map_err(|error| {
                Error::because(ErrorType::InternalError, "invalid request id", error)
            })?;
        Ok(())
    }

    fn upstream_response_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<Option<Duration>> {
        if let Some(chunk) = body {
            if !chunk.is_empty()
                && let Some(trace) = &mut ctx.telemetry
            {
                trace.observe_first_byte();
            }
            if ctx.stream_response {
                ctx.stream_meter
                    .observe(chunk)
                    .map_err(|cause| Error::explain(ErrorType::HTTPStatus(502), cause))?;
            } else if !ctx.response_overflow {
                if ctx.response_body.len() + chunk.len() <= MAX_CAPTURE_BYTES {
                    ctx.response_body.extend_from_slice(chunk);
                } else {
                    ctx.response_overflow = true;
                    ctx.response_body.clear();
                }
            }
        }
        ctx.response_complete |= end_of_stream;
        Ok(None)
    }

    async fn logging(&self, session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        if !ctx.usage_emitted
            && let Err(record_error) = self.emit_usage(ctx, error)
        {
            xscope_telemetry::background_event("usage_enqueue", "error");
            tracing::error!(
                error = %record_error,
                request_id = %ctx.request_id,
                "usage enqueue failed; central reservation requires review"
            );
        }
        if let Some(reservation) = ctx.quota_reservation.take() {
            if let Some(usage) = ctx.actual_usage() {
                let actual_tokens = usage.prompt_tokens.saturating_add(usage.completion_tokens);
                if let Err(quota_error) = self.quota.settle(reservation, actual_tokens).await {
                    xscope_telemetry::background_event("quota_settle", "error");
                    tracing::warn!(
                        error = %quota_error,
                        request_id = %ctx.request_id,
                        "could not settle distributed token reservation"
                    );
                }
            } else {
                // Unknown usage is not zero: keep the TPM reservation until its
                // minute window expires, including cancelled provider requests.
                tracing::warn!(request_id = %ctx.request_id, "usage unknown; retaining token quota reservation");
            }
        }
        let status = session
            .response_written()
            .map_or(ctx.response_status, |response| response.status.as_u16());
        let outcome = if ctx.meterable {
            ctx.outcome(error)
        } else if status >= 500 {
            "provider_error"
        } else if status >= 400 {
            "rejected"
        } else {
            "succeeded"
        };
        if let Some(usage) = ctx.actual_usage() {
            xscope_telemetry::tokens(usage.prompt_tokens, usage.completion_tokens);
        }
        if let Some(mut trace) = ctx.telemetry.take() {
            trace.finish(status, outcome);
        }
        tracing::debug!(
            request_id = %ctx.request_id,
            endpoint = %ctx.endpoint_id,
            status = ctx.response_status,
            latency_ms = ctx.started.elapsed().as_millis(),
            "request completed"
        );
    }
}

fn header_matches(headers: &http::HeaderMap, rule: &xscope_domain::routing::HeaderRoute) -> bool {
    let mut values = headers.get_all(&rule.name).iter();
    values
        .next()
        .is_some_and(|value| value.as_bytes() == rule.value.as_bytes())
        && values.next().is_none()
}

fn strip_serving_control_headers(request: &mut RequestHeader) {
    // Envoy ORIGINAL_DST trusts EPP's destination header. None of the client's
    // routing, retry or timeout controls may cross this trust boundary.
    let untrusted: Vec<_> = request
        .headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            name.starts_with("x-envoy-")
                || name.starts_with("x-gateway-")
                || name.starts_with("x-inference-")
                || name.starts_with("x-ai-eg-")
                || name.starts_with("x-xscope-")
                || name.starts_with("x-route-")
        })
        .cloned()
        .collect();
    for name in untrusted {
        request.remove_header(&name);
    }
    request.remove_header("authorization");
    request.remove_header("accept-encoding");
}

fn estimate_reserved_tokens(request: &ModelRequest) -> u64 {
    let message_text = serde_json::to_string(&request.messages).unwrap_or_default();
    let prompt_tokens = tiktoken_rs::cl100k_base_singleton()
        .encode_ordinary(&message_text)
        .len()
        .try_into()
        .unwrap_or(u64::MAX);
    let completion_tokens = request
        .max_completion_tokens
        .or(request.max_tokens)
        .unwrap_or(1_024);
    prompt_tokens
        .saturating_add(completion_tokens)
        .min(10_000_000_001)
}

fn money_token_limits(request: &ModelRequest, context: i64) -> Result<(i64, i64)> {
    let output = request
        .max_completion_tokens
        .or(request.max_tokens)
        .unwrap_or(1024);
    if request.max_completion_tokens.is_some() && request.max_tokens.is_some()
        || request.n.is_some_and(|n| n != 1)
        || output == 0
        || output >= context as u64
        || estimate_reserved_tokens(request) > context as u64
    {
        return Err(Error::explain(
            ErrorType::HTTPStatus(400),
            "ambiguous or excessive model token bounds",
        ));
    }
    let output = i64::try_from(output).map_err(|error| {
        Error::because(
            ErrorType::HTTPStatus(400),
            "completion token bound is too large",
            error,
        )
    })?;
    Ok((context - output, output))
}

impl Gateway {
    async fn admit_request(&self, ctx: &mut RequestContext) -> Result<()> {
        let request = parse_request(&ctx.request_body, ctx.text_completion)?;
        let principal = ctx.principal.as_ref().ok_or_else(|| {
            Error::explain(ErrorType::HTTPStatus(401), "request principal is missing")
        })?;
        if !principal.allows_model(&request.model) {
            return Err(Error::explain(
                ErrorType::HTTPStatus(403),
                "API key does not allow the requested model",
            ));
        }
        ctx.stream_requested = request.stream;
        let limits = if let Some(billing) = &self.billing {
            let limits = money_token_limits(
                &request,
                ctx.selected_model
                    .as_ref()
                    .map_or(billing.context_tokens, |m| {
                        m.max_context_tokens.min(billing.context_tokens)
                    }),
            )?;
            // Enforce a finite completion bound at the provider as well as
            // in the financial contract. Never trust a tokenizer estimate
            // as the monetary input ceiling: reserve the remaining context.
            let mut value: serde_json::Value =
                serde_json::from_slice(&ctx.request_body).map_err(|error| {
                    Error::because(
                        ErrorType::InternalError,
                        "cannot decode validated inference request",
                        error,
                    )
                })?;
            let object = value.as_object_mut().ok_or_else(|| {
                Error::explain(
                    ErrorType::InternalError,
                    "validated inference request is not an object",
                )
            })?;
            object.remove("max_tokens");
            object.remove("max_completion_tokens");
            let name = if request.max_completion_tokens.is_some() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            object.insert(name.into(), limits.1.into());
            ctx.request_body = serde_json::to_vec(&value).map_err(|error| {
                Error::because(
                    ErrorType::InternalError,
                    "cannot encode bounded inference request",
                    error,
                )
            })?;
            Some(limits)
        } else {
            None
        };
        if request.stream {
            let mut value: serde_json::Value =
                serde_json::from_slice(&ctx.request_body).map_err(|error| {
                    Error::because(ErrorType::HTTPStatus(400), "invalid request body", error)
                })?;
            let options = value
                .as_object_mut()
                .ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "validated inference request is not an object",
                    )
                })?
                .entry("stream_options")
                .or_insert_with(|| serde_json::json!({}));
            if options.is_null() {
                *options = serde_json::json!({});
            }
            let options = options.as_object_mut().ok_or_else(|| {
                Error::explain(
                    ErrorType::HTTPStatus(400),
                    "stream_options must be an object",
                )
            })?;
            options.insert("include_usage".into(), true.into());
            ctx.request_body = serde_json::to_vec(&value).map_err(|error| {
                Error::because(
                    ErrorType::InternalError,
                    "cannot encode inference request",
                    error,
                )
            })?;
        }
        let requested_tokens = estimate_reserved_tokens(&request);
        match self.quota.reserve(principal, requested_tokens).await {
            Ok(Some(QuotaDecision::Allowed(reservation))) => {
                ctx.quota_reservation = Some(reservation);
            }
            Ok(Some(QuotaDecision::Denied(QuotaDenial::RequestsPerMinute))) => {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(429),
                    "API key distributed RPM limit exceeded",
                ));
            }
            Ok(Some(QuotaDecision::Denied(QuotaDenial::TokensPerMinute))) => {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(429),
                    "API key distributed TPM limit exceeded",
                ));
            }
            Ok(None) => {}
            Err(error) => {
                tracing::error!(%error, request_id = %ctx.request_id, "distributed quota unavailable");
                return Err(Error::explain(
                    ErrorType::HTTPStatus(503),
                    "distributed quota service unavailable",
                ));
            }
        }
        if let (Some(billing), Some((input_token_limit, output_token_limit))) =
            (&self.billing, limits)
        {
            let mut trace_headers = http::HeaderMap::new();
            if let Some(trace) = &ctx.telemetry {
                xscope_telemetry::inject(&trace.span, &mut trace_headers);
            }
            // Client X-Request-Id is correlation, never financial idempotency.
            let id = format!("req-{}", uuid::Uuid::now_v7());
            let reservation = xscope_domain::billing::ReserveRequest {
                id: id.clone(),
                request_id: id,
                tenant_id: principal.tenant_id.clone(),
                project_id: principal.project_id.clone(),
                api_key_id: principal.api_key_id.clone(),
                model_id: request.model.clone(),
                model_revision: ctx.model_revision.clone(),
                price_version: ctx.selected_model.as_ref().map_or_else(
                    || billing.price_version.clone(),
                    |m| m.price_version.clone(),
                ),
                input_token_limit,
                output_token_limit,
            };
            ctx.billing_ticket = Some(
                billing
                    .admit(
                        reservation,
                        trace_headers,
                        ctx.usage_permit
                            .as_ref()
                            .ok_or_else(|| {
                                Error::explain(
                                    ErrorType::InternalError,
                                    "billing admission has no usage queue permit",
                                )
                            })?
                            .clone(),
                    )
                    .await
                    .map_err(|status| {
                        let public_status = if matches!(status, 400 | 402 | 403 | 422) {
                            status
                        } else {
                            503
                        };
                        Error::explain(
                            ErrorType::HTTPStatus(public_status),
                            "financial admission rejected or unavailable",
                        )
                    })?,
            );
        }
        ctx.model_id = request.model;
        ctx.meterable = true;

        Ok(())
    }

    fn delivery_healthy(&self) -> bool {
        self.usage.is_healthy()
            && self
                .billing
                .as_ref()
                .is_none_or(BillingAdmission::is_healthy)
    }

    fn emit_usage(&self, ctx: &mut RequestContext, error: Option<&Error>) -> Result<()> {
        if !ctx.meterable {
            return Ok(());
        }
        let Some(principal) = ctx.principal.as_ref() else {
            return Ok(());
        };
        let (input_tokens, output_tokens) = ctx
            .actual_usage()
            .map(|usage| (usage.prompt_tokens, usage.completion_tokens))
            .unwrap_or_default();
        let status = ctx.outcome(error);
        self.usage
            .record(
                &UsageRecord {
                    trace: ctx.telemetry.as_ref().map(|trace| &trace.span),
                    request_id: &ctx.request_id,
                    principal,
                    endpoint_id: &ctx.endpoint_id,
                    model_id: &ctx.model_id,
                    model_revision: &ctx.model_revision,
                    price_version: ctx
                        .selected_model
                        .as_ref()
                        .map(|m| m.price_version.as_str()),
                    input_tokens,
                    output_tokens,
                    latency_ms: ctx
                        .started
                        .elapsed()
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                    status,
                    billing: ctx.billing_ticket.as_ref(),
                    usage_known: ctx.actual_usage().is_some(),
                },
                ctx.usage_permit.as_deref().ok_or_else(|| {
                    Error::explain(ErrorType::InternalError, "missing usage permit")
                })?,
            )
            .map_err(|cause| {
                Error::because(ErrorType::InternalError, "usage enqueue failed", cause)
            })?;
        ctx.usage_emitted = true;
        Ok(())
    }
}

impl RequestContext {
    fn outcome(&self, error: Option<&Error>) -> &'static str {
        if error.is_some_and(|error| error.esource() == &ErrorSource::Downstream) {
            "cancelled"
        } else if error.is_some() || self.response_status >= 500 || self.stream_meter.invalid {
            "provider_error"
        } else if self.response_status >= 400 {
            "client_error"
        } else if !self.response_complete
            || (self.stream_requested && (!self.stream_response || !self.stream_meter.done))
        {
            "provider_error"
        } else if self.actual_usage().is_none() {
            "usage_pending"
        } else {
            "succeeded"
        }
    }
    fn actual_usage(&self) -> Option<CompletionUsage> {
        if self.stream_response {
            self.stream_meter.usage.clone()
        } else if self.response_overflow {
            None
        } else {
            serde_json::from_slice::<CompletionResponse>(&self.response_body)
                .ok()
                .map(|response| response.usage)
        }
    }
}

fn request_id(session: &Session) -> String {
    session
        .req_header()
        .headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .map_or_else(|| format!("req-{}", uuid::Uuid::now_v7()), str::to_owned)
}

async fn respond_json(
    session: &mut Session,
    status: u16,
    value: serde_json::Value,
    request_id: &str,
) -> Result<()> {
    let body = Bytes::from(serde_json::to_vec(&value).map_err(|error| {
        Error::because(
            ErrorType::InternalError,
            "could not encode JSON response",
            error,
        )
    })?);
    let mut header = ResponseHeader::build(status, Some(4))?;
    header.insert_header("content-type", "application/json")?;
    header.insert_header("content-length", body.len().to_string())?;
    header.insert_header("x-request-id", request_id)?;
    session
        .write_response_header(Box::new(header), false)
        .await?;
    session.write_response_body(Some(body), true).await
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::{RequestContext, header_matches, strip_serving_control_headers};
    use pingora_http::RequestHeader;

    #[test]
    fn client_cannot_override_epp_destination_retries_or_objective() {
        let mut request = RequestHeader::build("POST", b"/v1/chat/completions", None).unwrap();
        for name in [
            "x-gateway-destination-endpoint",
            "x-envoy-original-dst-host",
            "x-envoy-retry-on",
            "x-inference-objective",
            "authorization",
            "accept-encoding",
            "x-route-cohort",
            "x-xscope-pool",
        ] {
            request.insert_header(name, "untrusted").unwrap();
        }
        request.insert_header("x-canary", "preview").unwrap();
        request.insert_header("x-request-id", "trace-id").unwrap();
        strip_serving_control_headers(&mut request);
        assert_eq!(request.headers.len(), 2);
        assert_eq!(request.headers["x-canary"], "preview");
        assert_eq!(request.headers["x-request-id"], "trace-id");
    }

    #[test]
    fn cohort_names_are_case_insensitive_but_values_are_exact_and_unambiguous() {
        let rule = xscope_domain::routing::HeaderRoute {
            name: "x-route-cohort".into(),
            value: "qa".into(),
            target: xscope_domain::routing::RouteTarget::Canary,
        };
        let mut headers = http::HeaderMap::new();
        headers.insert("X-Route-Cohort", http::HeaderValue::from_static("qa"));
        assert!(header_matches(&headers, &rule));
        headers.append("x-route-cohort", http::HeaderValue::from_static("qa"));
        assert!(!header_matches(&headers, &rule));
        headers.insert("x-route-cohort", http::HeaderValue::from_static("QA"));
        assert!(!header_matches(&headers, &rule));
    }

    #[test]
    fn missing_usage_is_pending_but_missing_done_is_provider_error() {
        let mut ctx = RequestContext {
            stream_requested: true,
            stream_response: true,
            response_status: 200,
            response_complete: true,
            ..RequestContext::default()
        };
        assert_eq!(ctx.outcome(None), "provider_error");
        ctx.stream_meter.observe(b"data: [DONE]\n\n").unwrap();
        assert_eq!(ctx.outcome(None), "usage_pending");
    }
}
#[test]
fn text_completion_contract_is_separate_and_financially_bounded() -> Result<()> {
    let body = br#"{"model":"synthetic","prompt":"hello","max_tokens":4,"stream":true}"#;
    let request = parse_request(body, true)?;
    assert_eq!(money_token_limits(&request, 700)?, (696, 4));
    assert!(request.stream);
    assert!(parse_request(body, false).is_err());
    assert!(
        parse_request(
            br#"{"model":"synthetic","messages":[],"max_tokens":4}"#,
            true
        )
        .is_err()
    );
    assert!(parse_request(br#"{"model":"synthetic","prompt":["a","b"]}"#, true).is_err());
    assert!(
        parse_request(
            br#"{"model":"synthetic","prompt":"x","max_completion_tokens":4}"#,
            true
        )
        .is_err()
    );
    Ok(())
}
