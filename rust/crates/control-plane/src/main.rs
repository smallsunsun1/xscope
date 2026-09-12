mod api;
mod audit;
mod billing;
mod billing_feed;
mod billing_projection;
mod billing_review;
mod catalog;
mod clusters;
mod config;
mod error;
mod event_worker;
mod managed_pools;
mod payment_service;
mod refund_service;
mod releases;
mod repository;
mod tax;
mod traffic;
mod scaling;
mod gateway_proofs;

use std::process::ExitCode;

use anyhow::{Context, Result};
use sea_orm::{ConnectOptions, Database};
use sea_orm_migration::MigratorTrait;
use serde::Deserialize;
use xscope_domain::{ApiKeyRequest, Money, Project};
use xscope_migration::Migrator;

use crate::api::{AppState, internal_router, public_router};
use crate::config::Config;
use crate::repository::Repository;

#[derive(Deserialize)]
struct BootstrapApiKey {
    id: String,
    tenant_id: String,
    project_id: String,
    secret: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(error) = run().await {
        tracing::error!(error = ?error, "control plane stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<()> {
    let _telemetry = xscope_telemetry::init("xscope-control-plane")
        .context("initialize control-plane telemetry")?;
    let config = Config::from_env().context("load control-plane configuration")?;
    let mut options = ConnectOptions::new(config.database_url.clone());
    options
        .max_connections(4)
        .min_connections(0)
        .sqlx_logging(false);
    let database = Database::connect(options)
        .await
        .context("connect control-plane database")?;
    Migrator::up(&database, None)
        .await
        .context("apply control-plane database migrations")?;
    let repository = Repository::new(database);
    repository
        .bootstrap_catalog()
        .await
        .context("bootstrap model catalog")?;
    bootstrap(&repository, &config.bootstrap_api_keys_json)
        .await
        .context("bootstrap local API keys")?;
    repository
        .backfill_billing_accounts()
        .await
        .context("backfill billing accounts")?;

    let public_address = config.public_address;
    let internal_address = config.internal_address;
    let state = AppState::new(repository, config).context("create control-plane HTTP client")?;
    let public_listener = tokio::net::TcpListener::bind(public_address)
        .await
        .with_context(|| format!("bind public control-plane listener on {public_address}"))?;
    let internal_listener = tokio::net::TcpListener::bind(internal_address)
        .await
        .with_context(|| format!("bind internal control-plane listener on {internal_address}"))?;
    tracing::info!(address = %public_address, "control plane public API listening");
    tracing::info!(address = %internal_address, "control plane internal API listening");
    let internal_state = state.clone();
    let (worker_stop, worker_shutdown) = tokio::sync::watch::channel(false);
    let pending_monitor = tokio::spawn(billing::monitor_pending(
        state.repository.clone(),
        worker_shutdown.clone(),
    ));
    let worker = if std::env::var("XSCOPE_EVENT_WORKER_ENABLED").as_deref() == Ok("true") {
        Some(tokio::spawn(event_worker::run(
            state.repository.clone(),
            worker_shutdown,
        )))
    } else {
        None
    };
    let internal = tokio::spawn(async move {
        axum::serve(internal_listener, internal_router(internal_state)).await
    });
    axum::serve(public_listener, public_router(&state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve public control-plane API")?;
    internal.abort();
    let _ = worker_stop.send(true);
    let _ = pending_monitor.await;
    if let Some(worker) = worker {
        let _ = worker.await;
    }
    Ok(())
}

async fn bootstrap(repository: &Repository, raw: &str) -> Result<()> {
    if raw.trim().is_empty() {
        return Ok(());
    }
    let keys: Vec<BootstrapApiKey> = serde_json::from_str(raw)?;
    for key in keys {
        repository
            .ensure_bootstrap_api_key(
                Project {
                    id: key.project_id.clone(),
                    tenant_id: key.tenant_id.clone(),
                    name: "Local development".to_owned(),
                },
                ApiKeyRequest {
                    id: key.id,
                    tenant_id: key.tenant_id,
                    project_id: key.project_id,
                    name: "Local bootstrap key".to_owned(),
                    scopes: vec!["chat.completions".to_owned()],
                    allowed_models: vec!["xscope-demo".to_owned()],
                    expires_at: None,
                    rate_limit_rpm: 600,
                    rate_limit_tpm: 600_000,
                    monthly_budget: Money::cny(10_000),
                },
                &key.secret,
            )
            .await?;
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
