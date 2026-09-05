use crate::{
    Error,
    api::{ModelDeployment, ModelDeploymentStatus},
    autoscaling::{self, ScaledObject},
    pool::InferencePool,
    resources::{self, Hpa, Pdb},
};
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Service};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{ListParams, Patch, PatchParams},
    runtime::controller::Action,
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

pub struct Context {
    pub client: Client,
    pub cluster_id: String,
    pub region: String,
    pub prometheus_address: String,
}

/// Never adopt an unrelated object, even if it has the desired name or labels.
pub fn ensure_owner<K: Resource<DynamicType = ()>>(
    existing: &K,
    model: &ModelDeployment,
) -> Result<(), Error> {
    if !existing
        .meta()
        .owner_references
        .as_ref()
        .is_some_and(|owners| {
            owners.iter().any(|o| {
                o.controller == Some(true)
                    && Some(&o.uid) == model.metadata.uid.as_ref()
                    && o.kind == "ModelDeployment"
                    && o.api_version == "platform.xscope.io/v1alpha1"
            })
        })
    {
        return Err(Error::OwnershipConflict);
    }
    Ok(())
}

pub fn desired(
    model: &ModelDeployment,
    cluster_id: &str,
    region: &str,
    existing: Option<&Deployment>,
) -> Result<(Deployment, Service), Error> {
    let mut checked = model.clone();
    crate::api::validate(&mut checked)?;
    let model = &checked;
    let name = model.name_any();
    let namespace = model
        .namespace()
        .ok_or_else(|| Error::Invalid("namespace missing".into()))?;
    let owner = model
        .controller_owner_ref(&())
        .ok_or_else(|| Error::Invalid("resource UID missing".into()))?;
    let deployment_uid = owner.uid.clone();
    let mut labels = BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), name.clone()),
        ("app.kubernetes.io/component".into(), "model-runtime".into()),
        (
            "app.kubernetes.io/managed-by".into(),
            "xscope-operator".into(),
        ),
    ]);
    if !cluster_id.is_empty() {
        labels.insert("platform.xscope.io/cluster-id".into(), cluster_id.into());
    }
    if !region.is_empty() {
        labels.insert("platform.xscope.io/region".into(), region.into());
    }
    // Preserve immutable selectors across the Go -> Rust migration and config changes.
    if let Some(deployment) = existing {
        ensure_owner(deployment, model)?;
        if let Some(selector) = deployment
            .spec
            .as_ref()
            .and_then(|s| s.selector.match_labels.as_ref())
        {
            labels = selector.clone();
        }
    }
    let port = model.spec.runtime.port;
    let selector = labels.clone();
    labels.insert("platform.xscope.io/deployment-uid".into(), deployment_uid);
    labels.insert("app.kubernetes.io/part-of".into(), "xscope".into());
    let metadata = json!({"name":name,"namespace":namespace,"ownerReferences":[owner]});
    let deployment = serde_json::from_value(json!({
        "apiVersion":"apps/v1","kind":"Deployment","metadata":metadata,
        "spec":{"replicas":model.spec.replicas,"selector":{"matchLabels":selector},
            "template":{"metadata":{"labels":labels},"spec":{
                "automountServiceAccountToken":false,
                "nodeSelector":model.spec.node_selector,"tolerations":model.spec.tolerations,
                "containers":[{"name":"runtime","image":model.spec.runtime.image,"args":model.spec.runtime.arguments,
                    "ports":[{"name":"http","containerPort":port}],"resources":model.spec.resources,
                    "env":[{"name":"XSCOPE_RUNTIME_PORT","value":port.to_string()},
                        {"name":"XSCOPE_MODEL_ID","value":model.spec.model.id},
                        {"name":"XSCOPE_MODEL_REVISION","value":model.spec.model.revision},
                        {"name":"XSCOPE_MODEL_URI","value":model.spec.model.uri},
                        {"name":"XSCOPE_MODEL_CHECKSUM","value":model.spec.model.checksum}],
                    "readinessProbe":{"httpGet":{"path":"/readyz","port":"http"}}}]
            }}}
    }))?;
    let service = serde_json::from_value(json!({
        "apiVersion":"v1","kind":"Service","metadata":metadata,
        "spec":{"selector":labels,"ports":[{"name":"http","port":port,"targetPort":"http"}]}
    }))?;
    Ok((deployment, service))
}

