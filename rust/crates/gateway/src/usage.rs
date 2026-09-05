use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::billing::Ticket;
use crate::wal::Journal;

pub struct UsageSink {
    writer: Arc<Mutex<Journal>>,
    reporter: Option<SyncSender<()>>,
    healthy: Arc<AtomicBool>,
    region: String,
    model_revision: String,
    price_version: String,
}

pub(crate) struct UsageRecord<'a> {
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageEvent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) billing: Option<Ticket>,
    #[serde(default)]
    pub usage_known: bool,
    // Only W3C context is persisted, never credentials or client headers.
    #[serde(default, with = "trace_context")]
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
    /// Opens an append-only usage-event write-ahead log and starts its reporter.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the WAL cannot be created, opened, or replayed.
    pub fn open(
        path: impl AsRef<Path>,
        region: String,
        model_revision: String,
        price_version: String,
        report_url: String,
        internal_token: String,
    ) -> io::Result<Self> {
        let writer = Arc::new(Mutex::new(Journal::open(path.as_ref())?));
        let healthy = Arc::new(AtomicBool::new(true));
        let reporter = if report_url.is_empty() || internal_token.is_empty() {
            None
        } else {
            Some(spawn_reporter(
                report_url,
                internal_token,
                writer.clone(),
                healthy.clone(),
            ))
        };
        Ok(Self {
            writer,
            reporter,
            healthy,
            region,
            model_revision,
            price_version,
        })
    }

    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    /// Durably appends one immutable usage event before asynchronously reporting it.
    ///
    /// # Errors
    ///
    /// Returns an I/O or serialization error when the event cannot be made durable in the WAL.
    pub(crate) fn record(&self, record: &UsageRecord<'_>) -> io::Result<()> {
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
            price_version: self.price_version.clone(),
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cached_input_tokens: 0,
            latency_ms: record.latency_ms,
            status: record.status.to_owned(),
        };
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("usage WAL lock poisoned"))?;
        if let Err(error) = writer.append(&serde_json::to_vec(&event)?) {
            self.healthy.store(false, Ordering::Release);
            return Err(error);
        }
        drop(writer);
        xscope_telemetry::background_event("wal_append", "success");

        if let Some(reporter) = &self.reporter {
            // Notification only. The journal is the queue, so coalesced wakeups
            // cannot lose records, even while delivery is blocked for hours.
            let _ = reporter.try_send(());
        }
        Ok(())
    }
}

fn spawn_reporter(
    report_url: String,
    internal_token: String,
    journal: Arc<Mutex<Journal>>,
    healthy: Arc<AtomicBool>,
) -> SyncSender<()> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(%error, "could not create usage reporter client");
                healthy.store(false, Ordering::Release);
                return;
            }
        };
        loop {
            if matches!(receiver.try_recv(), Err(TryRecvError::Disconnected)) {
                return;
            }
            let record = match journal
                .lock()
                .map_err(|_| io::Error::other("WAL lock poisoned"))
                .and_then(|mut wal| wal.next())
            {
                Ok(Some(record)) => record,
                Ok(None) => {
                    if matches!(
                        receiver.recv_timeout(Duration::from_secs(1)),
                        Err(RecvTimeoutError::Disconnected)
                    ) {
                        return;
                    }
                    continue;
                }
                Err(error) => {
                    healthy.store(false, Ordering::Release);
                    tracing::error!(%error, "usage WAL read failed; preserving cursor and stopping reporter");
                    return;
                }
            };
            let event: UsageEvent = match serde_json::from_slice(&record.bytes) {
                Ok(event) => event,
                Err(error) => {
                    healthy.store(false, Ordering::Release);
                    tracing::error!(%error, "malformed WAL event; preserving evidence and stopping reporter");
                    return;
                }
            };
            match deliver(&client, &report_url, &internal_token, &event) {
                Ok(()) => {
                    if let Err(error) = journal
                        .lock()
                        .map_err(|_| io::Error::other("WAL lock poisoned"))
                        .and_then(|mut wal| wal.acknowledge(&record))
                    {
                        healthy.store(false, Ordering::Release);
                        tracing::error!(%error, "usage checkpoint failed; restart replays the unacknowledged event");
                        return;
                    }
                    xscope_telemetry::background_event("usage_export", "success");
                }
                Err(error) => {
                    xscope_telemetry::background_event("usage_export", "retry");
                    tracing::warn!(
                        %error,
                        event_id = %event.event_id,
                        "usage report failed; cursor retained, retrying (4xx requires operator attention)"
                    );
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
    sender
}

fn deliver(
    client: &reqwest::blocking::Client,
    report_url: &str,
    token: &str,
    event: &UsageEvent,
) -> Result<(), String> {
    let request = match (event.schema_version.as_str(), &event.billing) {
        ("v1", None) => client.post(report_url).json(event),
        ("v2", Some(ticket)) => {
            let base = report_url
                .strip_suffix("/usage-events")
                .ok_or("invalid financial reporter URL")?;
            let url = format!(
                "{base}/billing/projects/{}/reservations/{}",
                ticket.project_id, ticket.id
            );
            if event.usage_known
                && event.input_tokens <= ticket.input_token_limit as u64
                && event.output_tokens <= ticket.output_token_limit as u64
            {
                let settlement = xscope_domain::billing::SettleRequest {
                    input_tokens: i64::try_from(event.input_tokens).map_err(|e| e.to_string())?,
                    output_tokens: i64::try_from(event.output_tokens).map_err(|e| e.to_string())?,
                    latency_ms: i64::try_from(event.latency_ms).map_err(|e| e.to_string())?,
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
                // Unknown/over-limit usage is evidence, NOT a zero settlement.
                // Its hold already lives durably in PostgreSQL. Confirm that
                // state before advancing this delivery cursor; retain the WAL
                // record for reconciliation without blocking unrelated usage.
                let row: serde_json::Value = client
                    .get(url)
                    .bearer_auth(token)
                    .headers(event.trace_headers.clone())
                    .send()
                    .and_then(reqwest::blocking::Response::error_for_status)
                    .and_then(reqwest::blocking::Response::json)
                    .map_err(|e| e.to_string())?;
                if !matches!(row["state"].as_str(), Some("dispatched" | "settled")) {
                    return Err("unresolved usage has no dispatched hold".into());
                }
                tracing::warn!(reservation_id = %ticket.id, usage_known = event.usage_known,
                    "usage requires reconciliation; hold retained, evidence kept in WAL");
                xscope_telemetry::background_event("billing_pending", "retained");
                return Ok(());
            }
        }
        _ => return Err("unsupported or inconsistent usage WAL schema".into()),
    };
    request
        .bearer_auth(token)
        .headers(event.trace_headers.clone())
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(crate) mod trace_context {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        headers: &http::HeaderMap,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        let values: BTreeMap<_, _> = ["traceparent", "tracestate"]
            .into_iter()
            .filter_map(|name| {
                headers
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(|value| (name, value))
            })
            .collect();
        values.serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<http::HeaderMap, D::Error> {
        let values = BTreeMap::<String, String>::deserialize(deserializer)?;
        let mut headers = http::HeaderMap::new();
        for name in ["traceparent", "tracestate"] {
            if let Some(value) = values.get(name) {
                headers.insert(name, value.parse().map_err(serde::de::Error::custom)?);
            }
        }
        Ok(headers)
    }
}
