//! A single admission worker owns the intent WAL. Recovery may release an
//! undispatched hold, but can NEVER grant permission to replay inference.
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread;
use std::time::Duration;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use xscope_domain::billing::ReserveRequest;

use crate::segments::Journal;
use crate::storage::{StorageBudget, StoragePermit};

pub struct BillingAdmission {
    sender: SyncSender<Command>,
    healthy: Arc<AtomicBool>,
    pub context_tokens: i64,
    pub price_version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Ticket {
    pub id: String,
    pub project_id: String,
    pub input_token_limit: i64,
    pub output_token_limit: i64,
}

#[derive(Deserialize, Serialize)]
struct Intent {
    version: u32,
    request: ReserveRequest,
    #[serde(default, with = "crate::usage::trace_context")]
    trace_headers: http::HeaderMap,
}

struct Command {
    _space: Arc<StoragePermit>,
    intent: Intent,
    reply: oneshot::Sender<Result<(), u16>>,
}

#[derive(Deserialize)]
struct Reservation {
    state: String,
}

struct Protocol {
    client: Client,
    base: String,
    token: String,
}

enum Decision {
    Dispatched,
    Rejected(u16),
    Uncertain(u16),
}

impl BillingAdmission {
    /// Open the admission journal and reconcile unfinished intents before readiness.
    ///
    /// # Errors
    /// Returns an error if durable storage cannot be opened exclusively.
    pub fn open(
        usage_path: &Path,
        base: String,
        token: String,
        context_tokens: i64,
        price_version: String,
        storage: Arc<StorageBudget>,
    ) -> io::Result<Self> {
        let mut path = usage_path.as_os_str().to_owned();
        path.push(".reservations.jsonl");
        let journal = Journal::open(Path::new(&path), storage)?;
        let healthy = Arc::new(AtomicBool::new(false));
        let health = healthy.clone();
        let (sender, receiver) = mpsc::sync_channel::<Command>(64);
        thread::spawn(move || {
            let result = (|| -> io::Result<()> {
                let protocol = Protocol {
                    client: Client::builder()
                        .connect_timeout(Duration::from_secs(2))
                        .timeout(Duration::from_secs(3))
                        .redirect(reqwest::redirect::Policy::none())
                        .build()
                        .map_err(io::Error::other)?,
                    base: base.trim_end_matches('/').into(),
                    token,
                };
                let mut journal = journal;
                loop {
                    // No live request owns these records: startup, failed HTTP,
                    // cancelled caller, or lost response. Never dispatch here.
                    while let Some(record) = journal.next()? {
                        let intent: Intent = serde_json::from_slice(&record.bytes)?;
                        if intent.version != 1 {
                            return Err(io::Error::other("unsupported admission WAL version"));
                        }
                        health.store(false, Ordering::Release);
                        if let Err(status) = protocol.recover(&intent) {
                            xscope_telemetry::background_event("billing_recover", "retry");
                            tracing::warn!(status, reservation_id = %intent.request.id,
                                "financial admission recovery pending; inference blocked");
                            thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                        journal.acknowledge(&record)?;
                        xscope_telemetry::background_event("billing_recover", "success");
                    }
                    health.store(true, Ordering::Release);
                    let Ok(command) = receiver.recv() else {
                        return Ok(());
                    };
                    if command.reply.is_closed() {
                        continue;
                    }
                    // Must be durable before the first reserve HTTP call.
                    journal.append(&serde_json::to_vec(&command.intent)?)?;
                    let decision = protocol.admit(&command);
                    if !matches!(decision, Decision::Uncertain(_)) {
                        let record = journal.next()?.ok_or_else(|| {
                            io::Error::other("admission intent disappeared after durable append")
                        })?;
                        journal.acknowledge(&record)?;
                    } else {
                        health.store(false, Ordering::Release);
                    }
                    let result = match decision {
                        Decision::Dispatched => Ok(()),
                        Decision::Rejected(status) | Decision::Uncertain(status) => Err(status),
                    };
                    let _ = command.reply.send(result);
                }
            })();
            health.store(false, Ordering::Release);
            if let Err(error) = result {
                tracing::error!(%error, "financial admission WAL failed; preserving evidence and stopping admission");
            }
        });
        Ok(Self {
            sender,
            healthy,
            context_tokens,
            price_version,
        })
    }

    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub(crate) async fn admit(
        &self,
        request: ReserveRequest,
        trace_headers: http::HeaderMap,
        space: Arc<StoragePermit>,
    ) -> Result<Ticket, u16> {
        if !self.is_healthy() {
            return Err(503);
        }
        let ticket = Ticket {
            id: request.id.clone(),
            project_id: request.project_id.clone(),
            input_token_limit: request.input_token_limit,
            output_token_limit: request.output_token_limit,
        };
        let (reply, receive) = oneshot::channel();
        self.sender
            .try_send(Command {
                _space: space,
                intent: Intent {
                    version: 1,
                    request,
                    trace_headers,
                },
                reply,
            })
            .map_err(|_| 503_u16)?;
        tokio::time::timeout(Duration::from_secs(8), receive)
            .await
            .map_err(|_| 503_u16)?
            .map_err(|_| 503_u16)??;
        Ok(ticket)
    }
}

impl Protocol {
    fn reserve(&self, intent: &Intent) -> Result<Reservation, u16> {
        self.client
            .post(format!("{}/billing/reservations", self.base))
            .bearer_auth(&self.token)
            .headers(intent.trace_headers.clone())
            .json(&intent.request)
            .send()
            .map_err(|_| 503_u16)
            .and_then(decode)
    }

