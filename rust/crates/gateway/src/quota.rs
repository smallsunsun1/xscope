use std::sync::Arc;
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread;
use std::time::Duration;

use redis::Script;
use thiserror::Error;
use tokio::sync::oneshot;

use crate::auth::Principal;

const RESERVE_SCRIPT: &str = r"
local field = 'r:' .. ARGV[4]
local prior = redis.call('HGET', KEYS[1], field)
if prior then
  if prior ~= ARGV[3] then return redis.error_reply('reservation payload conflict') end
  return {1, 0}
end
if tonumber(redis.call('TIME')[1]) >= tonumber(ARGV[5]) then
  return redis.error_reply('quota window expired')
end
local requests = tonumber(redis.call('HGET', KEYS[1], 'requests') or '0')
local tokens = tonumber(redis.call('HGET', KEYS[1], 'tokens') or '0')
local rpm = tonumber(ARGV[1])
local tpm = tonumber(ARGV[2])
local requested = tonumber(ARGV[3])
if requests + 1 > rpm then
  return {0, 1, requests, tokens}
end
if tokens + requested > tpm then
  return {0, 2, requests, tokens}
end
requests = redis.call('HINCRBY', KEYS[1], 'requests', 1)
tokens = redis.call('HINCRBY', KEYS[1], 'tokens', requested)
redis.call('HSET', KEYS[1], field, ARGV[3])
redis.call('EXPIREAT', KEYS[1], ARGV[5])
return {1, 0, requests, tokens}
";

const SETTLE_SCRIPT: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then return 0 end
local reserved = redis.call('HGET', KEYS[1], 'r:' .. ARGV[3])
if not reserved or reserved ~= ARGV[1] then return redis.error_reply('unknown or conflicting reservation') end
local prior = redis.call('HGET', KEYS[1], 's:' .. ARGV[3])
if prior then
  if prior ~= ARGV[2] then return redis.error_reply('settlement payload conflict') end
  return 0
end
local tokens = tonumber(redis.call('HGET', KEYS[1], 'tokens') or '0')
reserved = tonumber(reserved)
local actual = tonumber(ARGV[2])
tokens = math.max(0, tokens - reserved + actual)
redis.call('HSET', KEYS[1], 'tokens', tokens)
redis.call('HSET', KEYS[1], 's:' .. ARGV[3], ARGV[2])
return tokens
";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaDenial {
    RequestsPerMinute,
    TokensPerMinute,
}

#[derive(Clone, Debug)]
pub struct QuotaReservation {
    key: String,
    id: String,
    reserved_tokens: u64,
}

#[derive(Debug)]
pub enum QuotaDecision {
    Allowed(QuotaReservation),
    Denied(QuotaDenial),
}

#[derive(Debug, Error)]
pub enum QuotaError {
    #[error("Redis quota worker is unavailable")]
    Unavailable,
    #[error("Redis quota operation timed out")]
    Timeout,
    #[error("Redis quota operation failed: {0}")]
    Redis(String),
}

enum Command {
    Reserve {
        key: String,
        id: String,
        expires_at: i64,
        rpm: u64,
        tpm: u64,
        tokens: u64,
        reply: oneshot::Sender<Result<QuotaDecision, QuotaError>>,
    },
    Settle {
        reservation: QuotaReservation,
        actual_tokens: u64,
        reply: oneshot::Sender<Result<(), QuotaError>>,
    },
}

enum CommandResult {
    Reserved(QuotaDecision),
    Settled,
}

pub struct QuotaManager {
    sender: Option<SyncSender<Command>>,
}

impl QuotaManager {
    #[must_use]
    pub fn new(redis_url: &str) -> Arc<Self> {
        if redis_url.is_empty() {
            return Arc::new(Self { sender: None });
        }
        let (sender, receiver) = mpsc::sync_channel::<Command>(1024);
        let url = redis_url.to_owned();
        thread::spawn(move || {
            let client = match redis::Client::open(url) {
                Ok(client) => client,
                Err(error) => {
                    tracing::error!(%error, "invalid Redis quota URL");
                    return;
                }
            };
            let mut connection = None;
            while let Ok(command) = receiver.recv() {
                let mut result = Err(QuotaError::Unavailable);
                // Both Lua operations carry the same server-generated ID across
                // retries. A lost reply can no longer double reserve or settle.
                for _ in 0..2 {
                    if command.cancelled() {
                        break;
                    }
                    result = (|| {
                        if connection.is_none() {
                            let connected = client
                                .get_connection_with_timeout(Duration::from_millis(300))
                                .map_err(|e| QuotaError::Redis(e.to_string()))?;
                            connected
                                .set_read_timeout(Some(Duration::from_millis(300)))
                                .map_err(|e| QuotaError::Redis(e.to_string()))?;
                            connected
                                .set_write_timeout(Some(Duration::from_millis(300)))
                                .map_err(|e| QuotaError::Redis(e.to_string()))?;
                            connection = Some(connected);
                        }
                        execute(connection.as_mut().unwrap(), &command)
                    })();
                    if result.is_ok() {
                        break;
                    }
                    connection = None;
                }
                reply(command, result);
            }
        });
        Arc::new(Self {
            sender: Some(sender),
        })
    }

