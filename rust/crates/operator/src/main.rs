use anyhow::{Context as _, Result};
use axum::{Router, routing::get};
use futures::StreamExt;
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Service};
use kube::{
    Api, Client,
    runtime::{Controller, watcher},
};
use kube_leader_election::{LeaseLock, LeaseLockParams, LeaseLockResult};
use std::{sync::Arc, time::Duration};
use xscope_kubernetes::{
    api::ModelDeployment,
    controller::{Context, error_policy, reconcile},
    pool::InferencePool,
    resources::{Hpa, Pdb},
};

fn env(name: &str, fallback: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| fallback.into())
}

#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {
    let _telemetry =
        xscope_telemetry::init("xscope-operator").context("initialize operator telemetry")?;
    let client = Client::try_default()
        .await
        .context("create operator Kubernetes client")?;
    let lock = LeaseLock::new(
        client.clone(),
        &env("XSCOPE_OPERATOR_NAMESPACE", "xscope-system"),
        LeaseLockParams {
            holder_id: format!("{}-{}", env("HOSTNAME", "operator"), uuid::Uuid::now_v7()),
            lease_name: "xscope-operator.platform.xscope.io".into(),
            lease_ttl: Duration::from_secs(15),
        },
    );
    let context = Arc::new(Context {
        client: client.clone(),
        cluster_id: env("XSCOPE_CLUSTER_ID", "local"),
        region: env("XSCOPE_REGION", "local"),
        prometheus_address: env(
            "XSCOPE_AUTOSCALING_PROMETHEUS_URL",
            "http://prometheus.xscope-system.svc:9090",
        ),
    });
    let address = env("XSCOPE_OPERATOR_ADDRESS", "0.0.0.0:8082");
    let listener = tokio::net::TcpListener::bind(&address)
        .await
        .with_context(|| format!("bind operator health listener on {address}"))?;
    let health = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/healthz", get(|| async { "ok" }))
                .route("/readyz", get(|| async { "ready" })),
        )
        .await
    });
    let mut controller: Option<tokio::task::JoinHandle<()>> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let shutdown = async {
        #[cfg(unix)]
        {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut terminate) => {
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
                }
                Err(error) => {
                    tracing::warn!(%error, "SIGTERM handler unavailable; waiting for Ctrl-C");
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = tick.tick() => {
                // Stop reconciliation immediately on renewal failure, before lease expiry.
                let acquired = matches!(tokio::time::timeout(Duration::from_secs(3), lock.try_acquire_or_renew()).await,
                    Ok(Ok(LeaseLockResult::Acquired(_))));
                if !acquired {
                    if let Some(task) = controller.take() {
                        task.abort(); let _ = task.await;
                        tracing::warn!("leadership lost; reconciliation stopped");
                    }
                    xscope_telemetry::background_event("lease", "standby");
                } else if controller.as_ref().is_none_or(|task| task.is_finished()) {
                    tracing::info!("leadership acquired; starting kube-rs controller");
                    let client = client.clone(); let context = context.clone();
                    controller = Some(tokio::spawn(async move {
                        Controller::new(Api::<ModelDeployment>::all(client.clone()), watcher::Config::default())
                            .owns(Api::<Deployment>::all(client.clone()), watcher::Config::default())
                            .owns(Api::<Service>::all(client.clone()), watcher::Config::default())
                            .owns(Api::<Hpa>::all(client.clone()), watcher::Config::default())
                            .owns(Api::<Pdb>::all(client.clone()), watcher::Config::default())
                            .owns(Api::<xscope_kubernetes::recommendation::ModelScale>::all(client.clone()), watcher::Config::default())
                            .owns(Api::<InferencePool>::all(client), watcher::Config::default())
                            .run(reconcile, error_policy, context)
                            .for_each(|result| async { if let Err(error) = result { tracing::warn!(%error,"controller stream error"); } }).await;
                    }));
                }
            }
        }
    }
    if let Some(task) = controller {
        task.abort();
        let _ = task.await;
    }
    // Do not step down while an in-flight reconciler still holds the lease.
    let _ = tokio::time::timeout(Duration::from_secs(3), lock.step_down()).await;
    health.abort();
    Ok(())
}
