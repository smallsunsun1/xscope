use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;
use base64::Engine;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use crate::config::ApiKeyConfig;

#[derive(Clone, Debug)]
pub struct Principal {
    route_policies: Vec<xscope_domain::RoutePolicy>,
    pub api_key_id: String,
    pub tenant_id: String,
    pub project_id: String,
    scopes: Vec<String>,
    allowed_models: Vec<String>,
    rate_limit_rpm: u64,
    rate_limit_tpm: u64,
    monthly_budget_amount: i64,
    current_month_spend_amount: i64,
    balance_enforced: bool,
    available_balance_amount: i64,
}

impl Principal {
    pub fn route_policy(&self, model: &str) -> Option<&xscope_domain::RoutePolicy> {
        self.route_policies
            .iter()
            .find(|policy| policy.model == model)
    }
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|candidate| candidate == scope)
    }

    #[must_use]
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models
            .iter()
            .any(|candidate| candidate == model)
    }

    #[must_use]
    pub const fn budget_exhausted(&self) -> bool {
        self.monthly_budget_amount > 0
            && self.current_month_spend_amount >= self.monthly_budget_amount
    }

    #[must_use]
    pub const fn funds_exhausted(&self) -> bool {
        self.balance_enforced && self.available_balance_amount <= 0
    }

    #[must_use]
    pub const fn rate_limit_rpm(&self) -> u64 {
        self.rate_limit_rpm
    }

    #[must_use]
    pub const fn rate_limit_tpm(&self) -> u64 {
        self.rate_limit_tpm
    }
}

#[derive(Clone)]
struct Credential {
    principal: Principal,
    secret_hash: [u8; 32],
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
struct KeySet(Vec<Credential>);

#[derive(Default)]
struct RateWindow {
    minute: i64,
    count: u64,
}

pub struct DynamicKeySet {
    current: ArcSwap<KeySet>,
    rate_windows: DashMap<String, RateWindow>,
}

#[derive(Debug, Deserialize)]
struct GatewaySnapshot {
    keys: Vec<GatewayKey>,
    #[serde(default)]
    route_policies: Vec<xscope_domain::RoutePolicy>,
}

#[derive(Debug, Deserialize)]
struct GatewayKey {
    id: String,
    tenant_id: String,
    project_id: String,
    secret_hash: String,
    scopes: Vec<String>,
    allowed_models: Vec<String>,
    expires_at: Option<DateTime<Utc>>,
    rate_limit_rpm: u64,
    #[serde(default = "default_rate_limit_tpm")]
    rate_limit_tpm: u64,
    monthly_budget: Money,
    current_month_spend: Money,
    #[serde(default)]
    balance_enforced: bool,
    #[serde(default)]
    available_balance: Money,
}

#[derive(Debug, Default, Deserialize)]
struct Money {
    amount: i64,
}

const fn default_rate_limit_tpm() -> u64 {
    60_000
}

#[derive(Debug, Error)]
enum SnapshotError {
    #[error("invalid or duplicate route policy in snapshot")]
    InvalidRoutePolicy,
    #[error("snapshot request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("API key {0} has an invalid SHA-256 digest")]
    InvalidDigest(String),
}

impl DynamicKeySet {
    #[must_use]
    pub fn new(configs: &[ApiKeyConfig]) -> Arc<Self> {
        Arc::new(Self {
            current: ArcSwap::from_pointee(KeySet::from_static(configs)),
            rate_windows: DashMap::new(),
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.current.load().0.is_empty()
    }

    #[must_use]
    pub fn authenticate(&self, authorization: Option<&str>) -> Option<Principal> {
        let secret = authorization?.strip_prefix("Bearer ")?;
        let candidate: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        let now = Utc::now();
        self.current
            .load()
            .0
            .iter()
            .find(|credential| {
                credential
                    .expires_at
                    .is_none_or(|expires_at| expires_at > now)
                    && bool::from(credential.secret_hash.ct_eq(&candidate))
            })
            .map(|credential| credential.principal.clone())
    }

    #[must_use]
    pub fn check_rate_limit(&self, principal: &Principal) -> bool {
        let minute = Utc::now().timestamp() / 60;
        let mut window = self
            .rate_windows
            .entry(principal.api_key_id.clone())
            .or_default();
        if window.minute != minute {
            window.minute = minute;
            window.count = 0;
        }
        if window.count >= principal.rate_limit_rpm {
            return false;
        }
        window.count += 1;
        true
    }

    pub fn spawn_refresh(
        self: &Arc<Self>,
        internal_url: String,
        internal_token: String,
        interval: Duration,
    ) {
        if internal_url.is_empty() || internal_token.is_empty() {
            return;
        }
        let keys = Arc::clone(self);
        thread::spawn(move || {
            let client = match reqwest::blocking::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(5))
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    tracing::error!(%error, "could not create API key snapshot client");
                    return;
                }
            };
            let url = format!("{}/gateway/snapshot", internal_url.trim_end_matches('/'));
            loop {
                match fetch_snapshot(&client, &url, &internal_token).and_then(KeySet::from_snapshot)
                {
                    Ok(snapshot) => {
                        let count = snapshot.0.len();
                        keys.current.store(Arc::new(snapshot));
                        tracing::info!(count, "API key policy snapshot refreshed");
                    }
                    Err(error) => tracing::warn!(%error, "API key policy snapshot refresh failed"),
                }
                thread::sleep(interval);
            }
        });
    }
}