    #[must_use]
    pub const fn is_distributed(&self) -> bool {
        self.sender.is_some()
    }

    /// Atomically reserves one request and the estimated token count.
    ///
    /// # Errors
    ///
    /// Returns [`QuotaError`] when Redis or the bounded worker is unavailable.
    pub async fn reserve(
        &self,
        principal: &Principal,
        requested_tokens: u64,
    ) -> Result<Option<QuotaDecision>, QuotaError> {
        let Some(sender) = &self.sender else {
            return Ok(None);
        };
        let minute = chrono::Utc::now().timestamp() / 60;
        let key = format!("xscope:quota:{{{}}}:{minute}", principal.api_key_id);
        let (reply, response) = oneshot::channel();
        sender
            .try_send(Command::Reserve {
                key,
                id: uuid::Uuid::now_v7().to_string(),
                expires_at: (minute + 2) * 60,
                rpm: principal.rate_limit_rpm(),
                tpm: principal.rate_limit_tpm(),
                tokens: requested_tokens,
                reply,
            })
            .map_err(map_send_error)?;
        match tokio::time::timeout(Duration::from_secs(2), response).await {
            Ok(Ok(result)) => result.map(Some),
            Ok(Err(_)) => Err(QuotaError::Unavailable),
            Err(_) => Err(QuotaError::Timeout),
        }
    }

    /// Replaces the estimated reservation with the final token count.
    ///
    /// # Errors
    ///
    /// Returns [`QuotaError`] when Redis or the bounded worker is unavailable.
    pub async fn settle(
        &self,
        reservation: QuotaReservation,
        actual_tokens: u64,
    ) -> Result<(), QuotaError> {
        let Some(sender) = &self.sender else {
            return Ok(());
        };
        let (reply, response) = oneshot::channel();
        sender
            .try_send(Command::Settle {
                reservation,
                actual_tokens,
                reply,
            })
            .map_err(map_send_error)?;
        match tokio::time::timeout(Duration::from_secs(2), response).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(QuotaError::Unavailable),
            Err(_) => Err(QuotaError::Timeout),
        }
    }
}

fn execute(
    connection: &mut redis::Connection,
    command: &Command,
) -> Result<CommandResult, QuotaError> {
    match command {
        Command::Reserve {
            key,
            id,
            expires_at,
            rpm,
            tpm,
            tokens,
            ..
        } => {
            let result: Vec<i64> = Script::new(RESERVE_SCRIPT)
                .key(key)
                .arg(*rpm)
                .arg(*tpm)
                .arg(*tokens)
                .arg(id)
                .arg(*expires_at)
                .invoke(connection)
                .map_err(|error| QuotaError::Redis(error.to_string()))?;
            match result.as_slice() {
                [1, ..] => Ok(CommandResult::Reserved(QuotaDecision::Allowed(
                    QuotaReservation {
                        key: key.clone(),
                        id: id.clone(),
                        reserved_tokens: *tokens,
                    },
                ))),
                [0, 1, ..] => Ok(CommandResult::Reserved(QuotaDecision::Denied(
                    QuotaDenial::RequestsPerMinute,
                ))),
                [0, 2, ..] => Ok(CommandResult::Reserved(QuotaDecision::Denied(
                    QuotaDenial::TokensPerMinute,
                ))),
                _ => Err(QuotaError::Redis("unexpected Redis script response".into())),
            }
        }
        Command::Settle {
            reservation,
            actual_tokens,
            ..
        } => {
            let _: i64 = Script::new(SETTLE_SCRIPT)
                .key(&reservation.key)
                .arg(reservation.reserved_tokens)
                .arg(*actual_tokens)
                .arg(&reservation.id)
                .invoke(connection)
                .map_err(|error| QuotaError::Redis(error.to_string()))?;
            Ok(CommandResult::Settled)
        }
    }
}

fn reply(command: Command, result: Result<CommandResult, QuotaError>) {
    match command {
        Command::Reserve { reply, .. } => {
            let result = result.and_then(|value| match value {
                CommandResult::Reserved(decision) => Ok(decision),
                CommandResult::Settled => Err(QuotaError::Unavailable),
            });
            let _ = reply.send(result);
        }
        Command::Settle { reply, .. } => {
            let result = result.and_then(|value| match value {
                CommandResult::Settled => Ok(()),
                CommandResult::Reserved(_) => Err(QuotaError::Unavailable),
            });
            let _ = reply.send(result);
        }
    }
}

impl Command {
    fn cancelled(&self) -> bool {
        match self {
            Self::Reserve { reply, .. } => reply.is_closed(),
            Self::Settle { reply, .. } => reply.is_closed(),
        }
    }
}

fn map_send_error<T>(_: TrySendError<T>) -> QuotaError {
    QuotaError::Unavailable
}