#[tracing::instrument(skip_all, fields(otel.name = "modeldeployment.reconcile"))]
pub async fn reconcile(
    model: Arc<ModelDeployment>,
    context: Arc<Context>,
) -> Result<Action, Error> {
    let result = reconcile_inner(&model, &context).await;
    if let Err(error) = &result {
        let mut status = model.status.clone().unwrap_or_default();
        condition(
            &mut status,
            &model,
            "ResourcesReady",
            "False",
            "ReconcileFailed",
            &error.to_string(),
        );
        condition(
            &mut status,
            &model,
            "Ready",
            "False",
            "ReconcileFailed",
            "Desired resources could not be reconciled",
        );
        if let Err(status_error) = write_status(&model, &context, status).await {
            tracing::warn!(%status_error, "could not report reconciliation failure");
        }
    }
    xscope_telemetry::background_event(
        "reconcile",
        if result.is_ok() { "success" } else { "error" },
    );
    result
}
async fn reconcile_inner(model: &ModelDeployment, context: &Context) -> Result<Action, Error> {
    if model.metadata.deletion_timestamp.is_some() {
        return Ok(Action::await_change());
    }
    let namespace = model
        .namespace()
        .ok_or_else(|| Error::Invalid("namespace missing".into()))?;
    let name = model.name_any();
    let deployments: Api<Deployment> = Api::namespaced(context.client.clone(), &namespace);
    let services: Api<Service> = Api::namespaced(context.client.clone(), &namespace);
    let hpas: Api<Hpa> = Api::namespaced(context.client.clone(), &namespace);
    let pdbs: Api<Pdb> = Api::namespaced(context.client.clone(), &namespace);
    let pools: Api<InferencePool> = Api::namespaced(context.client.clone(), &namespace);
    let scalers: Api<ScaledObject> = Api::namespaced(context.client.clone(), &namespace);
    let existing = deployments.get_opt(&name).await?;
    let existing_service = services.get_opt(&name).await?;
    if let Some(service) = &existing_service {
        ensure_owner(service, model)?;
    }
    let (deployment, service) = desired(
        model,
        &context.cluster_id,
        &context.region,
        existing.as_ref(),
    )?;
    let optional = resources::desired_optional(model, &deployment)?;
    let all_hpas = hpas.list(&ListParams::default()).await?.items;
    let all_scalers = match scalers.list(&ListParams::default()).await {
        Ok(list) => list.items,
        Err(kube::Error::Api(error)) if error.code == 404 && model.spec.autoscaling.is_none() => Vec::new(),
        Err(kube::Error::Api(error)) if error.code == 404 => return Err(Error::Invalid("KEDA CRD is not installed; install the approved KEDA release before enabling autoscaling".into())),
        Err(error) => return Err(error.into()),
    };
    autoscaling::preflight(model, &all_scalers, &all_hpas)?;
    let old_scaler = all_scalers.iter().find(|s| s.name_any() == name).cloned();
    let desired_scaler =
        autoscaling::desired(model, &deployment.metadata, &context.prometheus_address)?;
    let old_pdb = pdbs.get_opt(&name).await?;
    let old_pool = pools.get_opt(&name).await?;
    resources::preflight(old_pdb.as_ref(), optional.pdb.is_some(), model)?;
    resources::preflight(old_pool.as_ref(), optional.pool.is_some(), model)?;
    if let Some(serving) = &model.spec.serving {
        let picker = services.get_opt(&serving.endpoint_picker_service).await?;
        resources::validate_picker(picker.as_ref(), model)?;
    }
    let selector_labels = deployment
        .spec
        .as_ref()
        .ok_or_else(|| Error::Invalid("desired Deployment spec is missing".into()))?
        .selector
        .match_labels
        .as_ref()
        .ok_or_else(|| Error::Invalid("desired Deployment selector labels are missing".into()))?;
    let selector = selector_labels
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let service_port = service
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_ref())
        .and_then(|ports| ports.first())
        .map(|port| port.port)
        .ok_or_else(|| Error::Invalid("desired Service port is missing".into()))?;
    // Change scaling writers only after observing deletion, never in the same
    // reconciliation that requested it. KEDA's HPA is read-only to XScope.
    match autoscaling::transition(model, old_scaler.as_ref(), &all_hpas) {
        autoscaling::Transition::RemoveLegacy => {
            let legacy = all_hpas
                .iter()
                .find(|h| h.name_any() == name && ensure_owner(*h, model).is_ok())
                .cloned();
            resources::sync(&hpas, model, legacy, None).await?;
            return scaling_transition(
                model,
                context,
                "Removing legacy Operator HPA before KEDA activation",
            )
            .await;
        }
        autoscaling::Transition::RemoveScaledObject => {
            resources::sync(&scalers, model, old_scaler, None).await?;
            return scaling_transition(
                model,
                context,
                "Removing KEDA ScaledObject before manual scaling resumes",
            )
            .await;
        }
        autoscaling::Transition::Wait => {
            return scaling_transition(
                model,
                context,
                "Waiting for ScaledObject deletion and KEDA HPA garbage collection",
            )
            .await;
        }
        autoscaling::Transition::Apply => {}
    }
    let hpa = all_hpas
        .iter()
        .find(|h| autoscaling::hpa_targets_model(h, model))
        .cloned();
    let deployment = resources::sync(&deployments, model, existing, Some(deployment))
        .await?
        .ok_or_else(|| Error::Invalid("Deployment sync returned no resource".into()))?;
    resources::sync(&services, model, existing_service, Some(service)).await?;
    resources::sync(&pdbs, model, old_pdb, optional.pdb).await?;
    resources::sync(&pools, model, old_pool, optional.pool).await?;
    let scaled = if desired_scaler.is_some() {
        resources::sync(&scalers, model, old_scaler, desired_scaler).await?
    } else {
        None
    };
    let mut status = ModelDeploymentStatus {
        observed_generation: model.metadata.generation.unwrap_or_default(),
        ready_replicas: deployment
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or_default(),
        replicas: deployment
            .status
            .as_ref()
            .and_then(|s| s.replicas)
            .unwrap_or_default(),
        selector,
        inference_pool: if model.spec.serving.is_some() {
            name.clone()
        } else {
            String::new()
        },
        endpoint_picker_service: model
            .spec
            .serving
            .as_ref()
            .map(|s| s.endpoint_picker_service.clone())
            .unwrap_or_default(),
        endpoint: format!("http://{name}.{namespace}.svc:{service_port}"),
        cluster_id: context.cluster_id.clone(),
        region: context.region.clone(),
        conditions: model
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default(),
    };
    condition(
        &mut status,
        model,
        "ResourcesReady",
        "True",
        "Reconciled",
        "All desired owned resources reconciled",
    );
    let ready = model.spec.replicas > 0
        && status.ready_replicas == model.spec.replicas
        && deployment
            .status
            .as_ref()
            .and_then(|s| s.observed_generation)
            .unwrap_or_default()
            >= deployment.metadata.generation.unwrap_or_default();
    condition(
        &mut status,
        model,
        "Ready",
        if ready { "True" } else { "False" },
        if ready {
            "RuntimeReady"
        } else {
            "RuntimeNotReady"
        },
        "Runtime readiness only; external EPP health and gateway registration are separate",
    );
    let active = hpa
        .as_ref()
        .and_then(|h| h.status.as_ref())
        .and_then(|s| s.conditions.as_ref())
        .and_then(|conditions| conditions.iter().find(|c| c.type_ == "ScalingActive"));
    if model.spec.autoscaling.is_none() {
        condition(
            &mut status,
            model,
            "AutoscalingActive",
            "False",
            "Disabled",
            "Replicas are manually managed",
        );
    } else if let Some(not_ready) = scaled
        .as_ref()
        .and_then(|s| s.status.as_ref())
        .and_then(|s| {
            s.conditions
                .iter()
                .find(|c| c.type_ == "Ready" && c.status != "True")
        })
    {
        condition(
            &mut status,
            model,
            "AutoscalingActive",
            &not_ready.status,
            "KedaNotReady",
            &not_ready.message,
        );
    } else if let Some(active) = active {
        condition(
            &mut status,
            model,
            "AutoscalingActive",
            &active.status,
            active.reason.as_deref().unwrap_or("PendingMetrics"),
            active
                .message
                .as_deref()
                .unwrap_or("Waiting for HPA metrics"),
        );
    } else {
        condition(
            &mut status,
            model,
            "AutoscalingActive",
            "Unknown",
            "PendingMetrics",
            "KEDA ScaledObject configured; waiting for KEDA-managed HPA and metrics",
        );
    }
    write_status(model, context, status).await?;
    Ok(Action::requeue(Duration::from_secs(30)))
}

