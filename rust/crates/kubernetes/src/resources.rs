use crate::{Error, api::ModelDeployment, pool::InferencePool};
use k8s_openapi::api::{
    apps::v1::Deployment, autoscaling::v2::HorizontalPodAutoscaler, core::v1::Service,
    policy::v1::PodDisruptionBudget,
};
use kube::{
    Api, Resource, ResourceExt,
    api::{DeleteParams, Patch, PatchParams, PostParams, Preconditions},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use std::fmt::Debug;

pub type Hpa = HorizontalPodAutoscaler;
pub type Pdb = PodDisruptionBudget;

pub struct OptionalResources {
    pub pdb: Option<Pdb>,
    pub pool: Option<InferencePool>,
}

pub fn desired_optional(
    model: &ModelDeployment,
    deployment: &Deployment,
) -> Result<OptionalResources, Error> {
    let metadata = &deployment.metadata;
    let labels = deployment
        .spec
        .as_ref()
        .ok_or_else(|| Error::Invalid("desired Deployment spec is missing".into()))?
        .template
        .metadata
        .as_ref()
        .ok_or_else(|| Error::Invalid("desired Pod template metadata is missing".into()))?
        .labels
        .as_ref()
        .ok_or_else(|| Error::Invalid("desired Pod template labels are missing".into()))?;
    let pdb = model
        .spec
        .disruption_budget
        .as_ref()
        .map(|budget| {
            serde_json::from_value(json!({
                "apiVersion":"policy/v1","kind":"PodDisruptionBudget","metadata":metadata,
                "spec":{"maxUnavailable":budget.max_unavailable,"selector":{"matchLabels":labels}}
            }))
        })
        .transpose()?;
    let pool = model.spec.serving.as_ref().map(|serving| serde_json::from_value(json!({
        "apiVersion":"inference.networking.k8s.io/v1","kind":"InferencePool","metadata":metadata,
        "spec":{"selector":{"matchLabels":labels},"targetPorts":[{"number":model.spec.runtime.port}],
            "endpointPickerRef":{"name":serving.endpoint_picker_service,"port":{"number":serving.endpoint_picker_port},"failureMode":"FailClose"}}
    }))).transpose()?;
    Ok(OptionalResources { pdb, pool })
}

/// External EPP is a one-pool contract declared by the installation owner.
/// Operator never edits this Service, its Deployment, certificates or RBAC.
pub fn validate_picker(service: Option<&Service>, model: &ModelDeployment) -> Result<(), Error> {
    let Some(serving) = &model.spec.serving else {
        return Ok(());
    };
    let Some(service) = service else {
        return Err(Error::Invalid(
            "EPP Service is missing; install a pool-specific llm-d serving entry first".into(),
        ));
    };
    if service
        .annotations()
        .get("platform.xscope.io/inference-pool")
        != Some(&model.name_any())
        || service.spec.as_ref().is_none_or(|spec| {
            spec.ports
                .as_ref()
                .is_none_or(|ports| !ports.iter().any(|p| p.port == serving.endpoint_picker_port))
        })
    {
        return Err(Error::Invalid("EPP Service must declare this pool in platform.xscope.io/inference-pool and expose the configured port".into()));
    }
    Ok(())
}

/// Check *all* desired identities before writing any child resource.
pub fn preflight<K: Resource<DynamicType = ()>>(
    previous: Option<&K>,
    wanted: bool,
    model: &ModelDeployment,
) -> Result<(), Error> {
    if wanted && let Some(previous) = previous {
        crate::controller::ensure_owner(previous, model)?;
    }
    Ok(())
}

/// Never force-adopt a foreign object or delete a concurrently replaced child.
pub async fn sync<K>(
    api: &Api<K>,
    model: &ModelDeployment,
    previous: Option<K>,
    desired: Option<K>,
) -> Result<Option<K>, Error>
where
    K: Resource<DynamicType = ()> + Clone + Debug + Serialize + DeserializeOwned,
{
    let name = model.name_any();
    if let Some(mut desired) = desired {
        if let Some(previous) = previous {
            crate::controller::ensure_owner(&previous, model)?;
            desired.meta_mut().uid.clone_from(&previous.meta().uid);
            desired
                .meta_mut()
                .resource_version
                .clone_from(&previous.meta().resource_version);
            Ok(Some(
                api.patch(
                    &name,
                    &PatchParams::apply("xscope-operator").force(),
                    &Patch::Apply(&desired),
                )
                .await?,
            ))
        } else {
            Ok(Some(
                api.create(
                    &PostParams {
                        field_manager: Some("xscope-operator".into()),
                        ..PostParams::default()
                    },
                    &desired,
                )
                .await?,
            ))
        }
    } else {
        if let Some(previous) = previous
            && crate::controller::ensure_owner(&previous, model).is_ok()
        {
            api.delete(
                &name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: previous.meta().uid.clone(),
                        resource_version: previous.meta().resource_version.clone(),
                    }),
                    ..DeleteParams::default()
                },
            )
            .await?;
        }
        Ok(None)
    }
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::api::{AutoscalingSpec, DisruptionBudgetSpec, ServingSpec};
    use k8s_openapi::api::core::v1::ResourceRequirements;

    fn model() -> ModelDeployment {
        let mut model = crate::api::example();
        model.spec.resources = serde_json::from_value::<ResourceRequirements>(
            json!({"requests":{"cpu":"20m"},"limits":{"cpu":"200m"}}),
        )
        .unwrap();
        model.spec.autoscaling = Some(AutoscalingSpec {
            min_replicas: 1,
            max_replicas: 3,
            target_pending_requests: 0,
            target_running_requests: 0,
            target_cpu_utilization_percentage: None,
        });
        model.spec.disruption_budget = Some(DisruptionBudgetSpec { max_unavailable: 1 });
        model.spec.serving = Some(ServingSpec {
            endpoint_picker_service: "demo-epp".into(),
            endpoint_picker_port: 9002,
        });
        model
    }

    #[test]
    fn owned_resources_share_isolated_labels_and_keda_scales_the_cr() {
        let model = model();
        let (deployment, service) =
            crate::controller::desired(&model, "local", "local", None).unwrap();
        let resources = desired_optional(&model, &deployment).unwrap();
        let scaled =
            crate::autoscaling::desired(&model, &deployment.metadata, "http://prometheus:9090")
                .unwrap()
                .unwrap();
        crate::controller::ensure_owner(&scaled, &model).unwrap();
        let spec = scaled.spec;
        assert_eq!(spec.scale_target_ref.kind, "ModelDeployment");
        assert_eq!(spec.scale_target_ref.name, "demo");
        assert_eq!(spec.max_replica_count, 3);
        assert_eq!(spec.triggers[0].metadata["value"], "70");
        let pool = resources.pool.unwrap();
        crate::controller::ensure_owner(&pool, &model).unwrap();
        assert_eq!(
            pool.spec.selector.match_labels,
            service.spec.unwrap().selector.unwrap()
        );
        assert_eq!(
            pool.spec.selector.match_labels["platform.xscope.io/deployment-uid"],
            "model-uid"
        );
        assert_eq!(pool.spec.endpoint_picker_ref.failure_mode, "FailClose");
        let pdb = resources.pdb.unwrap();
        crate::controller::ensure_owner(&pdb, &model).unwrap();
        assert_eq!(
            pdb.spec.unwrap().selector.unwrap().match_labels.unwrap(),
            pool.spec.selector.match_labels
        );
        let original = crate::api::example();
        let resources = desired_optional(&original, &deployment).unwrap();
        assert!(resources.pdb.is_none() && resources.pool.is_none());
        assert!(
            crate::autoscaling::desired(&original, &deployment.metadata, "http://prometheus:9090")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pending_requests_use_keda_prometheus_and_optional_cpu() {
        let mut model = model();
        let scaling = model.spec.autoscaling.as_mut().unwrap();
        scaling.target_pending_requests = 5;
        let (deployment, _) = crate::controller::desired(&model, "local", "local", None).unwrap();
        let metrics =
            crate::autoscaling::desired(&model, &deployment.metadata, "http://prometheus:9090")
                .unwrap()
                .unwrap()
                .spec
                .triggers;
        assert_eq!(metrics.len(), 1);
        assert!(metrics[0].metadata["query"].contains("llm_d_epp_flow_control_queue_size"));
        model
            .spec
            .autoscaling
            .as_mut()
            .unwrap()
            .target_cpu_utilization_percentage = Some(60);
        assert_eq!(
            crate::autoscaling::desired(&model, &deployment.metadata, "http://prometheus:9090")
                .unwrap()
                .unwrap()
                .spec
                .triggers
                .len(),
            2
        );
    }

    #[test]
    fn reject_misbound_epp_and_invalid_scaling_before_creating_children() {
        let mut model = model();
        assert!(validate_picker(None, &model).is_err());
        let mut service: Service = serde_json::from_value(json!({"metadata":{"name":"demo-epp","annotations":{"platform.xscope.io/inference-pool":"wrong"}},"spec":{"ports":[{"port":9002}]}})).unwrap();
        assert!(validate_picker(Some(&service), &model).is_err());
        service
            .metadata
            .annotations
            .as_mut()
            .unwrap()
            .insert("platform.xscope.io/inference-pool".into(), "demo".into());
        validate_picker(Some(&service), &model).unwrap();
        model.spec.autoscaling.as_mut().unwrap().min_replicas = 0;
        assert!(crate::api::validate(&mut model).is_err());
        model.spec.autoscaling.as_mut().unwrap().min_replicas = 1;
        model.spec.replicas = 0;
        assert!(crate::api::validate(&mut model).is_err());
        model.spec.replicas = 2;
        model.spec.resources.requests = None;
        assert!(crate::api::validate(&mut model).is_err());
    }

    #[tokio::test]
    async fn removal_requires_exact_uid_and_version_and_ignores_foreign_objects() {
        use axum::body::{Body, to_bytes};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tower::service_fn;
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let client = kube::Client::new(
            service_fn(move |request: http::Request<kube::client::Body>| {
                observed.fetch_add(1, Ordering::SeqCst);
                async move {
                    assert_eq!(request.method(), "DELETE");
                    let bytes = to_bytes(Body::new(request.into_body()), 1 << 20)
                        .await
                        .unwrap();
                    let data: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(
                        data["preconditions"],
                        json!({"uid":"child-uid","resourceVersion":"7"})
                    );
                    Ok::<_, std::convert::Infallible>(http::Response::new(Body::from(
                        json!({"apiVersion":"v1","kind":"Status","status":"Success"}).to_string(),
                    )))
                }
            }),
            "xscope-system",
        );
        let model = model();
        let (_, mut service) = crate::controller::desired(&model, "local", "local", None).unwrap();
        service.metadata.uid = Some("child-uid".into());
        service.metadata.resource_version = Some("7".into());
        let api: Api<Service> = Api::namespaced(client, "xscope-system");
        sync(&api, &model, Some(service.clone()), None)
            .await
            .unwrap();
        service.metadata.owner_references = None;
        assert!(preflight(Some(&service), true, &model).is_err());
        assert!(
            sync(&api, &model, Some(service.clone()), Some(service.clone()))
                .await
                .is_err()
        );
        sync(&api, &model, Some(service), None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
