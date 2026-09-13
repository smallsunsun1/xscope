use std::collections::BTreeSet;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use thiserror::Error;

#[derive(Clone, Debug)]
pub struct Config {
    pub public_address: SocketAddr,
    pub internal_address: SocketAddr,
    pub database_url: String,
    pub database_pool: DatabasePool,
    pub internal_token: String,
    pub alert_webhook_token: Option<String>,
    pub ops_prometheus_url: Option<String>,
    pub console_auth: bool,
    pub console_directory: Option<PathBuf>,
    pub cluster_agent_url: String,
    pub default_tenant_id: String,
    pub auto_join_default_tenant: bool,
    pub bootstrap_admin_users: BTreeSet<String>,
    pub platform_admin_subjects: BTreeSet<String>,
    pub billing_reviewer_subjects: BTreeSet<String>,
    pub bootstrap_api_keys_json: String,
    pub component_targets: Vec<(String, String)>,
    pub route_pools: Vec<xscope_domain::RoutePool>,
}

#[derive(Clone, Debug)]
pub struct DatabasePool {
    pub max: u32,
    pub min: u32,
    pub acquire_ms: u64,
}
impl DatabasePool {
    fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let number = |name, fallback, min, max| {
            get(name)
                .map_or(Ok(fallback), |v| v.parse::<u32>())
                .ok()
                .filter(|v| (min..=max).contains(v))
                .ok_or_else(|| ConfigError::Invalid {
                    name,
                    message: "outside supported range".into(),
                })
        };
        let max = number("XSCOPE_DB_MAX_CONNECTIONS", 4, 1, 128)?;
        Ok(Self {
            max,
            min: number("XSCOPE_DB_MIN_CONNECTIONS", 0, 0, max)?,
            acquire_ms: u64::from(number("XSCOPE_DB_ACQUIRE_TIMEOUT_MS", 2000, 100, 30000)?),
        })
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{0} must be set")]
    Missing(&'static str),
    #[error("{name} is invalid: {message}")]
    Invalid { name: &'static str, message: String },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url = required("XSCOPE_DATABASE_URL")?;
        let internal_token = required("XSCOPE_INTERNAL_TOKEN")?;
        let console_directory = env::var_os("XSCOPE_CONSOLE_DIR").map(PathBuf::from);
        let bootstrap_admin_users = env_or("XSCOPE_BOOTSTRAP_ADMIN_USERS", "platform-admin")
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(Self {
            route_pools: serde_json::from_str(&env_or(
                "XSCOPE_ROUTE_POOLS_JSON",
                r#"[{"id":"demo-pool","model":"xscope-demo","revision":"development"}]"#,
            ))
            .map_err(|error| ConfigError::Invalid {
                name: "XSCOPE_ROUTE_POOLS_JSON",
                message: error.to_string(),
            })?,
            public_address: address("XSCOPE_CONTROL_ADDRESS", "0.0.0.0:8081")?,
            internal_address: address("XSCOPE_CONTROL_INTERNAL_ADDRESS", "0.0.0.0:8084")?,
            database_url,
            database_pool: DatabasePool::parse(|key| env::var(key).ok())?,
            internal_token,
            alert_webhook_token: env::var("XSCOPE_ALERT_WEBHOOK_TOKEN")
                .ok()
                .filter(|v| v.len() >= 32),
            ops_prometheus_url: env::var("XSCOPE_OPS_PROMETHEUS_URL").ok(),
            console_auth: env_or("XSCOPE_CONSOLE_AUTH", "disabled") == "trusted-headers",
            console_directory,
            cluster_agent_url: env_or("XSCOPE_CLUSTER_AGENT_URL", "http://cluster-agent:8083"),
            default_tenant_id: env_or("XSCOPE_DEFAULT_TENANT_ID", "tenant-local"),
            auto_join_default_tenant: env_or("XSCOPE_AUTO_JOIN_DEFAULT_TENANT", "false") == "true",
            bootstrap_admin_users,
            platform_admin_subjects: env_or("XSCOPE_PLATFORM_ADMIN_SUBJECTS", "")
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect(),
            billing_reviewer_subjects: env_or("XSCOPE_BILLING_REVIEWER_SUBJECTS", "")
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect(),
            bootstrap_api_keys_json: env::var("XSCOPE_BOOTSTRAP_API_KEYS_JSON").unwrap_or_default(),
            component_targets: vec![
                (
                    "gateway".to_owned(),
                    env_or("XSCOPE_GATEWAY_HEALTH_URL", "http://gateway:80/healthz"),
                ),
                (
                    "runtime".to_owned(),
                    env_or("XSCOPE_RUNTIME_HEALTH_URL", "http://runtime:8090/healthz"),
                ),
                (
                    "operator".to_owned(),
                    env_or("XSCOPE_OPERATOR_HEALTH_URL", "http://operator:8082/healthz"),
                ),
                (
                    "cluster-agent".to_owned(),
                    format!(
                        "{}/healthz",
                        env_or("XSCOPE_CLUSTER_AGENT_URL", "http://cluster-agent:8083")
                            .trim_end_matches('/')
                    ),
                ),
            ],
        })
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(ConfigError::Missing(name))
}

fn env_or(name: &str, fallback: &str) -> String {
    env::var(name).unwrap_or_else(|_| fallback.to_owned())
}

fn address(name: &'static str, fallback: &str) -> Result<SocketAddr, ConfigError> {
    SocketAddr::from_str(&env_or(name, fallback)).map_err(|error| ConfigError::Invalid {
        name,
        message: error.to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn database_pool_rejects_unbounded_or_inverted_configuration() {
        assert_eq!(DatabasePool::parse(|_| None).unwrap().max, 4);
        assert!(
            DatabasePool::parse(|key| (key == "XSCOPE_DB_MIN_CONNECTIONS").then(|| "5".into()))
                .is_err()
        );
        assert!(
            DatabasePool::parse(|key| (key == "XSCOPE_DB_MAX_CONNECTIONS").then(|| "0".into()))
                .is_err()
        );
    }
}
