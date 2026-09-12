use std::env;

use serde::Deserialize;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Settings {
    pub listen: String,
    pub region: String,
    pub model_revision: String,
    pub price_version: String,
    pub api_keys: Vec<ApiKeyConfig>,
    pub serving: ServingConfig,
    pub additional_serving: Vec<ServingConfig>,
    pub control_internal_url: String,
    pub internal_token: String,
    pub policy_refresh_seconds: u64,
    pub redis_url: String,
    pub billing_reservations: bool,
    pub model_context_tokens: i64,
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
    pub revision: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub server_name: String,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("WAL mode was removed; XSCOPE_USAGE_MODE may only be memory")]
    InvalidUsageMode,
    #[error("HTTP usage reporting requires XSCOPE_CONTROL_INTERNAL_URL and XSCOPE_INTERNAL_TOKEN")]
    MissingUsageReporter,
    #[error(
        "billing reservations require an internal URL/token and a positive model context; XSCOPE_BILLING_RESERVATIONS must be true or false"
    )]
    InvalidBilling,
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
    #[error("serving pool IDs must be unique and revisions must be nonempty")]
    InvalidServingPools,
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
        let mut serving: ServingConfig = parse_json_env(
            "XSCOPE_SERVING_ENTRY_JSON",
            r#"{"id":"demo-pool","model":"xscope-demo","address":"127.0.0.1:8085"}"#,
        )?;
        let model_revision = env_or("XSCOPE_MODEL_REVISION", "development");
        if serving.revision.is_empty() {
            serving.revision.clone_from(&model_revision);
        }
        serving.validate()?;
        let additional_serving: Vec<ServingConfig> =
            parse_json_env("XSCOPE_ADDITIONAL_SERVING_JSON", "[]")?;
        let mut ids = std::collections::HashSet::from([serving.id.clone()]);
        for pool in &additional_serving {
            pool.validate()?;
            if pool.revision.trim().is_empty() || !ids.insert(pool.id.clone()) {
                return Err(SettingsError::InvalidServingPools);
            }
        }
        let control_internal_url = env_or("XSCOPE_CONTROL_INTERNAL_URL", "");
        let internal_token = env_or("XSCOPE_INTERNAL_TOKEN", "");
        let usage_mode = env_or("XSCOPE_USAGE_MODE", "memory");
        // Refuse stale manifests instead of silently changing their durability contract.
        if usage_mode != "memory" {
            return Err(SettingsError::InvalidUsageMode);
        }
        if control_internal_url.is_empty() || internal_token.is_empty() {
            return Err(SettingsError::MissingUsageReporter);
        }
        let billing_reservations = env_or("XSCOPE_BILLING_RESERVATIONS", "false")
            .parse::<bool>()
            .map_err(|_| SettingsError::InvalidBilling)?;
        let model_context_tokens = env_or("XSCOPE_MODEL_CONTEXT_TOKENS", "32768")
            .parse::<i64>()
            .map_err(|_| SettingsError::InvalidBilling)?;
        if model_context_tokens <= 0
            || (billing_reservations
                && (control_internal_url.is_empty() || internal_token.is_empty()))
        {
            return Err(SettingsError::InvalidBilling);
        }
        Ok(Self {
            listen: env_or("XSCOPE_GATEWAY_ADDRESS", "0.0.0.0:8080"),
            region: env_or("XSCOPE_REGION", "local"),
            model_revision,
            price_version: env_or("XSCOPE_PRICE_VERSION", "2026-09-01"),
            api_keys,
            serving,
            additional_serving,
            control_internal_url,
            internal_token,
            billing_reservations,
            model_context_tokens,
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
        if xscope_domain::traffic::managed(&self.id)
            || [&self.id, &self.model, &self.address]
                .iter()
                .any(|value| value.trim().is_empty())
            || [&self.id, &self.revision]
                .iter()
                .any(|value| value.len() > 128 || http::HeaderValue::from_str(value).is_err())
        {
            return Err(SettingsError::InvalidServingEntry);
        }
        Ok(())
    }
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
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