fn fetch_snapshot(
    client: &reqwest::blocking::Client,
    url: &str,
    token: &str,
) -> Result<GatewaySnapshot, SnapshotError> {
    Ok(client
        .get(url)
        .bearer_auth(token)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)?
        .json()?)
}

impl KeySet {
    fn from_static(configs: &[ApiKeyConfig]) -> Self {
        Self(
            configs
                .iter()
                .map(|config| Credential {
                    principal: Principal {
                        route_policies: Vec::new(),
                        api_key_id: config.id.clone(),
                        tenant_id: config.tenant_id.clone(),
                        project_id: config.project_id.clone(),
                        scopes: vec!["chat.completions".to_owned()],
                        allowed_models: vec!["xscope-demo".to_owned()],
                        rate_limit_rpm: 600,
                        rate_limit_tpm: 600_000,
                        monthly_budget_amount: 0,
                        current_month_spend_amount: 0,
                        balance_enforced: false,
                        available_balance_amount: 0,
                    },
                    secret_hash: Sha256::digest(config.secret.as_bytes()).into(),
                    expires_at: None,
                })
                .collect(),
        )
    }

    fn from_snapshot(snapshot: GatewaySnapshot) -> Result<Self, SnapshotError> {
        let mut identities = std::collections::HashSet::new();
        for policy in &snapshot.route_policies {
            if policy.revision <= 0
                || policy.spec.validate().is_err()
                || !identities.insert((&policy.tenant_id, &policy.project_id, &policy.model))
            {
                return Err(SnapshotError::InvalidRoutePolicy);
            }
        }
        let mut credentials = Vec::with_capacity(snapshot.keys.len());
        for key in snapshot.keys {
            let digest = base64::engine::general_purpose::STANDARD
                .decode(&key.secret_hash)
                .map_err(|_| SnapshotError::InvalidDigest(key.id.clone()))?;
            let secret_hash: [u8; 32] = digest
                .try_into()
                .map_err(|_| SnapshotError::InvalidDigest(key.id.clone()))?;
            credentials.push(Credential {
                principal: Principal {
                    route_policies: snapshot
                        .route_policies
                        .iter()
                        .filter(|p| p.tenant_id == key.tenant_id && p.project_id == key.project_id)
                        .cloned()
                        .collect(),
                    api_key_id: key.id,
                    tenant_id: key.tenant_id,
                    project_id: key.project_id,
                    scopes: key.scopes,
                    allowed_models: key.allowed_models,
                    rate_limit_rpm: key.rate_limit_rpm,
                    rate_limit_tpm: key.rate_limit_tpm,
                    monthly_budget_amount: key.monthly_budget.amount,
                    current_month_spend_amount: key.current_month_spend.amount,
                    balance_enforced: key.balance_enforced,
                    available_balance_amount: key.available_balance.amount,
                },
                secret_hash,
                expires_at: key.expires_at,
            });
        }
        Ok(Self(credentials))
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::{DynamicKeySet, GatewaySnapshot, KeySet};
    use crate::config::ApiKeyConfig;

    #[test]
    fn authenticates_bearer_secret_without_retaining_plaintext() {
        let keys = DynamicKeySet::new(&[ApiKeyConfig {
            id: "key-1".into(),
            tenant_id: "tenant-1".into(),
            project_id: "project-1".into(),
            secret: "secret-value".into(),
        }]);
        let principal = keys.authenticate(Some("Bearer secret-value")).unwrap();
        assert_eq!(principal.project_id, "project-1");
        assert!(principal.has_scope("chat.completions"));
        assert!(principal.allows_model("xscope-demo"));
        assert!(keys.authenticate(Some("Bearer wrong")).is_none());
    }

    #[test]
    fn accepts_snapshot_from_a_control_plane_during_rolling_upgrade() {
        let digest = base64::engine::general_purpose::STANDARD.encode([7_u8; 32]);
        let snapshot: GatewaySnapshot = serde_json::from_value(serde_json::json!({
            "keys": [{
                "id": "key-1",
                "tenant_id": "tenant-1",
                "project_id": "project-1",
                "secret_hash": digest,
                "scopes": ["chat.completions"],
                "allowed_models": ["xscope-demo"],
                "rate_limit_rpm": 60,
                "monthly_budget": {"amount": 0},
                "current_month_spend": {"amount": 0}
            }]
        }))
        .unwrap();
        let keys = KeySet::from_snapshot(snapshot).unwrap();
        assert_eq!(keys.0[0].principal.rate_limit_tpm(), 60_000);
        assert!(!keys.0[0].principal.funds_exhausted());
    }
}
