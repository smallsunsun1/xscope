//! KEDA is the only automatic scaling configuration owner. Its upstream CRD is
//! installed separately; these types do not generate a replacement CRD.
use crate::{Error, api::ModelDeployment, controller::ensure_owner, resources::Hpa};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{CustomResource, ResourceExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[kube(
    group = "keda.sh",
    version = "v1alpha1",
    kind = "ScaledObject",
    plural = "scaledobjects",
    namespaced,
    status = "ScaledObjectStatus",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct ScaledObjectSpec {
    pub scale_target_ref: ScaleTargetRef,
    #[serde(default)]
    pub min_replica_count: i32,
    #[serde(default)]
    pub max_replica_count: i32,
    #[serde(default)]
    pub polling_interval: i32,
    #[serde(default)]
    pub advanced: Value,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaleTargetRef {
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub kind: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trigger {
    #[serde(rename = "type")]
    pub type_: String,
    #[serde(default)]
    pub metric_type: String,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaledObjectStatus {
    #[serde(default)]
    pub conditions: Vec<KedaCondition>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct KedaCondition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub message: String,
}

pub fn targets_model(target: &ScaleTargetRef, model: &ModelDeployment) -> bool {
    target.api_version == "platform.xscope.io/v1alpha1"
        && target.kind == crate::recommendation::target_kind(model)
        && target.name == model.name_any()
}

pub fn hpa_targets_model(hpa: &Hpa, model: &ModelDeployment) -> bool {
    hpa.spec.as_ref().is_some_and(|spec| {
        spec.scale_target_ref.api_version.as_deref() == Some("platform.xscope.io/v1alpha1")
            && spec.scale_target_ref.kind == crate::recommendation::target_kind(model)
            && spec.scale_target_ref.name == model.name_any()
    })
}

pub fn keda_owns_hpa(hpa: &Hpa, scaled: &ScaledObject) -> bool {
    scaled.uid().is_some_and(|uid| {
        hpa.owner_references().iter().any(|owner| {
            owner.controller == Some(true)
                && owner.api_version == "keda.sh/v1alpha1"
                && owner.kind == "ScaledObject"
                && owner.uid == uid
                && owner.name == scaled.name_any()
        })
    })
}

/// Reject every competing scaler, not just an object with our expected name.
/// The caller waits for legacy deletion/ScaledObject GC before changing writers.
pub fn preflight(
    model: &ModelDeployment,
    scaled_objects: &[ScaledObject],
    hpas: &[Hpa],
) -> Result<(), Error> {
    let owned = scaled_objects
        .iter()
        .find(|s| s.name_any() == model.name_any());
    for scaled in scaled_objects {
        if scaled.name_any() == model.name_any()
            || targets_model(&scaled.spec.scale_target_ref, model)
        {
            ensure_owner(scaled, model)?;
            if scaled.name_any() != model.name_any() {
                return Err(Error::Conflict(
                    "another ScaledObject already targets this model".into(),
                ));
            }
        }
    }
    for hpa in hpas {
        let reserved_name = hpa.name_any() == format!("keda-hpa-{}", model.name_any());
        if hpa_targets_model(hpa, model) || reserved_name || hpa.name_any() == model.name_any() {
            if owned.is_some_and(|s| keda_owns_hpa(hpa, s)) {
                continue;
            }
            if hpa.name_any() == model.name_any() && ensure_owner(hpa, model).is_ok() {
                continue; // only this legacy identity may be removed by XScope
            }
            return Err(Error::Conflict(
                "foreign HPA conflicts with the model's KEDA scaler".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Transition {
    RemoveLegacy,
    RemoveScaledObject,
    Wait,
    Apply,
}

pub fn transition(
    model: &ModelDeployment,
    scaled: Option<&ScaledObject>,
    hpas: &[Hpa],
) -> Transition {
    if hpas
        .iter()
        .any(|h| h.name_any() == model.name_any() && ensure_owner(h, model).is_ok())
    {
        return Transition::RemoveLegacy;
    }
    if scaled.is_some_and(|s| s.metadata.deletion_timestamp.is_some()) {
        return Transition::Wait;
    }
    if model.spec.autoscaling.is_none() {
        if scaled.is_some() {
            return Transition::RemoveScaledObject;
        }
        if hpas.iter().any(|h| hpa_targets_model(h, model)) {
            return Transition::Wait;
        }
    }
    Transition::Apply
}

pub fn desired(
    model: &ModelDeployment,
    metadata: &ObjectMeta,
    prometheus_address: &str,
) -> Result<Option<ScaledObject>, Error> {
    let Some(scaling) = &model.spec.autoscaling else {
        return Ok(None);
    };
    let mut triggers = Vec::new();
    for (target, metric) in [
        (
            scaling.target_pending_requests,
            "llm_d_epp_flow_control_queue_size",
        ),
        (scaling.target_running_requests, "llm_d_epp_request_running"),
    ] {
        if target == 0 {
            continue;
        }
        let serving = model.spec.serving.as_ref().ok_or_else(|| {
            Error::Invalid("EPP autoscaling requires serving.endpointPickerService".into())
        })?;
        crate::api::validate_name(&serving.endpoint_picker_service, true)?;
        let namespace = model
            .namespace()
            .ok_or_else(|| Error::Invalid("namespace missing".into()))?;
        crate::api::validate_name(&namespace, true)?;
        if !(prometheus_address.starts_with("http://")
            || prometheus_address.starts_with("https://"))
        {
            return Err(Error::Invalid(
                "operator Prometheus address must be an HTTP(S) URL".into(),
            ));
        }
        // Label contract is installed by the administrator. No user-supplied
        // PromQL/URL and no `or vector(0)` that would hide missing telemetry.
        let selector = format!(
            "namespace=\"{namespace}\",service=\"{}\"",
            serving.endpoint_picker_service
        );
        let query = format!(
            "sum({metric}{{job=\"epp\",{selector}}}) and on() (min(up{{job=\"epp\",{selector}}}) == 1) and on() (count(count by (instance) ({metric}{{job=\"epp\",{selector}}})) == count(up{{job=\"epp\",{selector}}})) and on() (time() - min(timestamp({metric}{{job=\"epp\",{selector}}})) < 90) and on() (time() - min(timestamp(up{{job=\"epp\",{selector}}})) < 90)"
        );
        triggers.push(Trigger {
            type_: "prometheus".into(),
            metric_type: "AverageValue".into(),
            metadata: BTreeMap::from([
                ("serverAddress".into(), prometheus_address.into()),
                ("query".into(), query),
                ("threshold".into(), target.to_string()),
                ("ignoreNullValues".into(), "false".into()),
            ]),
        });
    }
    if triggers.is_empty() || scaling.target_cpu_utilization_percentage.is_some() {
        triggers.push(Trigger {
            type_: "cpu".into(),
            metric_type: "Utilization".into(),
            metadata: BTreeMap::from([(
                "value".into(),
                scaling
                    .target_cpu_utilization_percentage
                    .unwrap_or(70)
                    .to_string(),
            )]),
        });
    }
    Ok(Some(serde_json::from_value(json!({
        "apiVersion":"keda.sh/v1alpha1", "kind":"ScaledObject", "metadata":metadata,
        "spec":{
            "scaleTargetRef":{"apiVersion":"platform.xscope.io/v1alpha1","kind":crate::recommendation::target_kind(model),"name":model.name_any()},
            "minReplicaCount":scaling.min_replicas,"maxReplicaCount":scaling.max_replicas,"pollingInterval":30,
            "advanced":{"restoreToOriginalReplicaCount":false,"horizontalPodAutoscalerConfig":{
                "name":format!("keda-hpa-{}",model.name_any()),
                "behavior":{"scaleUp":{"stabilizationWindowSeconds":30,"policies":[{"type":"Pods","value":1,"periodSeconds":60}]},
                    "scaleDown": if scaling.managed { json!({"stabilizationWindowSeconds":60,"policies":[{"type":"Pods","value":1,"periodSeconds":60}]}) } else { json!({"selectPolicy":"Disabled"}) }}
            }},"triggers":triggers
        }
    }))?))
}

#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::api::{AutoscalingSpec, ServingSpec};

    #[test]
    fn unrelated_upstream_defaults_do_not_break_namespace_inventory() {
        // KEDA defaults optional fields; reading another application's scaler
        // must not require the fields XScope emits for its own scalers.
        let scaled: ScaledObject = serde_json::from_value(json!({
            "apiVersion":"keda.sh/v1alpha1", "kind":"ScaledObject",
            "metadata":{"name":"other"},
            "spec":{"scaleTargetRef":{"name":"other"},
                "triggers":[{"type":"cpu","metadata":{"value":"70"}}]}
        }))
        .unwrap();
        assert!(!targets_model(
            &scaled.spec.scale_target_ref,
            &crate::api::example()
        ));
    }

    fn fixture() -> (ModelDeployment, ScaledObject, Hpa) {
        let mut model = crate::api::example();
        model.spec.autoscaling = Some(AutoscalingSpec {
            managed: false,
            min_replicas: 1,
            max_replicas: 8,
            target_pending_requests: 4,
            target_running_requests: 10,
            target_cpu_utilization_percentage: None,
        });
        model.spec.serving = Some(ServingSpec {
            endpoint_picker_service: "demo-epp".into(),
            endpoint_picker_port: 9002,
        });
        let (deployment, _) = crate::controller::desired(&model, "local", "local", None).unwrap();
        let mut scaled = desired(&model, &deployment.metadata, "http://prometheus:9090")
            .unwrap()
            .unwrap();
        scaled.metadata.uid = Some("scaler-uid".into());
        let hpa = serde_json::from_value(json!({
            "metadata":{"name":"keda-hpa-demo","namespace":"xscope-system","ownerReferences":[
                {"apiVersion":"keda.sh/v1alpha1","kind":"ScaledObject","name":"demo","uid":"scaler-uid","controller":true}]},
            "spec":{"scaleTargetRef":{"apiVersion":"platform.xscope.io/v1alpha1","kind":"ModelDeployment","name":"demo"},"maxReplicas":8}
        })).unwrap();
        (model, scaled, hpa)
    }

    #[test]
    fn prometheus_contract_has_pool_isolation_freshness_and_no_missing_as_zero() {
        let (model, scaled, _) = fixture();
        assert!(targets_model(&scaled.spec.scale_target_ref, &model));
        assert_eq!(scaled.spec.triggers.len(), 2);
        for trigger in &scaled.spec.triggers {
            assert_eq!(trigger.metric_type, "AverageValue");
            assert_eq!(trigger.metadata["ignoreNullValues"], "false");
            let query = &trigger.metadata["query"];
            assert!(query.contains("namespace=\"xscope-system\""));
            assert!(query.contains("service=\"demo-epp\""));
            assert!(query.contains("min(up{"));
            assert!(query.contains("timestamp("));
            assert!(!query.contains("vector(0)"));
        }
        assert_eq!(
            scaled.spec.advanced["horizontalPodAutoscalerConfig"]["behavior"]["scaleDown"]["selectPolicy"],
            "Disabled"
        );
        assert_eq!(scaled.spec.min_replica_count, 1);
        assert!(
            serde_json::to_value(scaled).unwrap()["spec"]
                .get("fallback")
                .is_none()
        );
    }

    #[test]
    fn only_keda_may_own_its_hpa_and_competing_names_are_rejected() {
        let (model, scaled, hpa) = fixture();
        preflight(
            &model,
            std::slice::from_ref(&scaled),
            std::slice::from_ref(&hpa),
        )
        .unwrap();
        assert!(keda_owns_hpa(&hpa, &scaled));
        let mut foreign = hpa.clone();
        foreign.metadata.owner_references.as_mut().unwrap()[0].uid = "other-uid".into();
        assert!(preflight(&model, std::slice::from_ref(&scaled), &[foreign]).is_err());
        let mut foreign = hpa.clone();
        foreign.metadata.name = Some("unexpected-scaler".into());
        foreign.metadata.owner_references = None;
        assert!(preflight(&model, std::slice::from_ref(&scaled), &[foreign]).is_err());
        let mut competing = scaled.clone();
        competing.metadata.name = Some("second-scaler".into());
        assert!(preflight(&model, &[scaled, competing], &[hpa]).is_err());
    }

    #[test]
    fn new_uid_never_inherits_old_scaler_or_hpa() {
        let (mut model, scaled, hpa) = fixture();
        model.metadata.uid = Some("new-model-uid".into());
        assert!(preflight(&model, &[scaled], &[hpa]).is_err());
    }

    #[test]
    fn migration_removal_and_gc_are_separate_observed_steps() {
        let (mut model, mut scaled, hpa) = fixture();
        let mut legacy = hpa.clone();
        legacy.metadata.name = Some(model.name_any());
        legacy.metadata.owner_references = scaled.metadata.owner_references.clone();
        preflight(&model, &[], std::slice::from_ref(&legacy)).unwrap();
        assert_eq!(
            transition(&model, None, &[legacy]),
            Transition::RemoveLegacy
        );
        assert_eq!(transition(&model, None, &[]), Transition::Apply);
        model.spec.autoscaling = None;
        assert_eq!(
            transition(&model, Some(&scaled), std::slice::from_ref(&hpa)),
            Transition::RemoveScaledObject
        );
        scaled.metadata.deletion_timestamp =
            Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
                k8s_openapi::jiff::Timestamp::now(),
            ));
        assert_eq!(
            transition(&model, Some(&scaled), std::slice::from_ref(&hpa)),
            Transition::Wait
        );
        assert_eq!(transition(&model, None, &[hpa]), Transition::Wait);
        assert_eq!(transition(&model, None, &[]), Transition::Apply);
    }

    #[test]
    fn invalid_or_user_supplied_metric_configuration_fails_closed() {
        let (mut model, scaled, _) = fixture();
        model.spec.serving = None;
        assert!(crate::api::validate(&mut model).is_err());
        assert!(desired(&model, &scaled.metadata, "http://prometheus:9090").is_err());
        let (mut model, _, _) = fixture();
        model
            .spec
            .autoscaling
            .as_mut()
            .unwrap()
            .target_running_requests = -1;
        assert!(crate::api::validate(&mut model).is_err());
        let (mut model, _, _) = fixture();
        model.spec.autoscaling.as_mut().unwrap().min_replicas = 0;
        assert!(crate::api::validate(&mut model).is_err());
        let (model, _, _) = fixture();
        let mut value = serde_json::to_value(&model).unwrap();
        value["spec"]["autoscaling"]["query"] = json!("arbitrary query");
        assert!(serde_json::from_value::<ModelDeployment>(value).is_err());
    }
}
