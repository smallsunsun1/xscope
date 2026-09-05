use std::collections::BTreeSet;
use std::error::Error;
use std::net::ToSocketAddrs;
use std::time::Duration;

use pingora_core::prelude::{Opt, Server, background_service};
use pingora_load_balancing::discovery::Static;
use pingora_load_balancing::health_check::TcpHealthCheck;
use pingora_load_balancing::{Backend, Backends, LoadBalancer, selection::RoundRobin};
use pingora_proxy::http_proxy_service;
use xscope_gateway::auth::DynamicKeySet;
use xscope_gateway::config::Settings;
use xscope_gateway::proxy::Gateway;
use xscope_gateway::quota::QuotaManager;
use xscope_gateway::usage::UsageSink;

fn main() {
    let _telemetry = xscope_telemetry::init("xscope-gateway").expect("telemetry initialization");
    if let Err(error) = run() {
        tracing::error!(error = %error, "gateway failed to start");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let settings = Settings::from_env()?;
    let keys = DynamicKeySet::new(&settings.api_keys);
    if keys.is_empty() {
        tracing::warn!(
            config = "XSCOPE_API_KEYS_JSON",
            "API key configuration is empty; readiness and inference will fail closed"
        );
    }
    keys.spawn_refresh(
        settings.control_internal_url.clone(),
        settings.internal_token.clone(),
        Duration::from_secs(settings.policy_refresh_seconds),
    );
    let quota = QuotaManager::new(&settings.redis_url);
    if quota.is_distributed() {
        tracing::info!("Redis-backed distributed RPM/TPM quota is enabled");
    } else {
        tracing::warn!("Redis quota is disabled; using per-process RPM fallback without TPM");
    }

    let mut backends = BTreeSet::new();
    // Only transport addresses of ONE serving entry, never model Pod inventory.
    // InferencePool discovery and model replica selection belong to llm-d EPP.
    for address in settings.serving.address.to_socket_addrs()? {
        backends.insert(Backend::new(&address.to_string())?);
    }
    let discovered = Backends::new(Static::new(backends));
    let mut load_balancer = LoadBalancer::<RoundRobin>::from_backends(discovered);
    load_balancer.set_health_check(TcpHealthCheck::new());
    load_balancer.health_check_frequency = Some(Duration::from_secs(5));
    let background = background_service("serving entry transport health", load_balancer);

    let usage = UsageSink::open(
        &settings.usage_wal,
        settings.region,
        settings.model_revision,
        settings.price_version,
        if settings.control_internal_url.is_empty() {
            String::new()
        } else {
            format!(
                "{}/usage-events",
                settings.control_internal_url.trim_end_matches('/')
            )
        },
        settings.internal_token,
    )?;
    let gateway = Gateway {
        serving_transport: background.task(),
        serving: settings.serving,
        keys,
        quota,
        usage,
    };

    let mut server = Server::new(Some(Opt::default()))?;
    server.bootstrap();
    let mut proxy = http_proxy_service(&server.configuration, gateway);
    proxy.add_tcp(&settings.listen);
    server.add_service(background);
    server.add_service(proxy);
    server.run_forever();
}
