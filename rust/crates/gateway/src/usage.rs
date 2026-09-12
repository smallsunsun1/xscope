use std::io;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::billing::Ticket;
use crate::delivery::{Permit, Queue};

pub struct UsageSink {
    queue: Arc<Queue>,
    region: String,
    model_revision: String,
    price_version: String,
}

pub(crate) struct UsageRecord<'a> {
    pub price_version: Option<&'a str>,
    pub trace: Option<&'a tracing::Span>,
    pub request_id: &'a str,
    pub principal: &'a Principal,
    pub endpoint_id: &'a str,
    pub model_id: &'a str,
    pub model_revision: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub status: &'a str,
    pub billing: Option<&'a Ticket>,
    pub usage_known: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) billing: Option<Ticket>,
    pub usage_known: bool,
    // Injected into the HTTP request, never persisted in a local journal/body.
    #[serde(skip)]
    pub trace_headers: http::HeaderMap,
    pub schema_version: String,
    pub event_id: String,
    pub request_id: String,
    pub occurred_at: DateTime<Utc>,
    pub tenant_id: String,
    pub project_id: String,
    pub api_key_id: String,
    pub model_id: String,
    pub model_revision: String,
    pub endpoint_id: String,
    pub region: String,
    pub price_version: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub latency_ms: u64,
    pub status: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct CompletionResponse {
    pub usage: CompletionUsage,
}

impl UsageSink {
    pub fn new(
        queue: Arc<Queue>,
        region: String,
        model_revision: String,
        price_version: String,
    ) -> Self {
        Self {
            queue,
            region,
            model_revision,
            price_version,
        }
    }

    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.queue.is_ready()
    }

    pub(crate) fn reserve(&self) -> io::Result<Arc<Permit>> {
        self.queue.reserve().map(Arc::new)
    }

    /// Enqueues one immutable usage event using the admitted request's capacity.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded HTTP queue cannot accept the event.
    pub(crate) fn record(&self, record: &UsageRecord<'_>, permit: &Permit) -> io::Result<()> {
        let mut trace_headers = http::HeaderMap::new();
        if let Some(span) = record.trace {
            xscope_telemetry::inject(span, &mut trace_headers);
        }
        let event = UsageEvent {
            trace_headers,
            schema_version: if record.billing.is_some() { "v2" } else { "v1" }.to_owned(),
            billing: record.billing.cloned(),
            usage_known: record.usage_known,
            event_id: format!("evt-{}", uuid::Uuid::now_v7()),
            request_id: record.request_id.to_owned(),
            occurred_at: Utc::now(),
            tenant_id: record.principal.tenant_id.clone(),
            project_id: record.principal.project_id.clone(),
            api_key_id: record.principal.api_key_id.clone(),
            model_id: record.model_id.to_owned(),
            model_revision: if record.model_revision.is_empty() {
                self.model_revision.clone()
            } else {
                record.model_revision.to_owned()
            },
            endpoint_id: record.endpoint_id.to_owned(),
            region: self.region.clone(),
            price_version: record
                .price_version
                .unwrap_or(&self.price_version)
                .to_owned(),
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cached_input_tokens: 0,
            latency_ms: record.latency_ms,
            status: record.status.to_owned(),
        };
        self.queue.submit(permit, event)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum DeliveryError {
    #[error("retryable usage HTTP status {0}")]
    Retry(u16),
    #[error("permanent usage HTTP status {0}")]
    Permanent(u16),
}

fn http_failure(error: reqwest::Error) -> DeliveryError {
    match error.status().map(|s| s.as_u16()) {
        None => DeliveryError::Retry(0),
        Some(status @ (408 | 425 | 429 | 500..=599)) => DeliveryError::Retry(status),
        Some(status) => DeliveryError::Permanent(status),
    }
}

pub(crate) fn deliver(
    client: &reqwest::blocking::Client,
    report_url: &str,
    token: &str,
    event: &UsageEvent,
) -> Result<(), DeliveryError> {
    let request = match (event.schema_version.as_str(), &event.billing) {
        ("v1", None) => client.post(report_url).json(event),
        ("v2", Some(ticket)) => {
            let base = report_url
                .strip_suffix("/usage-events")
                .ok_or(DeliveryError::Permanent(0))?;
            let url = format!(
                "{base}/billing/projects/{}/reservations/{}",
                ticket.project_id, ticket.id
            );
            if event.usage_known
                && event.input_tokens <= ticket.input_token_limit as u64
                && event.output_tokens <= ticket.output_token_limit as u64
            {
                let settlement = xscope_domain::billing::SettleRequest {
                    input_tokens: i64::try_from(event.input_tokens)
                        .map_err(|_| DeliveryError::Permanent(400))?,
                    output_tokens: i64::try_from(event.output_tokens)
                        .map_err(|_| DeliveryError::Permanent(400))?,
                    latency_ms: i64::try_from(event.latency_ms)
                        .map_err(|_| DeliveryError::Permanent(400))?,
                    endpoint_id: event.endpoint_id.clone(),
                    region: event.region.clone(),
                    status: match event.status.as_str() {
                        "succeeded" => "succeeded",
                        "cancelled" => "cancelled",
                        _ => "provider_error",
                    }
                    .into(),
                };
                client.post(format!("{url}/settle")).json(&settlement)
            } else {
                // Store the unresolved outcome centrally, never fabricate zero usage.
                client.post(format!("{url}/unresolved")).json(
                    &xscope_domain::billing::UnresolvedRequest {
                        event_id: event.event_id.clone(),
                        reason: if event.usage_known {
                            "usage_over_limit"
                        } else {
                            "usage_missing"
                        }
                        .into(),
                        input_tokens: event.usage_known.then_some(event.input_tokens),
                        output_tokens: event.usage_known.then_some(event.output_tokens),
                    },
                )
            }
        }
        _ => return Err(DeliveryError::Permanent(400)),
    };
    let response = request
        .bearer_auth(token)
        .headers(event.trace_headers.clone())
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(http_failure)?;
    // error_for_status deliberately excludes 3xx. A redirect is not a commit ACK.
    if !response.status().is_success() {
        return Err(DeliveryError::Permanent(response.status().as_u16()));
    }
    Ok(())
}
