//! Member-side readiness evidence. ACK of an applied spec is not readiness.
use crate::{
    api::{ModelDeployment, validate, validate_name},
    controller::ensure_owner,
    pool::InferencePool,
    resources,
};
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Service};
use kube::{Api, Client, ResourceExt};
use xscope_domain::traffic::{ObservationTask, PoolObservation};

fn deployment_ready(deployment: &Deployment, replicas: i32) -> bool {
    replicas > 0
        && deployment.metadata.deletion_timestamp.is_none()
        && deployment.spec.as_ref().and_then(|s| s.replicas) == Some(replicas)
        && deployment.status.as_ref().is_some_and(|s| {
            s.observed_generation.unwrap_or_default()
                >= deployment.metadata.generation.unwrap_or(i64::MAX)
                && s.ready_replicas.unwrap_or_default() >= replicas
                && s.updated_replicas.unwrap_or_default() == replicas
                && s.available_replicas.unwrap_or_default() >= replicas
        })
}

pub async fn observe(
    client: Client,
    namespace: &str,
    cluster_id: &str,
    task: &ObservationTask,
) -> PoolObservation {
    let mut report = PoolObservation {
        scaling: None,
        applied_replicas: -1,
        idle: false,
        pool_id: task.pool_id.clone(),
        generation: task.generation,
        nonce: task.nonce.clone(),
        deployment_uid: None,
        ready: false,
        code: "kubernetes_unavailable".into(),
    };
    match read(
        client,
        namespace,
        cluster_id,
        task,
        &mut report,
    )
    .await
    {
        Ok(uid) => {
            report.ready = true;
            report.code = "ok".into();
            report.deployment_uid = Some(uid);
        }
        Err(code) => report.code = code.into(),
    }
    report
}

