use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::prelude::HttpPeer;
use pingora_error::{Error, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_load_balancing::{LoadBalancer, selection::RoundRobin};
use pingora_proxy::{ProxyHttp, Session};
use serde::Deserialize;

use crate::auth::{KeySet, Principal};
use crate::config::UpstreamConfig;
use crate::usage::{CompletionResponse, UsageRecord, UsageSink};

const MAX_CAPTURE_BYTES: usize = 1 << 20;

pub struct Gateway {
    pub upstreams: Arc<LoadBalancer<RoundRobin>>,
    pub endpoint_by_address: HashMap<String, UpstreamConfig>,
    pub keys: KeySet,
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
    response_status: u16,
    usage_emitted: bool,
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
            response_status: 0,
            usage_emitted: false,
        }
    }
}

#[derive(Deserialize)]
struct ModelRequest {
    model: String,
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
                        "data":[{"id":"xscope-demo","object":"model"}]
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
        if let Some(chunk) = body {
            if ctx.request_body.len() + chunk.len() > MAX_CAPTURE_BYTES {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(413),
                    "request body exceeds 1 MiB",
                ));
            }
            ctx.request_body.extend_from_slice(chunk);
        }
        if end_of_stream {
            if let Ok(request) = serde_json::from_slice::<ModelRequest>(&ctx.request_body) {
                ctx.model_id = request.model;
            }
        }
        Ok(())
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        let backend = self
            .upstreams
            .select(ctx.request_id.as_bytes(), 256)
            .ok_or_else(|| {
                Error::explain(ErrorType::HTTPStatus(503), "no healthy model endpoint")
            })?;
        let address = backend.addr.to_string();
        let endpoint = self.endpoint_by_address.get(&address).ok_or_else(|| {
            Error::explain(
                ErrorType::InternalError,
                "selected endpoint has no metadata",
            )
        })?;
        ctx.endpoint_id.clone_from(&endpoint.id);
        Ok(Box::new(HttpPeer::new(
            backend,
            endpoint.tls,
            endpoint.server_name.clone(),
        )))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        request: &mut RequestHeader,
        _ctx: &mut Self::CTX,
    ) -> Result<()> {
        request.remove_header("authorization");
        request.remove_header("accept-encoding");
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> Result<()> {
        ctx.response_status = response.status.as_u16();
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
            if ctx.response_body.len() + chunk.len() <= MAX_CAPTURE_BYTES {
                ctx.response_body.extend_from_slice(chunk);
            }
        }
        if end_of_stream {
            self.emit_usage(ctx, None)?;
        }
        Ok(None)
    }

    async fn logging(&self, _session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX) {
        if !ctx.usage_emitted {
            if let Err(record_error) = self.emit_usage(ctx, error) {
                tracing::error!(
                    error = %record_error,
                    request_id = %ctx.request_id,
                    "usage WAL write failed"
                );
            }
        }
        tracing::info!(
            request_id = %ctx.request_id,
            endpoint = %ctx.endpoint_id,
            status = ctx.response_status,
            latency_ms = ctx.started.elapsed().as_millis(),
            "request completed"
        );
    }
}

impl Gateway {
    fn emit_usage(&self, ctx: &mut RequestContext, error: Option<&Error>) -> Result<()> {
        let Some(principal) = ctx.principal.as_ref() else {
            return Ok(());
        };
        let completion = serde_json::from_slice::<CompletionResponse>(&ctx.response_body).ok();
        if let Some(response) = completion.as_ref() {
            ctx.model_id.clone_from(&response.model);
        }
        let (input_tokens, output_tokens) = completion
            .map(|response| {
                (
                    response.usage.prompt_tokens,
                    response.usage.completion_tokens,
                )
            })
            .unwrap_or_default();
        let status = if error.is_some() || ctx.response_status >= 500 {
            "provider_error"
        } else if ctx.response_status >= 400 {
            "client_error"
        } else {
            "succeeded"
        };
        self.usage
            .record(&UsageRecord {
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
