use crate::{
    Error,
    api::{ModelDeployment, ModelDeploymentStatus},
};
use k8s_openapi::api::{apps::v1::Deployment, core::v1::Service};
use kube::{
    Api, Client, Resource, ResourceExt,
    api::{Patch, PatchParams, PostParams},
    runtime::controller::Action,
};
use serde_json::json;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

pub struct Context {
    pub client: Client,
    pub cluster_id: String,
    pub region: String,
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
    let metadata = json!({"name":name,"namespace":namespace,"ownerReferences":[owner]});
    let deployment = serde_json::from_value(json!({
        "apiVersion":"apps/v1","kind":"Deployment","metadata":metadata,
        "spec":{"replicas":model.spec.replicas,"selector":{"matchLabels":labels},
            "template":{"metadata":{"labels":labels},"spec":{
                "nodeSelector":model.spec.node_selector,"tolerations":model.spec.tolerations,
                "containers":[{"name":"runtime","image":model.spec.runtime.image,"args":model.spec.runtime.arguments,
                    "ports":[{"name":"http","containerPort":port}],"resources":model.spec.resources,
                    "env":[{"name":"XSCOPE_MODEL_ID","value":model.spec.model.id},
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
    let models: Api<ModelDeployment> = Api::namespaced(context.client.clone(), &namespace);
    let existing = deployments.get_opt(&name).await?;
    let existing_service = services.get_opt(&name).await?;
    if let Some(service) = &existing_service {
        ensure_owner(service, model)?;
    }
    let (mut deployment, mut service) = desired(
        model,
        &context.cluster_id,
        &context.region,
        existing.as_ref(),
    )?;
    // Ownership is checked before force-claiming fields written by the old controller.
    let params = PatchParams::apply("xscope-operator").force();
    // Create when absent; on updates use UID/resourceVersion preconditions so
    // a delete/recreate race cannot cause takeover of an unrelated object.
    let deployment = if let Some(previous) = existing {
        deployment.metadata.uid = previous.metadata.uid;
        deployment.metadata.resource_version = previous.metadata.resource_version;
        deployments
            .patch(&name, &params, &Patch::Apply(&deployment))
            .await?
    } else {
        deployments
            .create(&PostParams::default(), &deployment)
            .await?
    };
    if let Some(previous) = existing_service {
        service.metadata.uid = previous.metadata.uid;
        service.metadata.resource_version = previous.metadata.resource_version;
        services
            .patch(&name, &params, &Patch::Apply(&service))
            .await?;
    } else {
        services.create(&PostParams::default(), &service).await?;
    }
    let status = ModelDeploymentStatus {
        observed_generation: model.metadata.generation.unwrap_or_default(),
        ready_replicas: deployment
            .status
            .as_ref()
            .and_then(|s| s.ready_replicas)
            .unwrap_or_default(),
        endpoint: format!(
            "http://{name}.{namespace}.svc:{}",
            service.spec.as_ref().unwrap().ports.as_ref().unwrap()[0].port
        ),
        cluster_id: context.cluster_id.clone(),
        region: context.region.clone(),
        conditions: model
            .status
            .as_ref()
            .map(|s| s.conditions.clone())
            .unwrap_or_default(),
    };
    if model.status.as_ref() != Some(&status) {
        models
            .patch_status(
                &name,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata":{"resourceVersion":model.metadata.resource_version}, "status":status
                })),
            )
            .await?;
    }
    Ok(Action::requeue(Duration::from_secs(300)))
}
pub fn error_policy(_model: Arc<ModelDeployment>, error: &Error, _context: Arc<Context>) -> Action {
    tracing::warn!(%error, "reconciliation failed; retrying");
    Action::requeue(Duration::from_secs(10))
}

#[cfg(test)]
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