async fn read(
    client: Client,
    namespace: &str,
    cluster_id: &str,
    task: &ObservationTask,
    report: &mut PoolObservation,
) -> Result<String, &'static str> {
    if validate_name(namespace, true).is_err()
        || validate_name(&task.deployment, true).is_err()
        || validate_name(&task.serving_service, true).is_err()
        || !xscope_domain::traffic::managed(&task.pool_id)
    {
        return Err("identity_mismatch");
    }
    let mut expected = ModelDeployment::new(
        &task.deployment,
        serde_json::from_value(task.expected_spec.clone()).map_err(|_| "spec_drift")?,
    );
    expected.metadata.namespace = Some(namespace.into());
    validate(&mut expected).map_err(|_| "spec_drift")?;
    let models: Api<ModelDeployment> = Api::namespaced(client.clone(), namespace);
    let model = models
        .get(&task.deployment)
        .await
        .map_err(|_| "kubernetes_unavailable")?;
    let uid = model.metadata.uid.clone().ok_or("identity_mismatch")?;
    if model.metadata.deletion_timestamp.is_some()
        || model
            .labels()
            .get("platform.xscope.io/desired-cluster")
            .map(String::as_str)
            != Some(cluster_id)
        || task.expected_uid.as_ref().is_some_and(|id| id != &uid)
        || model
            .annotations()
            .get("platform.xscope.io/desired-version")
            .and_then(|v| v.parse::<i64>().ok())
            .is_none_or(|v| v < task.desired_version)
    {
        return Err("identity_mismatch");
    }
    if serde_json::to_value(&model.spec).map_err(|_| "spec_drift")?
        != serde_json::to_value(&expected.spec).map_err(|_| "spec_drift")?
    {
        return Err("spec_drift");
    }
    if model
        .spec
        .serving
        .as_ref()
        .is_none_or(|s| s.endpoint_picker_service != task.serving_service)
    {
        return Err("identity_mismatch");
    }
    // A cold/unready runtime still has an observed Kubernetes identity. Keep
    // that evidence so it can be drained/deleted without ever admitting traffic.
    report.deployment_uid = Some(uid.clone());
    let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);
    let runtime = deployments
        .get(&task.deployment)
        .await
        .map_err(|_| "kubernetes_unavailable")?;
    ensure_owner(&runtime, &model).map_err(|_| "identity_mismatch")?;
    report.applied_replicas = runtime.status.as_ref().and_then(|s|s.replicas).unwrap_or(-1);
    report.scaling = crate::recommendation::observe(client.clone(), &model, report.applied_replicas).await.unwrap_or(None);
    let ready = if crate::recommendation::enabled(&model) {
        let minimum=model.spec.autoscaling.as_ref().map_or(1,|s|s.min_replicas);
        runtime.metadata.deletion_timestamp.is_none() && runtime.spec.as_ref().and_then(|s|s.replicas)==Some(model.spec.replicas)
            && runtime.status.as_ref().is_some_and(|s| s.observed_generation.unwrap_or(0)>=runtime.metadata.generation.unwrap_or(i64::MAX)
                && s.ready_replicas.unwrap_or(0)>=minimum && s.available_replicas.unwrap_or(0)>=minimum && s.updated_replicas.unwrap_or(0)>=minimum)
    } else { deployment_ready(&runtime,model.spec.replicas) };
    if !ready {
        return Err("not_ready");
    }
    let services: Api<Service> = Api::namespaced(client.clone(), namespace);
    let entry = services
        .get(&task.serving_service)
        .await
        .map_err(|_| "kubernetes_unavailable")?;
    resources::validate_picker(Some(&entry), &model).map_err(|_| "identity_mismatch")?;
    // Installation-owned assertion: use the managed Envoy profile and prohibit
    // direct/unmanaged ingress. Observations do not modify external resources.
    if entry
        .annotations()
        .get("platform.xscope.io/managed-only")
        .map(String::as_str)
        != Some("true")
    {
        return Err("identity_mismatch");
    }
    let serving = deployments
        .get(&task.serving_service)
        .await
        .map_err(|_| "kubernetes_unavailable")?;
    if !deployment_ready(
        &serving,
        serving.spec.as_ref().and_then(|s| s.replicas).unwrap_or(0),
    ) {
        return Err("not_ready");
    }
    let labels = serving
        .spec
        .as_ref()
        .and_then(|s| s.template.metadata.as_ref())
        .and_then(|m| m.labels.as_ref())
        .ok_or("identity_mismatch")?;
    if entry
        .spec
        .as_ref()
        .and_then(|s| s.selector.as_ref())
        .is_none_or(|selector| {
            selector.is_empty() || !selector.iter().all(|(k, v)| labels.get(k) == Some(v))
        })
    {
        return Err("identity_mismatch");
    }
    let pools: Api<InferencePool> = Api::namespaced(client, namespace);
    let actual = pools
        .get(&task.deployment)
        .await
        .map_err(|_| "kubernetes_unavailable")?;
    ensure_owner(&actual, &model).map_err(|_| "identity_mismatch")?;
    let wanted = resources::desired_optional(&model, &runtime)
        .map_err(|_| "spec_drift")?
        .pool
        .ok_or("spec_drift")?;
    if serde_json::to_value(&actual.spec).map_err(|_| "spec_drift")?
        != serde_json::to_value(&wanted.spec).map_err(|_| "spec_drift")?
    {
        return Err("spec_drift");
    }
    let current = models
        .get(&task.deployment)
        .await
        .map_err(|_| "kubernetes_unavailable")?;
    if current.metadata.resource_version != model.metadata.resource_version {
        return Err("spec_drift");
    }
    Ok(uid)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn readiness_requires_current_generation_and_updated_available_replicas() {
        let mut d: Deployment=serde_json::from_value(serde_json::json!({"metadata":{"generation":2},"spec":{"replicas":3,"selector":{"matchLabels":{"app":"synthetic"}},"template":{"spec":{"containers":[]}}},
            "status":{"observedGeneration":2,"readyReplicas":3,"updatedReplicas":3,"availableReplicas":3}})).unwrap();
        assert!(deployment_ready(&d, 3));
        d.status.as_mut().unwrap().updated_replicas = Some(2);
        assert!(!deployment_ready(&d, 3));
        d.status.as_mut().unwrap().updated_replicas = Some(3);
        d.status.as_mut().unwrap().observed_generation = Some(1);
        assert!(!deployment_ready(&d, 3));
        assert!(!deployment_ready(&d, 0));
    }
}
