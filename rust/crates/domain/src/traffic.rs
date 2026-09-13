//! Managed pools are opt-in, never retroactive safety claims for legacy entries.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const LEASE_SECONDS: u64 = 15;
pub fn managed(id: &str) -> bool {
    id.starts_with("managed-")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PoolBinding {
    #[serde(default = "legacy_protocol")]
    pub traffic_protocol: u32,
    /// Assign only an unbound default; never replace an existing stable route.
    #[serde(default)]
    pub make_default: bool,
    pub cluster_id: String,
    pub deployment: String,
    /// Installation-owned, same-namespace Envoy + EPP Service/Deployment name.
    pub serving_service: String,
    /// Approved entry transport (runtime configuration, never a client URL).
    pub address: String,
    #[serde(default)]
    pub tls: bool,
    #[serde(default)]
    pub server_name: String,
    pub expected_desired_version: i64,
}
fn legacy_protocol() -> u32 {
    1
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PoolControl {
    pub id: String,
    pub generation: i64,
    pub accepting: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrafficSnapshot {
    #[serde(default)]
    pub admission_token: String,
    pub session_id: String,
    pub sequence: i64,
    pub nonce: String,
    pub lease_seconds: u64,
    pub pools: Vec<PoolControl>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrafficReport {
    #[serde(default)]
    pub closed: bool,
    pub session_id: String,
    pub sequence: i64,
    pub nonce: String,
    pub active: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservationTask {
    #[serde(default)]
    pub traffic_protocol: u32,
    #[serde(default)]
    pub idle_after: Option<String>,
    pub pool_id: String,
    pub generation: i64,
    pub nonce: String,
    pub desired_version: i64,
    pub deployment: String,
    pub serving_service: String,
    pub expected_uid: Option<String>,
    pub expected_spec: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PoolObservation {
    #[serde(default)]
    pub idle_after: Option<String>,
    #[serde(default)]
    pub applied_replicas: i32,
    #[serde(default)]
    pub scaling: Option<ScaleEvidence>,
    #[serde(default)]
    pub idle: bool,
    pub pool_id: String,
    pub generation: i64,
    pub nonce: String,
    pub deployment_uid: Option<String>,
    pub ready: bool,
    /// ok/not_ready/spec_drift/identity_mismatch/kubernetes_unavailable only.
    pub code: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScaleEvidence {
    pub uid: String,
    pub resource_version: String,
    pub replicas: i32,
    pub applied_replicas: i32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayIdentity {
    pub cluster_id: String,
    pub namespace: String,
    pub pod: String,
    pub pod_uid: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayRuntime {
    pub container_id: String,
    pub node: String,
    pub node_uid: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProofTask {
    #[serde(default)]
    pub release_finalizer: bool,
    pub session_id: String,
    pub nonce: String,
    pub identity: GatewayIdentity,
    pub runtime: Option<GatewayRuntime>,
    pub closed: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayProof {
    pub session_id: String,
    pub nonce: String,
    pub outcome: String,
    pub runtime: Option<GatewayRuntime>,
}
