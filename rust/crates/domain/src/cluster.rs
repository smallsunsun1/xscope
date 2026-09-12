//! Versioned outbound cluster protocol. ACK means Kubernetes accepted the spec,
//! not that a model became ready. Omission never deletes a resource.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredDeployment {
    pub name: String,
    /// None is an explicit delete, requiring the last observed Kubernetes UID.
    pub spec: Option<Value>,
    pub delete_uid: Option<String>,
    /// Managed scaling is bound to the observed object, not just its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_uid: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredState {
    pub expected_version: i64,
    pub deployments: Vec<DesiredDeployment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterDelivery {
    pub cluster_id: String,
    pub namespace: String,
    pub version: i64,
    pub sha256: String,
    pub lease: String,
    pub lease_seconds: u32,
    pub deployments: Vec<DesiredDeployment>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterReport {
    pub version: i64,
    pub sha256: String,
    pub lease: String,
    /// applied, rejected. Error codes are bounded, never cloud/API response text.
    pub outcome: String,
    pub error_code: Option<String>,
}
