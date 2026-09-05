use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::net::ToSocketAddrs;
use std::time::Duration;

use pingora_core::prelude::{Opt, Server, background_service};
use pingora_load_balancing::discovery::Static;
use pingora_load_balancing::health_check::TcpHealthCheck;
use pingora_load_balancing::{Backend, Backends, LoadBalancer, selection::RoundRobin};
use pingora_proxy::http_proxy_service;
use xscope_gateway::auth::KeySet;
use xscope_gateway::config::Settings;
use xscope_gateway::proxy::Gateway;
use xscope_gateway::usage::UsageSink;

fn main() {
    init_tracing();
    if let Err(error) = run() {
        tracing::error!(error = %error, "gateway failed to start");
        std::process::exit(1);
    }
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let format = std::env::var("XSCOPE_LOG_FORMAT").unwrap_or_else(|_| "compact".to_owned());

    if format.eq_ignore_ascii_case("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .compact()
            .init();
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let settings = Settings::from_env()?;
    let keys = KeySet::new(&settings.api_keys);
    if keys.is_empty() {
        tracing::warn!(
            config = "XSCOPE_API_KEYS_JSON",
            "API key configuration is empty; readiness and inference will fail closed"
        );
    }

    let mut backends = BTreeSet::new();
    let mut endpoint_by_address = HashMap::new();
    for endpoint in &settings.upstreams {
        for address in endpoint.address.to_socket_addrs()? {
            backends.insert(Backend::new_with_weight(
                &address.to_string(),
                endpoint.weight,
            )?);
            endpoint_by_address.insert(address.to_string(), endpoint.clone());
        }
    }
    let discovered = Backends::new(Static::new(backends));
    let mut load_balancer = LoadBalancer::<RoundRobin>::from_backends(discovered);
    load_balancer.set_health_check(TcpHealthCheck::new());
    load_balancer.health_check_frequency = Some(Duration::from_secs(5));
    let background = background_service("model endpoint discovery and health", load_balancer);

    let usage = UsageSink::open(
        &settings.usage_wal,
        settings.region,
        settings.model_revision,
        settings.price_version,
    )?;
    let gateway = Gateway {
        upstreams: background.task(),
        endpoint_by_address,
        keys,
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
