use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::Principal;

pub struct UsageSink {
    writer: Mutex<BufWriter<File>>,
    reporter: Option<SyncSender<UsageEvent>>,
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
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub status: &'a str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UsageEvent {
    // Live asynchronous delivery continues the inference trace. WAL replay
    // deliberately starts a new trace; telemetry is not billing state.
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
        let replay = read_wal(path.as_ref())?;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let reporter = if report_url.is_empty() || internal_token.is_empty() {
            None
        } else {
            Some(spawn_reporter(report_url, internal_token, replay))
        };
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            reporter,
            region,
            model_revision,
            price_version,
        })
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
            schema_version: "v1".to_owned(),
            event_id: format!("evt-{}", uuid::Uuid::now_v7()),
            request_id: record.request_id.to_owned(),
            occurred_at: Utc::now(),
            tenant_id: record.principal.tenant_id.clone(),
            project_id: record.principal.project_id.clone(),
            api_key_id: record.principal.api_key_id.clone(),
            model_id: record.model_id.to_owned(),
            model_revision: self.model_revision.clone(),
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
        serde_json::to_writer(&mut *writer, &event)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_data()?;
        drop(writer);
        xscope_telemetry::background_event("wal_append", "success");

        if let Some(reporter) = &self.reporter {
            match reporter.try_send(event) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    xscope_telemetry::background_event("usage_queue", "full");
                    tracing::warn!(
                        "usage reporter queue is full; event remains durable in the WAL for replay"
                    );
                }
                Err(TrySendError::Disconnected(_)) => {
                    tracing::error!("usage reporter stopped; event remains durable in the WAL");
                }
            }
        }
        Ok(())
    }
}

fn read_wal(path: &Path) -> io::Result<VecDeque<UsageEvent>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(VecDeque::new()),
        Err(error) => return Err(error),
    };
    let mut events = VecDeque::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        match serde_json::from_str(&line) {
            Ok(event) => events.push_back(event),
            Err(error) => tracing::warn!(%error, "ignoring malformed usage WAL record"),
        }
    }
    Ok(events)
}

fn spawn_reporter(
    report_url: String,
    internal_token: String,
    replay: VecDeque<UsageEvent>,
) -> SyncSender<UsageEvent> {
    let (sender, receiver) = mpsc::sync_channel(1024);
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .build()
        {
            Ok(client) => client,
            Err(error) => {
                tracing::error!(%error, "could not create usage reporter client");
                return;
            }
        };
        let mut backlog = replay;
        loop {
            if backlog.is_empty() {
                let Ok(event) = receiver.recv() else {
                    return;
                };
                backlog.push_back(event);
            }
            while let Ok(event) = receiver.try_recv() {
                backlog.push_back(event);
            }
            let Some(event) = backlog.front() else {
                continue;
            };
            match client
                .post(&report_url)
                .bearer_auth(&internal_token)
                .headers(event.trace_headers.clone())
                .json(event)
                .send()
            {
                Ok(response) if response.status().is_success() => {
                    xscope_telemetry::background_event("usage_export", "success");
                    backlog.pop_front();
                }
                Ok(response)
                    if response.status().is_client_error() && response.status().as_u16() != 429 =>
                {
                    xscope_telemetry::background_event("usage_export", "rejected");
                    tracing::error!(
                        status = response.status().as_u16(),
                        event_id = %event.event_id,
                        "usage report was permanently rejected; skipping it"
                    );
                    backlog.pop_front();
                }
                Ok(response) => {
                    xscope_telemetry::background_event("usage_export", "retry");
                    tracing::warn!(
                        status = response.status().as_u16(),
                        event_id = %event.event_id,
                        "usage report failed; retrying"
                    );
                    thread::sleep(Duration::from_secs(2));
                }
                Err(error) => {
                    xscope_telemetry::background_event("usage_export", "retry");
                    tracing::warn!(%error, event_id = %event.event_id, "usage report failed; retrying");
                    thread::sleep(Duration::from_secs(2));
                }
            }
        }
    });
    sender
}
