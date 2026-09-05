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
    pub serving: ServingConfig,
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
#[serde(deny_unknown_fields)]
pub struct ServingConfig {
    pub id: String,
    pub model: String,
    pub address: String,
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
    #[error(
        "XSCOPE_UPSTREAMS_JSON is obsolete; configure one serving entry with XSCOPE_SERVING_ENTRY_JSON"
    )]
    LegacyUpstreams,
    #[error("serving entry id, model and address must be nonempty")]
    InvalidServingEntry,
}

impl Settings {
    /// Loads gateway settings from the `XSCOPE_*` environment variables.
    ///
    /// # Errors
    ///
    /// Returns [`SettingsError`] when settings are malformed or obsolete.
    pub fn from_env() -> Result<Self, SettingsError> {
        let api_keys = parse_json_env("XSCOPE_API_KEYS_JSON", "[]")?;
        if env::var_os("XSCOPE_UPSTREAMS_JSON").is_some() {
            return Err(SettingsError::LegacyUpstreams);
        }
        let serving: ServingConfig = parse_json_env(
            "XSCOPE_SERVING_ENTRY_JSON",
            r#"{"id":"demo-pool","model":"xscope-demo","address":"127.0.0.1:8085"}"#,
        )?;
        serving.validate()?;
        Ok(Self {
            listen: env_or("XSCOPE_GATEWAY_ADDRESS", "0.0.0.0:8080"),
            region: env_or("XSCOPE_REGION", "local"),
            model_revision: env_or("XSCOPE_MODEL_REVISION", "development"),
            price_version: env_or("XSCOPE_PRICE_VERSION", "2026-09-01"),
            usage_wal: env_or("XSCOPE_USAGE_WAL", "/tmp/xscope-usage-v1.jsonl"),
            api_keys,
            serving,
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

impl ServingConfig {
    fn validate(&self) -> Result<(), SettingsError> {
        if [&self.id, &self.model, &self.address]
            .iter()
            .any(|value| value.trim().is_empty())
        {
            return Err(SettingsError::InvalidServingEntry);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ServingConfig;

    #[test]
    fn serving_entry_requires_identity_and_rejects_legacy_weights() {
        let entry: ServingConfig =
            serde_json::from_str(r#"{"id":"pool-a","model":"demo","address":"serving:8085"}"#)
                .unwrap();
        assert!(entry.validate().is_ok());
        let invalid = ServingConfig {
            model: String::new(),
            ..entry
        };
        assert!(invalid.validate().is_err());
        assert!(
            serde_json::from_str::<ServingConfig>(
                r#"{"id":"pod-a","model":"demo","address":"10.0.0.1:8000","weight":100}"#,
            )
            .is_err()
        );
    }
}
