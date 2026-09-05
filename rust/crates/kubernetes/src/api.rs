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
}
fn default_port() -> i32 {
    8000
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutoscalingSpec {
    #[serde(default)]
    pub min_replicas: i32,
    #[serde(default)]
    pub max_replicas: i32,
    #[serde(default)]
    pub target_pending_requests: i32,
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
    validate_name(&model.name_any(), false)?;
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
    if let Some(rollout) = &mut spec.rollout {
        if rollout.strategy.is_empty() {
            rollout.strategy = rolling();
        }
        if rollout.strategy != "rolling" {
            return Err(Error::Invalid("only rolling is currently supported".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
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
mod tests {
    use super::*;
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
