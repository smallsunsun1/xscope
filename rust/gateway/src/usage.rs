use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::auth::Principal;

pub struct UsageSink {
    writer: Mutex<BufWriter<File>>,
    region: String,
    model_revision: String,
    price_version: String,
}

pub(crate) struct UsageRecord<'a> {
    pub request_id: &'a str,
    pub principal: &'a Principal,
    pub endpoint_id: &'a str,
    pub model_id: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
    pub status: &'a str,
}

#[derive(Debug, Serialize)]
pub struct UsageEvent<'a> {
    pub schema_version: &'static str,
    pub event_id: String,
    pub request_id: &'a str,
    pub occurred_at: DateTime<Utc>,
    pub tenant_id: &'a str,
    pub project_id: &'a str,
    pub api_key_id: &'a str,
    pub model_id: &'a str,
    pub model_revision: &'a str,
    pub endpoint_id: &'a str,
    pub region: &'a str,
    pub price_version: &'a str,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub latency_ms: u64,
    pub status: &'a str,
}

#[derive(Debug, Deserialize)]
pub struct CompletionUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct CompletionResponse {
    pub model: String,
    pub usage: CompletionUsage,
}

impl UsageSink {
    /// Opens an append-only usage-event write-ahead log.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the WAL cannot be created or opened.
    pub fn open(
        path: impl AsRef<Path>,
        region: String,
        model_revision: String,
        price_version: String,
    ) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
            region,
            model_revision,
            price_version,
        })
    }

    /// Appends and synchronizes one immutable usage event.
    ///
    /// # Errors
    ///
    /// Returns an I/O or serialization error when the event cannot be made
    /// durable in the WAL.
    pub(crate) fn record(&self, record: &UsageRecord<'_>) -> io::Result<()> {
        let event = UsageEvent {
            schema_version: "v1",
            event_id: format!("evt-{}", uuid::Uuid::now_v7()),
            request_id: record.request_id,
            occurred_at: Utc::now(),
            tenant_id: &record.principal.tenant_id,
            project_id: &record.principal.project_id,
            api_key_id: &record.principal.api_key_id,
            model_id: record.model_id,
            model_revision: &self.model_revision,
            endpoint_id: record.endpoint_id,
            region: &self.region,
            price_version: &self.price_version,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            cached_input_tokens: 0,
            latency_ms: record.latency_ms,
            status: record.status,
        };
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("usage WAL lock poisoned"))?;
        serde_json::to_writer(&mut *writer, &event)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        writer.get_ref().sync_data()
    }
}