    fn transition(&self, intent: &Intent, action: &str) -> Result<Reservation, u16> {
        self.client
            .post(format!(
                "{}/billing/projects/{}/reservations/{}/{action}",
                self.base, intent.request.project_id, intent.request.id
            ))
            .bearer_auth(&self.token)
            .headers(intent.trace_headers.clone())
            .json(&serde_json::json!({"reason":"not_dispatched"}))
            .send()
            .map_err(|_| 503_u16)
            .and_then(decode)
    }

    fn admit(&self, command: &Command) -> Decision {
        let intent = &command.intent;
        let reservation = match self.reserve(intent) {
            Ok(row) => row,
            Err(status @ (400 | 402 | 403 | 404 | 422)) => return Decision::Rejected(status),
            Err(status) => return Decision::Uncertain(status),
        };
        // Only a fresh live admission may dispatch. A reused terminal ID must
        // not become authority to send a second provider request.
        if reservation.state != "reserved" {
            return Decision::Uncertain(503);
        }
        if command.reply.is_closed() {
            return Decision::Uncertain(503);
        }
        match self.transition(intent, "dispatch") {
            Ok(row) if row.state == "dispatched" => {}
            _ => return Decision::Uncertain(503),
        }
        xscope_telemetry::background_event("billing_admit", "success");
        Decision::Dispatched
    }

    fn recover(&self, intent: &Intent) -> Result<(), u16> {
        match self.reserve(intent) {
            Ok(row) => match row.state.as_str() {
                "reserved" => {
                    if self.transition(intent, "release")?.state != "released" {
                        return Err(503);
                    }
                    Ok(())
                }
                // Dispatch is an irreversible ambiguity boundary. Even a lost
                // dispatch ACK must keep its hold, never auto-refund or resend.
                "dispatched" | "settled" | "released" => Ok(()),
                _ => Err(503),
            },
            // Reserve is transactional and replays existing identities BEFORE
            // current key/price/balance checks. These responses attest no hold.
            Err(400 | 402 | 403 | 404 | 422) => Ok(()),
            Err(status) => Err(status),
        }
    }
}

fn decode(response: reqwest::blocking::Response) -> Result<Reservation, u16> {
    if !response.status().is_success() {
        return Err(response.status().as_u16());
    }
    response.json().map_err(|_| 503)
}
