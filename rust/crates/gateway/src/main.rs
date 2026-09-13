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
    let delivery_settings =
        xscope_gateway::delivery::Settings::from_env().context("load usage delivery settings")?;
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
        settings.gateway_identity.clone(),
    );
    let quota = QuotaManager::new(&settings.redis_url);
    if quota.is_distributed() {
        tracing::info!("Redis-backed distributed RPM/TPM quota is enabled");
    } else {
        tracing::warn!("Redis quota is disabled; using per-process RPM fallback without TPM");
    }

    let mut server = Server::new(Some(Opt::default())).context("create Pingora server")?;
    let configuration = std::sync::Arc::get_mut(&mut server.configuration)
        .context("configure Pingora graceful shutdown")?;
    configuration.grace_period_seconds = Some(delivery_settings.grace_seconds);
    configuration.graceful_shutdown_timeout_seconds = Some(5);
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
            BillingAdmission::new(
                settings.control_internal_url.clone(),
                settings.internal_token.clone(),
                settings.model_context_tokens,
                settings.price_version.clone(),
            )
            .context("initialize HTTP admission")?,
        )
    } else {
        None
    };
    let report_url = format!(
        "{}/usage-events",
        settings.control_internal_url.trim_end_matches('/')
    );
    let queue = xscope_gateway::delivery::Queue::start(
        delivery_settings,
        report_url,
        settings.internal_token,
    )
    .context("initialize volatile HTTP reporting")?;
    server.add_service(background_service(
        "usage admission drain",
        xscope_gateway::delivery::Drain(queue.clone()),
    ));
    tracing::warn!(
        "usage reporting is volatile: abrupt process/Pod loss requires central pending review"
    );
    let usage = UsageSink::new(
        queue.clone(),
        settings.region,
        settings.model_revision,
        settings.price_version,
    );
    let gateway = Gateway {
        transports,
        serving: settings.serving,
        keys: keys.clone(),
        quota,
        usage,
        billing,
    };

    let mut proxy = http_proxy_service(&server.configuration, gateway);
    proxy.add_tcp(&settings.listen);
    server.add_service(proxy);
    // Unlike run_forever(), run() returns instead of process::exit(), allowing
    // the HTTP outbox and telemetry to drain after request runtimes stop.
    server.run(Default::default());
    keys.finish();
    queue.finish();
    Ok(())
}
