use std::collections::{BTreeSet, HashMap};
use std::net::ToSocketAddrs;
use std::time::Duration;

use anyhow::{Context, Result};
use pingora_core::prelude::{Opt, Server, background_service};
use pingora_load_balancing::discovery::Static;
use pingora_load_balancing::health_check::TcpHealthCheck;
use pingora_load_balancing::{Backend, Backends, LoadBalancer, selection::RoundRobin};
use pingora_proxy::http_proxy_service;
use xscope_gateway::auth::DynamicKeySet;
use xscope_gateway::billing::BillingAdmission;
use xscope_gateway::config::Settings;
use xscope_gateway::proxy::{Gateway, ServingTransport};
use xscope_gateway::quota::QuotaManager;
use xscope_gateway::usage::UsageSink;

fn main() {
    if let Err(error) = run() {
        tracing::error!(error = ?error, "gateway failed to start");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let _telemetry =
        xscope_telemetry::init("xscope-gateway").context("initialize gateway telemetry")?;
    let settings = Settings::from_env().context("load gateway configuration")?;
    let storage_settings = xscope_gateway::storage::StorageSettings::from_env()
        .context("load gateway WAL storage settings")?;
    let storage = xscope_gateway::storage::StorageBudget::new(
        std::path::Path::new(&settings.usage_wal),
        storage_settings,
    );
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

    let mut server = Server::new(Some(Opt::default())).context("create Pingora server")?;
    server.bootstrap();
    let mut transports = HashMap::new();
    for pool in std::iter::once(&settings.serving).chain(&settings.additional_serving) {
        let mut backends = BTreeSet::new();
        // Only transport addresses of a pool's serving entry, never model Pods.
        // Replica selection remains exclusively owned by that pool's EPP.
        for address in pool
            .address
            .to_socket_addrs()
            .with_context(|| format!("resolve serving pool {} address", pool.id))?
        {
            backends.insert(
                Backend::new(&address.to_string())
                    .with_context(|| format!("create serving backend for pool {}", pool.id))?,
            );
        }
        let discovered = Backends::new(Static::new(backends));
        let mut load_balancer = LoadBalancer::<RoundRobin>::from_backends(discovered);
        load_balancer.set_health_check(TcpHealthCheck::new());
        load_balancer.health_check_frequency = Some(Duration::from_secs(5));
        let background =
            background_service(&format!("serving transport {}", pool.id), load_balancer);
        transports.insert(
            pool.id.clone(),
            ServingTransport {
                config: pool.clone(),
                transport: background.task(),
            },
        );
        server.add_service(background);
    }

    let billing = if settings.billing_reservations {
        Some(
            BillingAdmission::open(
                std::path::Path::new(&settings.usage_wal),
                settings.control_internal_url.clone(),
                settings.internal_token.clone(),
                settings.model_context_tokens,
                settings.price_version.clone(),
                storage.clone(),
            )
            .context("open billing reservation WAL")?,
        )
    } else {
        None
    };
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
        storage,
    )
    .context("open usage event WAL")?;
    let gateway = Gateway {
        transports,
        serving: settings.serving,
        keys,
        quota,
        usage,
        billing,
    };

    let mut proxy = http_proxy_service(&server.configuration, gateway);
    proxy.add_tcp(&settings.listen);
    server.add_service(proxy);
    server.run_forever();
}
