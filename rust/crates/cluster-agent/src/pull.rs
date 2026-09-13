//! Outbound member agent; Kubernetes Lease elects one active member writer. No kubeconfig
//! leaves this process. Legacy inbound mutation routes are disabled in pull mode.
use anyhow::{Result, bail};
use kube::{
    Api, Client, ResourceExt,
    api::{DeleteParams, PostParams, Preconditions},
};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{io::Read, time::Duration};
use xscope_domain::cluster::{ClusterDelivery, ClusterReport};
use xscope_kubernetes::api::{ModelDeployment, validate, validate_name};

const OWNER: &str = "platform.xscope.io/desired-cluster";
const VERSION: &str = "platform.xscope.io/desired-version";
const DIGEST: &str = "platform.xscope.io/desired-sha256";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub prometheus_url: Option<String>,
    pub cluster_id: String,
    pub namespace: String,
    pub control_url: String,
    pub credential_file: String,
    #[serde(default)]
    pub allow_local_http: bool,
}
impl Config {
    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) = std::env::var_os("XSCOPE_CLUSTER_PULL_CONFIG_FILE") else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(8193)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 8192 {
            bail!("cluster pull configuration exceeds size limit");
        }
        let config: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid cluster pull configuration"))?;
        validate_name(&config.cluster_id, true)?;
        if let Some(base) = &config.prometheus_url {
            let url =
                reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("invalid Prometheus URL"))?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || (url.scheme() != "https"
                    && !(config.allow_local_http
                        && url.scheme() == "http"
                        && url.host_str().is_some_and(|h| {
                            h == "localhost"
                                || h.ends_with(".svc")
                                || h.ends_with(".svc.cluster.local")
                                || h.parse::<std::net::IpAddr>()
                                    .is_ok_and(|ip| ip.is_loopback())
                        })))
            {
                bail!("Prometheus requires HTTPS or explicitly allowed cluster-local HTTP");
            }
        }
        validate_name(&config.namespace, true)?;
        let url = reqwest::Url::parse(&config.control_url)
            .map_err(|_| anyhow::anyhow!("invalid control URL"))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || (url.scheme() != "https" && !(config.allow_local_http && url.scheme() == "http"))
            || (url.scheme() == "http"
                && !url.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                        || host.ends_with(".svc")
                        || host.ends_with(".svc.cluster.local")
                }))
            || !std::path::Path::new(&config.credential_file).is_absolute()
        {
            bail!(
                "cluster pull requires HTTPS without URL credentials and an absolute credential file"
            );
        }
        Ok(Some(config))
    }
    fn credential(&self) -> Result<String> {
        // Reload projected Secrets every poll: rotation does not require a rebuild.
        let mut bytes = Vec::new();
        std::fs::File::open(&self.credential_file)?
            .take(129)
            .read_to_end(&mut bytes)?;
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid cluster credential"))?
            .trim();
        if value.len() != 43
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            bail!("invalid cluster credential");
        }
        Ok(value.into())
    }
}

fn validate_delivery(
    config: &Config,
    delivery: &ClusterDelivery,
) -> Result<Vec<Option<ModelDeployment>>, &'static str> {
    let canonical = serde_json::to_value(&delivery.deployments).map_err(|_| "invalid_spec")?;
    let bytes = serde_json::to_vec(&canonical).map_err(|_| "invalid_spec")?;
    if delivery.cluster_id != config.cluster_id
        || delivery.namespace != config.namespace
        || delivery.version <= 0
        || delivery.lease_seconds != 60
        || delivery.deployments.len() > 100
        || bytes.len() > 256 * 1024
        || format!("{:x}", Sha256::digest(bytes)) != delivery.sha256
    {
        return Err("invalid_spec");
    }
    let mut names = std::collections::HashSet::new();
    delivery
        .deployments
        .iter()
        .map(|item| {
            validate_name(&item.name, true).map_err(|_| "invalid_spec")?;
            if !names.insert(&item.name) {
                return Err("invalid_spec");
            }
            if let Some(spec) = &item.spec {
                if item.delete_uid.is_some() {
                    return Err("invalid_spec");
                }
                let mut model = ModelDeployment::new(
                    &item.name,
                    serde_json::from_value(spec.clone()).map_err(|_| "invalid_spec")?,
                );
                model.metadata.namespace = Some(config.namespace.clone());
                validate(&mut model).map_err(|_| "invalid_spec")?;
                Ok(Some(model))
            } else if item
                .delete_uid
                .as_deref()
                .is_some_and(|uid| !uid.is_empty() && uid.len() <= 128)
            {
                Ok(None)
            } else {
                Err("invalid_spec")
            }
        })
        .collect()
}

