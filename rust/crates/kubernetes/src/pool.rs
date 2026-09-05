//! Typed subset of the official GAIE v1 schema installed by //deploy/inference:crds.
//! This type does not generate or install a competing CRD.
use kube::CustomResource;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(CustomResource, Clone, Debug, Deserialize, Serialize)]
#[kube(
    group = "inference.networking.k8s.io",
    version = "v1",
    kind = "InferencePool",
    plural = "inferencepools",
    namespaced,
    schema = "disabled"
)]
#[serde(rename_all = "camelCase")]
pub struct InferencePoolSpec {
    pub selector: PoolSelector,
    pub target_ports: Vec<PoolPort>,
    pub endpoint_picker_ref: EndpointPickerRef,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolSelector {
    pub match_labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PoolPort {
    pub number: i32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointPickerRef {
    pub name: String,
    pub port: PoolPort,
    pub failure_mode: String,
}