async fn scaling_transition(
    model: &ModelDeployment,
    context: &Context,
    message: &str,
) -> Result<Action, Error> {
    let mut status = model.status.clone().unwrap_or_default();
    condition(
        &mut status,
        model,
        "AutoscalingActive",
        "Unknown",
        "ScalerTransition",
        message,
    );
    write_status(model, context, status).await?;
    Ok(Action::requeue(Duration::from_secs(2)))
}

fn condition(
    status: &mut ModelDeploymentStatus,
    model: &ModelDeployment,
    kind: &str,
    value: &str,
    reason: &str,
    message: &str,
) {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
    let previous = status.conditions.iter().find(|c| c.type_ == kind);
    let timestamp = previous
        .filter(|c| c.status == value)
        .map(|c| c.last_transition_time.clone())
        .unwrap_or_else(|| Time(k8s_openapi::jiff::Timestamp::now()));
    let next = Condition {
        type_: kind.into(),
        status: value.into(),
        reason: reason.into(),
        message: message.into(),
        observed_generation: model.metadata.generation,
        last_transition_time: timestamp,
    };
    status.conditions.retain(|c| c.type_ != kind);
    status.conditions.push(next);
    status.conditions.sort_by(|a, b| a.type_.cmp(&b.type_));
}

async fn write_status(
    model: &ModelDeployment,
    context: &Context,
    status: ModelDeploymentStatus,
) -> Result<(), Error> {
    if model.status.as_ref() != Some(&status) {
        let namespace = model
            .namespace()
            .ok_or_else(|| Error::Invalid("namespace missing".into()))?;
        Api::<ModelDeployment>::namespaced(context.client.clone(), &namespace)
            .patch_status(
                &model.name_any(),
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata":{"resourceVersion":model.metadata.resource_version}, "status":status
                })),
            )
            .await?;
    }
    Ok(())
}
pub fn error_policy(_model: Arc<ModelDeployment>, error: &Error, _context: Arc<Context>) -> Action {
    tracing::warn!(%error, "reconciliation failed; retrying");
    Action::requeue(Duration::from_secs(10))
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn workload_service_resources_and_owner_are_preserved() {
        let model = crate::api::example();
        let (deployment, service) = desired(&model, "local", "local", None).unwrap();
        ensure_owner(&deployment, &model).unwrap();
        assert_eq!(deployment.spec.as_ref().unwrap().replicas, Some(2));
        assert_eq!(service.spec.unwrap().ports.unwrap()[0].port, 8000);
        assert_eq!(
            deployment
                .spec
                .as_ref()
                .unwrap()
                .template
                .spec
                .as_ref()
                .unwrap()
                .containers[0]
                .resources,
            Some(model.spec.resources.clone())
        );
        let (next, _) = desired(&model, "renamed", "renamed", Some(&deployment)).unwrap();
        assert_eq!(
            next.spec.unwrap().selector,
            deployment.spec.unwrap().selector
        );
    }
    #[test]
    fn rejects_foreign_same_name_resources() {
        let model = crate::api::example();
        let (mut deployment, _) = desired(&model, "local", "local", None).unwrap();
        deployment.metadata.owner_references = None;
        assert!(matches!(
            desired(&model, "local", "local", Some(&deployment)),
            Err(Error::OwnershipConflict)
        ));
    }
}