pub async fn apply(
    client: Client,
    config: &Config,
    delivery: &ClusterDelivery,
) -> Result<(), &'static str> {
    let models = validate_delivery(config, delivery)?; // Validate every spec before the first mutation.
    let api: Api<ModelDeployment> = Api::namespaced(client, &config.namespace);
    for (item, desired) in delivery.deployments.iter().zip(models) {
        let current = api
            .get_opt(&item.name)
            .await
            .map_err(|_| "kubernetes_unavailable")?;
        if item
            .expected_uid
            .as_ref()
            .is_some_and(|uid| current.as_ref().and_then(|m| m.metadata.uid.as_ref()) != Some(uid))
        {
            return Err("identity_mismatch");
        }
        if let Some(current) = &current {
            if current.labels().get(OWNER) != Some(&config.cluster_id) {
                return Err("ownership_conflict");
            }
            let version = current
                .annotations()
                .get(VERSION)
                .and_then(|v| v.parse::<i64>().ok())
                .ok_or("ownership_conflict")?;
            if version > delivery.version
                || (version == delivery.version
                    && current.annotations().get(DIGEST) != Some(&delivery.sha256))
            {
                return Err("ownership_conflict");
            }
        }
        if let Some(mut desired) = desired {
            if let Some(current) = current {
                // Replace is RV-fenced and preserves unrelated metadata/status.
                desired.metadata = current.metadata;
                desired.status = current.status;
                desired
                    .labels_mut()
                    .insert(OWNER.into(), config.cluster_id.clone());
                desired
                    .annotations_mut()
                    .insert(VERSION.into(), delivery.version.to_string());
                desired
                    .annotations_mut()
                    .insert(DIGEST.into(), delivery.sha256.clone());
                api.replace(&item.name, &PostParams::default(), &desired)
                    .await
                    .map_err(|_| "kubernetes_unavailable")?;
            } else {
                desired
                    .labels_mut()
                    .insert(OWNER.into(), config.cluster_id.clone());
                desired
                    .annotations_mut()
                    .insert(VERSION.into(), delivery.version.to_string());
                desired
                    .annotations_mut()
                    .insert(DIGEST.into(), delivery.sha256.clone());
                api.create(&PostParams::default(), &desired)
                    .await
                    .map_err(|_| "kubernetes_unavailable")?;
            }
        } else if let Some(current) = current {
            if current.uid().as_deref() != item.delete_uid.as_deref() {
                return Err("ownership_conflict");
            }
            api.delete(
                &item.name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: current.uid(),
                        resource_version: current.resource_version(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(|_| "kubernetes_unavailable")?;
        }
    }
    Ok(())
}

async fn response(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        bail!("control plane rejected member request");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("member response interrupted"))?
    {
        if bytes.len() + chunk.len() > 512 * 1024 {
            bail!("member response exceeds bound");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid member response"))
}
async fn provider_idle(
    http: &reqwest::Client,
    base: &str,
    namespace: &str,
    service: &str,
    after: &str,
) -> bool {
    let Ok(query) = xscope_kubernetes::gateway_proof::idle_query(namespace, service, after) else {
        return false;
    };
    let Ok(mut url) = reqwest::Url::parse(&format!("{}/api/v1/query", base.trim_end_matches('/')))
    else {
        return false;
    };
    url.query_pairs_mut().append_pair("query", &query);
    let Ok(result) = http.get(url).send().await else {
        return false;
    };
    let Ok(value) = response(result).await else {
        return false;
    };
    value["status"] == "success"
        && value["data"]["resultType"] == "vector"
        && value["data"]["result"]
            .as_array()
            .is_some_and(|a| a.len() == 1 && a[0]["value"][1] == "0")
}

pub async fn run(client: Client, config: Config, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    use kube_leader_election::{LeaseLock, LeaseLockParams, LeaseLockResult};
    let lock = LeaseLock::new(
        client.clone(),
        &config.namespace,
        LeaseLockParams {
            holder_id: format!("member-{}", uuid::Uuid::now_v7()),
            lease_name: "xscope-member-writer".into(),
            lease_ttl: Duration::from_secs(15),
        },
    );
    let config = std::sync::Arc::new(config);
    let mut writer: Option<tokio::task::JoinHandle<()>> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tick.tick() => {
                let acquired = matches!(tokio::time::timeout(Duration::from_secs(3), lock.try_acquire_or_renew()).await, Ok(Ok(LeaseLockResult::Acquired(_))));
                if !acquired {
                    if let Some(task) = writer.take() { task.abort(); let _ = task.await; }
                    xscope_telemetry::background_event("member_lease", "standby");
                } else if writer.as_ref().is_none_or(|task|task.is_finished()) {
                    writer = Some(tokio::spawn(run_active(client.clone(), config.clone(), shutdown.clone())));
                    xscope_telemetry::background_event("member_lease", "acquired");
                }
            }
        }
    }
    if let Some(task) = writer {
        task.abort();
        let _ = task.await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(3), lock.step_down()).await;
}
async fn run_active(
    client: Client,
    config: std::sync::Arc<Config>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    use tracing::Instrument;
    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(http) => http,
        Err(_) => {
            tracing::error!("member HTTP client unavailable");
            return;
        }
    };
    let base = format!(
        "{}/internal/v1/clusters/{}",
        config.control_url.trim_end_matches('/'),
        config.cluster_id
    );
    let mut delay = 15;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let work = async {
            let token = config.credential()?;
            let poll = http
                .post(format!("{base}/poll"))
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("member poll unavailable"))?;
            let value = response(poll).await?;
            if !value["delivery"].is_null() {
                let delivery: ClusterDelivery =
                    serde_json::from_value(value["delivery"].clone())
                        .map_err(|_| anyhow::anyhow!("invalid delivery contract"))?;
                let result = tokio::time::timeout(
                    Duration::from_secs(40),
                    apply(client.clone(), &config, &delivery),
                )
                .await
                .unwrap_or(Err("apply_timeout"));
                let report = ClusterReport {
                    version: delivery.version,
                    sha256: delivery.sha256,
                    lease: delivery.lease,
                    outcome: if result.is_ok() {
                        "applied"
                    } else {
                        "rejected"
                    }
                    .into(),
                    error_code: result.err().map(str::to_owned),
                };
                let result = http
                    .post(format!("{base}/report"))
                    .bearer_auth(&token)
                    .json(&report)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("member report unavailable"))?;
                response(result).await?;
                xscope_telemetry::background_event(
                    "cluster_pull",
                    if report.outcome == "applied" {
                        "ack"
                    } else {
                        "nack"
                    },
                );
            }
            let result = http
                .get(format!("{base}/observations"))
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("member observations unavailable"))?;
            if result.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok::<(), anyhow::Error>(());
            }
            let tasks = response(result).await?;
            let tasks: Vec<xscope_domain::traffic::ObservationTask> =
                serde_json::from_value(tasks["tasks"].clone())
                    .map_err(|_| anyhow::anyhow!("invalid observation tasks"))?;
            if tasks.len() > 100 {
                bail!("too many observation tasks");
            }
            for task in tasks {
                let mut report = tokio::time::timeout(
                    Duration::from_secs(5),
                    xscope_kubernetes::observation::observe(
                        client.clone(),
                        &config.namespace,
                        &config.cluster_id,
                        &task,
                    ),
                )
                .await
                .map_err(|_| anyhow::anyhow!("observation deadline exceeded"))?;
                if let (Some(after), Some(prometheus)) = (&task.idle_after, &config.prometheus_url)
                {
                    report.idle = provider_idle(
                        &http,
                        prometheus,
                        &config.namespace,
                        &task.serving_service,
                        after,
                    )
                    .await;
                }
                let result = http
                    .post(format!("{base}/observations"))
                    .bearer_auth(&token)
                    .json(&report)
                    .send()
                    .await
                    .map_err(|_| anyhow::anyhow!("member observation report unavailable"))?;
                response(result).await?;
            }
            let result = http
                .get(format!("{base}/gateway-proofs"))
                .bearer_auth(&token)
                .send()
                .await
                .map_err(|_| anyhow::anyhow!("gateway proof tasks unavailable"))?;
            if result.status() != reqwest::StatusCode::NOT_FOUND {
                let value = response(result).await?;
                let tasks: Vec<xscope_domain::traffic::GatewayProofTask> =
                    serde_json::from_value(value["tasks"].clone())
                        .map_err(|_| anyhow::anyhow!("invalid gateway proof tasks"))?;
                if tasks.len() > 100 {
                    bail!("too many gateway proof tasks");
                }
                for task in tasks {
                    let proof = tokio::time::timeout(
                        Duration::from_secs(5),
                        xscope_kubernetes::gateway_proof::observe(
                            client.clone(),
                            &http,
                            &config.namespace,
                            &task,
                        ),
                    )
                    .await
                    .map_err(|_| anyhow::anyhow!("gateway proof deadline"))?;
                    response(
                        http.post(format!("{base}/gateway-proofs"))
                            .bearer_auth(&token)
                            .json(&proof)
                            .send()
                            .await
                            .map_err(|_| anyhow::anyhow!("gateway proof report unavailable"))?,
                    )
                    .await?;
                }
            }
            Ok(())
        }
        .instrument(tracing::info_span!("cluster.reconcile"));
        tokio::select! { _ = shutdown.changed() => return, result = work => match result {
            Ok(()) => delay = 15,
            Err(_) => { delay = std::cmp::min(delay * 2, 60); xscope_telemetry::background_event("cluster_pull", "retry"); tracing::warn!("member reconciliation retry; credentials and API details suppressed"); }
        }}
        tokio::select! { _ = shutdown.changed() => return, _ = tokio::time::sleep(Duration::from_secs(delay)) => {} }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Response,
    };
    use std::sync::{Arc, Mutex};
    use tower::service_fn;
    fn delivery() -> ClusterDelivery {
        let deployments = vec![xscope_domain::cluster::DesiredDeployment {
            expected_uid: None,
            name: "synthetic-model".into(),
            delete_uid: None,
            spec: Some(
                serde_json::json!({"model":{"id":"demo","revision":"v1","uri":"s3://example/demo","checksum":format!("sha256:{}","a".repeat(64))},
                "runtime":{"image":"example/runtime:v1","protocol":"openai"},"replicas":0,"resources":{}}),
            ),
        }];
        let sha256 = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&serde_json::to_value(&deployments).unwrap()).unwrap()
            )
        );
        ClusterDelivery {
            cluster_id: "synthetic-cluster".into(),
            namespace: "synthetic-member".into(),
            version: 1,
            sha256,
            lease: "synthetic-lease".into(),
            lease_seconds: 60,
            deployments,
        }
    }
    fn config() -> Config {
        Config {
            prometheus_url: None,
            cluster_id: "synthetic-cluster".into(),
            namespace: "synthetic-member".into(),
            control_url: "https://example.invalid".into(),
            credential_file: "/synthetic-credential".into(),
            allow_local_http: false,
        }
    }
    #[tokio::test]
    async fn applies_only_owned_validated_specs_and_uid_fenced_deletes() {
        let stored = Arc::new(Mutex::new(None::<Value>));
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let (objects, methods) = (stored.clone(), calls.clone());
        let client = Client::new(
            service_fn(move |request: axum::http::Request<kube::client::Body>| {
                let (objects, methods) = (objects.clone(), methods.clone());
                async move {
                    assert!(request.uri().path().starts_with("/apis/platform.xscope.io/v1alpha1/namespaces/synthetic-member/modeldeployments"));
                    let method = request.method().as_str().to_owned();
                    methods.lock().unwrap().push(method.clone());
                    let body = to_bytes(Body::new(request.into_body()), 1 << 20)
                        .await
                        .unwrap();
                    let mut stored = objects.lock().unwrap();
                    let mut status = 200;
                    let result = match method.as_str() {
                    "GET" => stored.clone().unwrap_or_else(|| { status = 404; serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":"NotFound","message":"fixture missing","code":404}) }),
                    "POST" | "PUT" => {
                        let mut value: Value = serde_json::from_slice(&body).unwrap();
                        if method == "PUT" { assert_eq!(value["metadata"]["resourceVersion"], "synthetic-rv"); }
                        value["metadata"]["uid"] = serde_json::json!("synthetic-uid");
                        value["metadata"]["resourceVersion"] = serde_json::json!("synthetic-rv");
                        *stored = Some(value.clone()); value
                    }
                    "DELETE" => {
                        let options: Value = serde_json::from_slice(&body).unwrap();
                        assert_eq!(options["preconditions"]["uid"], "synthetic-uid");
                        assert_eq!(options["preconditions"]["resourceVersion"], "synthetic-rv");
                        *stored = None; serde_json::json!({"apiVersion":"v1","kind":"Status","status":"Success"})
                    }
                    _ => panic!("unexpected fixture operation"),
                };
                    Ok::<_, std::convert::Infallible>(
                        Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Body::from(result.to_string()))
                            .unwrap(),
                    )
                }
            }),
            "synthetic-member",
        );
        let mut delivery = delivery();
        assert!(apply(client.clone(), &config(), &delivery).await.is_ok());
        assert!(apply(client.clone(), &config(), &delivery).await.is_ok());
        assert_eq!(
            stored.lock().unwrap().as_ref().unwrap()["spec"]["replicas"],
            0
        );
        let count = calls.lock().unwrap().len();
        delivery.namespace = "foreign-namespace".into();
        assert_eq!(
            apply(client.clone(), &config(), &delivery).await,
            Err("invalid_spec")
        );
        assert_eq!(calls.lock().unwrap().len(), count); // no Kubernetes mutation on invalid batch.
        delivery.namespace = "synthetic-member".into();
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["labels"][OWNER] =
            serde_json::json!("foreign");
        assert_eq!(
            apply(client.clone(), &config(), &delivery).await,
            Err("ownership_conflict")
        );
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["labels"][OWNER] =
            serde_json::json!("synthetic-cluster");
        stored.lock().unwrap().as_mut().unwrap()["metadata"]["annotations"][VERSION] =
            serde_json::json!("2");
        assert_eq!(
            apply(client.clone(), &config(), &delivery).await,
            Err("ownership_conflict")
        );
        delivery.version = 3;
        delivery.deployments[0].spec = None;
        delivery.deployments[0].delete_uid = Some("wrong-uid".into());
        delivery.sha256 = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&serde_json::to_value(&delivery.deployments).unwrap()).unwrap()
            )
        );
        assert_eq!(
            apply(client.clone(), &config(), &delivery).await,
            Err("ownership_conflict")
        );
        delivery.deployments[0].delete_uid = Some("synthetic-uid".into());
        delivery.sha256 = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&serde_json::to_value(&delivery.deployments).unwrap()).unwrap()
            )
        );
        assert!(apply(client.clone(), &config(), &delivery).await.is_ok());
        assert!(stored.lock().unwrap().is_none());
        assert!(apply(client, &config(), &delivery).await.is_ok()); // lost delete reply is idempotent.
    }
}
