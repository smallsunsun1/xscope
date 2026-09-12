use crate::Error;
use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use kube::{CustomResource, ResourceExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[kube(
    group = "platform.xscope.io",
    version = "v1alpha1",
    kind = "ModelDeployment",
    plural = "modeldeployments",
    namespaced,
    status = "ModelDeploymentStatus",
    schema = "disabled"
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelDeploymentSpec {
    pub model: ModelSpec,
    pub runtime: RuntimeSpec,
    pub replicas: i32,
    pub resources: ResourceRequirements,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autoscaling: Option<AutoscalingSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disruption_budget: Option<DisruptionBudgetSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serving: Option<ServingSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollout: Option<RolloutSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerations: Option<Vec<Toleration>>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSpec {
    pub id: String,
    pub revision: String,
    pub uri: String,
    pub checksum: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeSpec {
    pub image: String,
    pub protocol: String,
    #[serde(default = "default_port")]
    pub port: i32,
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Runtime-owned readiness contract; use /health for engines such as vLLM.
    #[serde(default)]
    pub health: RuntimeHealth,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeHealth {
    pub path: String,
    pub startup_timeout_seconds: i32,
}
impl Default for RuntimeHealth {
    fn default() -> Self {
        Self {
            path: "/readyz".into(),
            startup_timeout_seconds: 1800,
        }
    }
}
fn default_port() -> i32 {
    8000
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutoscalingSpec {
    /// KEDA writes a recommendation; only the control plane applies replicas.
    #[serde(default)]
    pub managed: bool,
    #[serde(default = "one")]
    pub min_replicas: i32,
    #[serde(default)]
    pub max_replicas: i32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub target_pending_requests: i32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub target_running_requests: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_cpu_utilization_percentage: Option<i32>,
}
fn one() -> i32 {
    1
}
fn is_zero(value: &i32) -> bool {
    *value == 0
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DisruptionBudgetSpec {
    pub max_unavailable: i32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServingSpec {
    /// Installation-owned, same-namespace Service. One EPP deployment per pool.
    pub endpoint_picker_service: String,
    #[serde(default = "epp_port")]
    pub endpoint_picker_port: i32,
}
fn epp_port() -> i32 {
    9002
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RolloutSpec {
    #[serde(default = "rolling")]
    pub strategy: String,
    #[serde(default)]
    pub canary_weight: i32,
}
fn rolling() -> String {
    "rolling".into()
}
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ModelDeploymentStatus {
    #[serde(default)]
    pub observed_generation: i64,
    #[serde(default)]
    pub ready_replicas: i32,
    #[serde(default)]
    pub replicas: i32,
    #[serde(default)]
    pub selector: String,
    #[serde(default)]
    pub inference_pool: String,
    #[serde(default)]
    pub endpoint_picker_service: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub cluster_id: String,
    #[serde(default)]
    pub region: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
}

pub fn validate_name(name: &str, namespace: bool) -> Result<(), Error> {
    let limit = if namespace { 63 } else { 253 };
    if name.is_empty()
        || name.len() > limit
        || !name.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part.as_bytes()[0].is_ascii_alphanumeric()
                && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        || (namespace && name.contains('.'))
    {
        return Err(Error::Invalid(
            "invalid Kubernetes resource name or namespace".into(),
        ));
    }
    Ok(())
}
pub fn validate(model: &mut ModelDeployment) -> Result<(), Error> {
    let model_name = model.name_any();
    validate_name(&model.name_any(), true)?;
    if !model
        .name_any()
        .starts_with(|c: char| c.is_ascii_lowercase())
    {
        return Err(Error::Invalid(
            "ModelDeployment name must start with a lowercase letter (Service DNS label)".into(),
        ));
    }
    validate_name(&model.namespace().unwrap_or_default(), true)?;
    let spec = &mut model.spec;
    if spec.model.id.is_empty()
        || spec.model.revision.is_empty()
        || spec.model.uri.is_empty()
        || spec.runtime.image.is_empty()
    {
        return Err(Error::Invalid(
            "model id, revision, uri and runtime image are required".into(),
        ));
    }
    if !spec
        .model
        .checksum
        .strip_prefix("sha256:")
        .is_some_and(|hash| {
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(Error::Invalid(
            "model checksum must be sha256 followed by 64 lowercase hexadecimal characters".into(),
        ));
    }
    if !matches!(
        spec.runtime.protocol.as_str(),
        "openai" | "triton-grpc" | "custom-http"
    ) {
        return Err(Error::Invalid("unsupported runtime protocol".into()));
    }
    if spec.runtime.port == 0 {
        spec.runtime.port = 8000;
    }
    if !(1..=65535).contains(&spec.runtime.port) || spec.replicas < 0 {
        return Err(Error::Invalid(
            "invalid runtime port or negative replicas".into(),
        ));
    }
    if !spec.runtime.health.path.starts_with('/')
        || spec.runtime.health.path.starts_with("//")
        || spec.runtime.health.path.len() > 256
        || !spec
            .runtime
            .health
            .path
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
        || !(10..=7200).contains(&spec.runtime.health.startup_timeout_seconds)
        || spec.runtime.health.startup_timeout_seconds % 5 != 0
    {
        return Err(Error::Invalid("runtime health requires a local HTTP path and startup timeout 10..7200 seconds in multiples of five".into()));
    }
    if let Some(scaling) = &spec.autoscaling {
        if scaling.managed && (spec.serving.is_none() || (scaling.target_pending_requests==0 && scaling.target_running_requests==0)) {
            return Err(Error::Invalid("managed scaling requires a serving pool and EPP request metrics".into()));
        }
        if scaling.min_replicas < 1
            || scaling.max_replicas < scaling.min_replicas
            || spec.replicas < scaling.min_replicas
            || spec.replicas > scaling.max_replicas
            || scaling.target_pending_requests < 0
            || scaling.target_running_requests < 0
            || scaling
                .target_cpu_utilization_percentage
                .is_some_and(|value| !(1..=100).contains(&value))
        {
            return Err(Error::Invalid("autoscaling requires 1 <= minReplicas <= replicas <= maxReplicas and CPU target 1..100".into()));
        }
        if (scaling.target_pending_requests > 0 || scaling.target_running_requests > 0)
            && spec.serving.is_none()
        {
            return Err(Error::Invalid(
                "EPP scaling requires a pool-specific serving entry".into(),
            ));
        }
        if (scaling.target_pending_requests == 0 && scaling.target_running_requests == 0)
            || scaling.target_cpu_utilization_percentage.is_some()
        {
            let cpu = spec.resources.requests.as_ref().and_then(|r| r.get("cpu"));
            if cpu.is_none_or(|cpu| cpu.0.trim().is_empty()) {
                return Err(Error::Invalid("CPU autoscaling requires resources.requests.cpu (quantity validation belongs to Kubernetes)".into()));
            }
        }
    }
    if spec
        .disruption_budget
        .as_ref()
        .is_some_and(|pdb| pdb.max_unavailable < 0)
    {
        return Err(Error::Invalid("maxUnavailable must be non-negative".into()));
    }
    if let Some(serving) = &spec.serving {
        validate_name(&serving.endpoint_picker_service, true)?;
        if serving.endpoint_picker_service == model_name
            || !(1..=65535).contains(&serving.endpoint_picker_port)
            || spec.runtime.protocol != "openai"
        {
            return Err(Error::Invalid(
                "serving requires OpenAI protocol, a separate EPP Service and valid port".into(),
            ));
        }
    }
    if let Some(rollout) = &mut spec.rollout {
        if rollout.strategy.is_empty() {
            rollout.strategy = rolling();
        }
        if rollout.strategy != "rolling" || rollout.canary_weight != 0 {
            return Err(Error::Invalid("only rolling is currently supported".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
// This compile-time JSON fixture is kept in lockstep with ModelDeploymentSpec.
#[allow(clippy::unwrap_used)]
pub(crate) fn example() -> ModelDeployment {
    let mut model = ModelDeployment::new("demo", serde_json::from_value(serde_json::json!({
        "model":{"id":"demo","revision":"v1","uri":"s3://models/demo","checksum":format!("sha256:{}", "a".repeat(64))},
        "runtime":{"image":"runtime:test","protocol":"openai"},"replicas":2,"resources":{"limits":{"cpu":"200m"}}
    })).unwrap());
    model.metadata.namespace = Some("xscope-system".into());
    model.metadata.uid = Some("model-uid".into());
    model.metadata.generation = Some(1);
    model
}
#[cfg(test)]
// Test fixture setup and response assertions deliberately panic at the failing boundary.
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn runtime_startup_readiness_is_engine_specific_and_bounded() {
        let mut model = example();
        model.spec.runtime.health.path = "/health".into();
        model.spec.runtime.health.startup_timeout_seconds = 600;
        validate(&mut model).unwrap();
        let (deployment, _) = crate::controller::desired(&model, "local", "local", None).unwrap();
        let pod = deployment.spec.unwrap().template.spec.unwrap();
        let runtime = &pod.containers[0];
        assert_eq!(
            runtime.startup_probe.as_ref().unwrap().failure_threshold,
            Some(120)
        );
        assert_eq!(
            runtime
                .readiness_probe
                .as_ref()
                .unwrap()
                .http_get
                .as_ref()
                .unwrap()
                .path
                .as_deref(),
            Some("/health")
        );
        for path in [
            "//external",
            "http://external",
            "/health?token=private",
            "/health\r\n",
        ] {
            model.spec.runtime.health.path = path.into();
            assert!(validate(&mut model).is_err());
        }
        model.spec.runtime.health.path = "/health".into();
        model.spec.runtime.health.startup_timeout_seconds = 601;
        assert!(validate(&mut model).is_err());
    }
    #[test]
    fn compatible_defaults_and_validation() {
        let mut model = example();
        validate(&mut model).unwrap();
        assert_eq!(model.spec.runtime.port, 8000);
        let json = serde_json::to_value(&model).unwrap();
        assert_eq!(json["apiVersion"], "platform.xscope.io/v1alpha1");
        assert_eq!(json["kind"], "ModelDeployment");
        model.spec.model.checksum = "invalid".into();
        assert!(validate(&mut model).is_err());
        let mut model = example();
        model.spec.runtime.port = 65536;
        assert!(validate(&mut model).is_err());
        let mut model = example();
        model.spec.rollout = Some(RolloutSpec {
            strategy: "canary".into(),
            canary_weight: 10,
        });
        assert!(validate(&mut model).is_err());
        assert!(validate_name("../secrets", true).is_err());
    }
}
