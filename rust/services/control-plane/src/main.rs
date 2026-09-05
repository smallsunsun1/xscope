mod api;
mod config;
mod error;
mod repository;

use std::process::ExitCode;

use sea_orm::{ConnectOptions, Database};
use sea_orm_migration::MigratorTrait;
use serde::Deserialize;
use tracing_subscriber::EnvFilter;
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
    init_tracing();
    if let Err(error) = run().await {
        tracing::error!(error = ?error, "control plane stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let mut options = ConnectOptions::new(config.database_url.clone());
    options
        .max_connections(4)
        .min_connections(0)
        .sqlx_logging(false);
    let database = Database::connect(options).await?;
    Migrator::up(&database, None).await?;
    let repository = Repository::new(database);
    bootstrap(&repository, &config.bootstrap_api_keys_json).await?;
    repository.backfill_billing_accounts().await?;

    let public_address = config.public_address;
    let internal_address = config.internal_address;
    let state = AppState::new(repository, config);
    let public_listener = tokio::net::TcpListener::bind(public_address).await?;
    let internal_listener = tokio::net::TcpListener::bind(internal_address).await?;
    tracing::info!(address = %public_address, "control plane public API listening");
    tracing::info!(address = %internal_address, "control plane internal API listening");
    let internal_state = state.clone();
    let internal = tokio::spawn(async move {
        axum::serve(internal_listener, internal_router(internal_state)).await
    });
    axum::serve(public_listener, public_router(&state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    internal.abort();
    Ok(())
}

async fn bootstrap(repository: &Repository, raw: &str) -> Result<(), Box<dyn std::error::Error>> {
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("xscope_control_plane=info,tower_http=info,sea_orm=warn")
    });
    if std::env::var("XSCOPE_LOG_FORMAT").as_deref() == Ok("json") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
