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
use crate::config::ServingConfig;
use crate::quota::{QuotaDecision, QuotaDenial, QuotaManager, QuotaReservation};
use crate::streaming::StreamMeter;
use crate::usage::{CompletionResponse, CompletionUsage, UsageRecord, UsageSink};

const MAX_CAPTURE_BYTES: usize = 1 << 20;

pub struct Gateway {
    pub serving_transport: Arc<LoadBalancer<RoundRobin>>,
    pub serving: ServingConfig,
    pub keys: Arc<DynamicKeySet>,
    pub quota: Arc<QuotaManager>,
    pub usage: UsageSink,
}

pub struct RequestContext {
    started: Instant,
    request_id: String,
    principal: Option<Principal>,
    endpoint_id: String,
    model_id: String,
    request_body: Vec<u8>,
    response_body: Vec<u8>,
    response_overflow: bool,
    stream_requested: bool,
    stream_response: bool,
    stream_meter: StreamMeter,
    response_complete: bool,
    response_status: u16,
    meterable: bool,
    quota_reservation: Option<QuotaReservation>,
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
            model_id: "unknown".into(),
            request_body: Vec::new(),
            response_body: Vec::new(),
            response_overflow: false,
            stream_requested: false,
            stream_response: false,
            stream_meter: StreamMeter::default(),
            response_complete: false,
            response_status: 0,
            meterable: false,
            quota_reservation: None,
            usage_emitted: false,
            telemetry: None,
        }
    }
}

#[derive(Deserialize)]
struct ModelRequest {
    model: String,
    messages: serde_json::Value,
    max_tokens: Option<u64>,
    max_completion_tokens: Option<u64>,
    #[serde(default)]
    stream: bool,
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
                let (status, body) = if self.keys.is_empty() {
                    (
                        503,
                        serde_json::json!({"error":{"code":"not_ready","message":"no API keys configured"}}),
                    )
                } else if !self
                    .serving_transport
                    .backends()
                    .get_backend()
                    .iter()
                    .any(|backend| self.serving_transport.backends().ready(backend))
                {
                    (
                        503,
                        serde_json::json!({"error":{"code":"not_ready","message":"no healthy serving endpoint"}}),
                    )
                } else {
                    (200, serde_json::json!({"status":"ready"}))
                };
                respond_json(session, status, body, &ctx.request_id).await?;
                return Ok(true);
            }
            ("GET", "/v1/models") => {
                respond_json(
                    session,
                    200,
                    serde_json::json!({
                        "object":"list",
                        "data":[{"id":self.serving.model,"object":"model"}]
                    }),
                    &ctx.request_id,
                )
                .await?;
                return Ok(true);
            }
            ("POST", "/v1/chat/completions") => {}
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
        if !principal.has_scope("chat.completions") {
            respond_json(session, 403, serde_json::json!({
                "error":{"code":"insufficient_scope","message":"API key does not allow chat completions"}
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
        ctx.principal = Some(principal);
        Ok(false)
    }

    async fn request_body_filter(
        &self,
        _session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        if let Some(chunk) = body.take() {
            if ctx.request_body.len() + chunk.len() > MAX_CAPTURE_BYTES {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(413),
                    "request body exceeds 1 MiB",
                ));
            }
            ctx.request_body.extend_from_slice(&chunk);
        }
        // Do not leak partial, unauthorised prompts upstream. Some(empty) is
        // important: Pingora treats None as end-of-body, even on a partial read.
        *body = Some(Bytes::new());
        if end_of_stream {
            let request =
                serde_json::from_slice::<ModelRequest>(&ctx.request_body).map_err(|error| {
                    Error::because(ErrorType::HTTPStatus(400), "invalid request body", error)
                })?;
            let principal = ctx.principal.as_ref().ok_or_else(|| {
                Error::explain(ErrorType::HTTPStatus(401), "request principal is missing")
            })?;
            if !principal.allows_model(&request.model) {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(403),
                    "API key does not allow the requested model",
                ));
            }
            if request.model != self.serving.model {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(404),
                    "no serving pool configured for the requested model",
                ));
            }
            ctx.stream_requested = request.stream;
            if request.stream {
                let mut value: serde_json::Value = serde_json::from_slice(&ctx.request_body)
                    .map_err(|error| {
                        Error::because(ErrorType::HTTPStatus(400), "invalid request body", error)
                    })?;
                let options = value
                    .as_object_mut()
                    .expect("ModelRequest requires an object")
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
            ctx.model_id = request.model;
            ctx.meterable = true;
            *body = Some(Bytes::from(std::mem::take(&mut ctx.request_body)));
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let backend = self
            .serving_transport
            .select(ctx.request_id.as_bytes(), 256)
            .ok_or_else(|| {
                Error::explain(ErrorType::HTTPStatus(503), "no healthy serving entry")
            })?;
        ctx.endpoint_id.clone_from(&self.serving.id);
        let mut peer = HttpPeer::new(backend, self.serving.tls, self.serving.server_name.clone());
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
            xscope_telemetry::background_event("wal_append", "error");
            tracing::error!(
                error = %record_error,
                request_id = %ctx.request_id,
                "usage WAL write failed"
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

impl Gateway {
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
            .record(&UsageRecord {
                trace: ctx.telemetry.as_ref().map(|trace| &trace.span),
                request_id: &ctx.request_id,
                principal,
                endpoint_id: &ctx.endpoint_id,
                model_id: &ctx.model_id,
                input_tokens,
                output_tokens,
                latency_ms: ctx
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
                status,
            })
            .map_err(|cause| {
                Error::because(ErrorType::InternalError, "usage WAL write failed", cause)
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

#[cfg(test)]
mod tests {
    use super::{RequestContext, strip_serving_control_headers};
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
