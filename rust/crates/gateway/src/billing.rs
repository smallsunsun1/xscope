//! The control plane owns durable holds. Recovery may release an
//! undispatched hold, but can NEVER grant permission to replay inference.
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use xscope_domain::billing::ReserveRequest;

use crate::delivery::Permit;

pub struct BillingAdmission {
    sender: SyncSender<Command>,
    healthy: Arc<AtomicBool>,
    timeout: Duration,
    pub context_tokens: i64,
    pub price_version: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Ticket {
    pub id: String,
    pub project_id: String,
    pub input_token_limit: i64,
    pub output_token_limit: i64,
}

struct Intent {
    request: ReserveRequest,
    trace_headers: http::HeaderMap,
}

struct Command {
    _permit: Arc<Permit>,
    intent: Intent,
    reply: oneshot::Sender<Result<(), u16>>,
    queued: Instant,
    deadline: Instant,
    _slot: AdmissionSlot,
}

// RAII also covers failed try_send, cancellation and worker unwinding.
struct AdmissionSlot;
impl Drop for AdmissionSlot {
    fn drop(&mut self) {
        xscope_telemetry::admission_change("occupied", -1);
    }
}
struct ActiveAdmission;
impl Drop for ActiveAdmission {
    fn drop(&mut self) {
        xscope_telemetry::admission_change("active", -1);
    }
}
struct WorkerHealth(Arc<AtomicBool>);
impl Drop for WorkerHealth {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub workers: usize,
    pub capacity: usize,
    pub timeout: Duration,
}

impl Settings {
    pub fn from_env() -> io::Result<Self> {
        Self::parse(|name| std::env::var(name).ok())
    }

    fn parse(get: impl Fn(&str) -> Option<String>) -> io::Result<Self> {
        let number = |name, fallback: usize, min, max| {
            get(name)
                .map_or(Ok(fallback), |raw| raw.parse::<usize>())
                .ok()
                .filter(|v| (min..=max).contains(v))
                .ok_or_else(|| io::Error::other(format!("invalid {name}")))
        };
        Ok(Self {
            workers: number("XSCOPE_BILLING_ADMISSION_WORKERS", 4, 1, 32)?,
            capacity: number("XSCOPE_BILLING_ADMISSION_CAPACITY", 64, 1, 4096)?,
            timeout: Duration::from_millis(number(
                "XSCOPE_BILLING_ADMISSION_TIMEOUT_MS",
                8000,
                100,
                60000,
            )? as u64),
        })
    }
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
    /// Volatile admission: the control plane is the authority for every hold.
    /// Failed/abandoned HTTP attempts are reconciled once; unrecoverable intents
    /// remain visible in the central pending list, never replayed as inference.
    pub fn new(
        base: String,
        token: String,
        context_tokens: i64,
        price_version: String,
    ) -> io::Result<Self> {
        let settings = Settings::from_env()?;
        let protocol = Arc::new(Protocol {
            client: Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .timeout(Duration::from_secs(3))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|_| io::Error::other("admission HTTP client initialization failed"))?,
            base: base.trim_end_matches('/').into(),
            token,
        });
        let healthy = Arc::new(AtomicBool::new(true));
        let (sender, receiver) = mpsc::sync_channel::<Command>(settings.capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        xscope_telemetry::admission_limits(settings.workers, settings.capacity);
        for _ in 0..settings.workers {
            let (receiver, protocol, health) =
                (receiver.clone(), protocol.clone(), healthy.clone());
            thread::Builder::new()
                .name("billing-admission".into())
                .spawn(move || {
                    let _health = WorkerHealth(health);
                    loop {
                        // Lock only the dequeue, never an HTTP request or recovery.
                        let command = match receiver.lock() {
                            Ok(receiver) => receiver.recv(),
                            Err(_) => break,
                        };
                        let Ok(command) = command else { break };
                        xscope_telemetry::admission_duration(
                            "queue",
                            command.queued.elapsed().as_secs_f64(),
                        );
                        if command.reply.is_closed() || Instant::now() >= command.deadline {
                            xscope_telemetry::background_event(
                                "billing_admit",
                                "expired_before_reserve",
                            );
                            continue;
                        }
                        xscope_telemetry::admission_change("active", 1);
                        let _active = ActiveAdmission;
                        let start = Instant::now();
                        let decision = protocol.admit(&command);
                        xscope_telemetry::admission_duration(
                            "protocol",
                            start.elapsed().as_secs_f64(),
                        );
                        let ambiguous = matches!(decision, Decision::Uncertain(_));
                        let result = match decision {
                            Decision::Dispatched => Ok(()),
                            Decision::Rejected(status) | Decision::Uncertain(status) => Err(status),
                        };
                        // A cancelled caller cannot forward. Never dispatch from recovery.
                        let abandoned = command.reply.send(result).is_err();
                        if (ambiguous || abandoned) && protocol.recover(&command.intent).is_err() {
                            tracing::warn!(reservation_id = %command.intent.request.id,
                            "admission outcome unknown; inspect central pending reservations");
                            xscope_telemetry::background_event(
                                "billing_recover",
                                "central_pending",
                            );
                        }
                    }
                })
                .map_err(|_| io::Error::other("admission worker initialization failed"))?;
        }
        Ok(Self {
            sender,
            healthy,
            timeout: settings.timeout,
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
        permit: Arc<Permit>,
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
        let queued = Instant::now();
        xscope_telemetry::admission_change("occupied", 1);
        self.sender
            .try_send(Command {
                _permit: permit,
                intent: Intent {
                    request,
                    trace_headers,
                },
                reply,
                queued,
                deadline: queued + self.timeout,
                _slot: AdmissionSlot,
            })
            .map_err(|_| {
                xscope_telemetry::background_event("billing_admit", "queue_rejected");
                503_u16
            })?;
        tokio::time::timeout(self.timeout, receive)
            .await
            .map_err(|_| {
                xscope_telemetry::background_event("billing_admit", "deadline_exceeded");
                503_u16
            })?
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
        if command.reply.is_closed() || Instant::now() >= command.deadline {
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
                "dispatched" | "settled" | "released" | "waived" => Ok(()),
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn concurrency_settings_are_bounded_and_invalid_values_fail_closed() {
        let defaults = Settings::parse(|_| None).unwrap();
        assert_eq!((defaults.workers, defaults.capacity), (4, 64));
        for (key, value) in [
            ("WORKERS", "0"),
            ("WORKERS", "33"),
            ("CAPACITY", "4097"),
            ("TIMEOUT_MS", "-1"),
        ] {
            assert!(
                Settings::parse(|name| (name == format!("XSCOPE_BILLING_ADMISSION_{key}"))
                    .then(|| value.into()))
                .is_err()
            );
        }
    }
}
