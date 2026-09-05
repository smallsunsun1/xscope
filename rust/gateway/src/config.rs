use std::env;

use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Settings {
    pub listen: String,
    pub region: String,
    pub model_revision: String,
    pub price_version: String,
    pub usage_wal: String,
    pub api_keys: Vec<ApiKeyConfig>,
    pub upstreams: Vec<UpstreamConfig>,
    pub control_internal_url: String,
    pub internal_token: String,
    pub policy_refresh_seconds: u64,
    pub redis_url: String,
}

#[derive(Clone, Deserialize)]
pub struct ApiKeyConfig {
    pub id: String,
    pub tenant_id: String,
    pub project_id: String,
    pub secret: String,
}

impl std::fmt::Debug for ApiKeyConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApiKeyConfig")
            .field("id", &self.id)
            .field("tenant_id", &self.tenant_id)
            .field("project_id", &self.project_id)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct UpstreamConfig {
    pub id: String,
    pub address: String,
    #[serde(default = "default_weight")]
    pub weight: usize,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub server_name: String,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("{name} contains invalid JSON: {source}")]
    Json {
        name: &'static str,
        source: serde_json::Error,
    },
    #[error("at least one upstream with a positive weight is required")]
    NoUpstreams,
}

impl Settings {
    /// Loads gateway settings from the `XSCOPE_*` environment variables.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError`] when a JSON setting is malformed or no
    /// positive-weight upstream is configured.
    pub fn from_env() -> Result<Self, SettingsError> {
        let api_keys = parse_json_env("XSCOPE_API_KEYS_JSON", "[]")?;
        let upstreams: Vec<UpstreamConfig> = parse_json_env(
            "XSCOPE_UPSTREAMS_JSON",
            r#"[{"id":"runtime-dev","address":"127.0.0.1:8090","weight":100}]"#,
        )?;
        if !upstreams.iter().any(|upstream| upstream.weight > 0) {
            return Err(SettingsError::NoUpstreams);
        }
        Ok(Self {
            listen: env_or("XSCOPE_GATEWAY_ADDRESS", "0.0.0.0:8080"),
            region: env_or("XSCOPE_REGION", "local"),
            model_revision: env_or("XSCOPE_MODEL_REVISION", "development"),
            price_version: env_or("XSCOPE_PRICE_VERSION", "2026-09-01"),
            usage_wal: env_or("XSCOPE_USAGE_WAL", "/tmp/xscope-usage-v1.jsonl"),
            api_keys,
            upstreams,
            control_internal_url: env_or("XSCOPE_CONTROL_INTERNAL_URL", ""),
            internal_token: env_or("XSCOPE_INTERNAL_TOKEN", ""),
            policy_refresh_seconds: env_or("XSCOPE_POLICY_REFRESH_SECONDS", "5")
                .parse()
                .unwrap_or(5)
                .max(1),
            redis_url: env_or("XSCOPE_REDIS_URL", ""),
        })
    }
}

fn parse_json_env<T: for<'de> Deserialize<'de>>(
    name: &'static str,
    fallback: &str,
) -> Result<T, SettingsError> {
    serde_json::from_str(&env::var(name).unwrap_or_else(|_| fallback.to_owned()))
        .map_err(|source| SettingsError::Json { name, source })
}

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

const fn default_weight() -> usize {
    1
}
