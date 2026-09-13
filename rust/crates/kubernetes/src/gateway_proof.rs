//! Read kubelet termination state, never equate a missing Pod with a dead process.
use crate::api::validate_name;
use k8s_openapi::api::{
    coordination::v1::Lease,
    core::v1::{Node, Pod},
};
use kube::{
    Api, Client, ResourceExt,
    api::{Patch, PatchParams},
};
use serde_json::json;
use xscope_domain::traffic::{GatewayProof, GatewayProofTask, GatewayRuntime};
pub const FINALIZER: &str = "platform.xscope.io/gateway-evidence";

pub fn idle_query(namespace: &str, service: &str, after: &str) -> Result<String, &'static str> {
    validate_name(namespace, true).map_err(|_| "invalid namespace")?;
    validate_name(service, true).map_err(|_| "invalid service")?;
    let fence: k8s_openapi::jiff::Timestamp = after.parse().map_err(|_| "invalid fence time")?;
    let labels = format!("job=\"envoy\",namespace=\"{namespace}\",service=\"{service}\"");
    Ok(format!(
        "(sum(envoy_http_downstream_rq_active{{{labels},envoy_http_conn_manager_prefix=\"serving\"}}) == 0) and on() (min(up{{{labels}}}) == 1) and on() (count(envoy_http_downstream_rq_active{{{labels},envoy_http_conn_manager_prefix=\"serving\"}}) == count(up{{{labels}}})) and on() (min(timestamp(envoy_http_downstream_rq_active{{{labels},envoy_http_conn_manager_prefix=\"serving\"}})) > {}) and on() (time() - min(timestamp(up{{{labels}}})) < 30)",
        fence.as_second().saturating_add(1)
    ))
}

pub async fn observe(
    client: Client,
    http: &reqwest::Client,
    namespace: &str,
    task: &GatewayProofTask,
) -> GatewayProof {
    let mut report = GatewayProof {
        session_id: task.session_id.clone(),
        nonce: task.nonce.clone(),
        outcome: "unknown".into(),
        runtime: task.runtime.clone(),
    };
    if let Ok((outcome, runtime)) = inspect(client, http, namespace, task).await {
        report.outcome = outcome.into();
        report.runtime = runtime;
    }
    report
}
async fn inspect(
    client: Client,
    http: &reqwest::Client,
    namespace: &str,
    task: &GatewayProofTask,
) -> Result<(&'static str, Option<GatewayRuntime>), ()> {
    if task.identity.namespace != namespace || validate_name(&task.identity.pod, true).is_err() {
        return Err(());
    }
    let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
    let Some(mut pod) = pods.get_opt(&task.identity.pod).await.map_err(|_| ())? else {
        return Ok((
            if task.closed {
                "cleaned"
            } else if task.runtime.is_none() {
                "unbound"
            } else {
                "unknown"
            },
            task.runtime.clone(),
        ));
    };
    if pod.uid().as_deref() != Some(&task.identity.pod_uid)
        || !(pod
            .labels()
            .get("app.kubernetes.io/name")
            .map(String::as_str)
            == Some("gateway")
            || pod
                .labels()
                .get("app.kubernetes.io/component")
                .map(String::as_str)
                == Some("api-gateway"))
    {
        return Err(());
    }
    if task.closed {
        if !task.release_finalizer {
            return Ok(("cleaned", task.runtime.clone()));
        }
        if pod.metadata.deletion_timestamp.is_none()
            && pod.finalizers().iter().any(|v| v == FINALIZER)
        {
            return Err(());
        }
        let finalizers: Vec<_> = pod
            .finalizers()
            .iter()
            .filter(|f| *f != FINALIZER)
            .cloned()
            .collect();
        if finalizers.len() != pod.finalizers().len() {
            pods.patch(&pod.name_any(),&PatchParams::default(),&Patch::Merge(json!({"metadata":{"uid":pod.uid(),"resourceVersion":pod.resource_version(),"finalizers":finalizers}}))).await.map_err(|_|())?;
        }
        return Ok(("cleaned", task.runtime.clone()));
    }
    let node_name = pod
        .spec
        .as_ref()
        .and_then(|s| s.node_name.clone())
        .ok_or(())?;
    let nodes: Api<Node> = Api::all(client.clone());
    let node = nodes.get(&node_name).await.map_err(|_| ())?;
    let leases: Api<Lease> = Api::namespaced(client, "kube-node-lease");
    let lease = leases.get(&node_name).await.map_err(|_| ())?;
    if !fresh_node(&node, &lease) {
        return Err(());
    }
    let node_uid = node.uid().ok_or(())?;
    let status = pod
        .status
        .as_ref()
        .and_then(|s| s.container_statuses.as_ref())
        .and_then(|cs| cs.iter().find(|c| c.name == "gateway"))
        .ok_or(())?;
    if let Some(runtime) = &task.runtime {
        if runtime.node != node_name || runtime.node_uid != node_uid {
            return Err(());
        }
        let terminated = status
            .state
            .as_ref()
            .and_then(|s| s.terminated.as_ref())
            .into_iter()
            .chain(
                status
                    .last_state
                    .as_ref()
                    .and_then(|s| s.terminated.as_ref()),
            );
        return Ok((
            if terminated
                .into_iter()
                .any(|s| s.container_id.as_deref() == Some(&runtime.container_id))
            {
                "terminated"
            } else {
                "unknown"
            },
            Some(runtime.clone()),
        ));
    }
    if pod.metadata.deletion_timestamp.is_some() {
        return Ok(("unbound", None));
    }
    if !status.state.as_ref().is_some_and(|s| s.running.is_some()) {
        return Err(());
    }
    let container_id = status.container_id.clone().ok_or(())?;
    let runtime = GatewayRuntime {
        container_id: container_id.clone(),
        node: node_name,
        node_uid,
    };
    if !pod.finalizers().iter().any(|f| f == FINALIZER) {
        let mut finalizers = pod.finalizers().to_vec();
        finalizers.push(FINALIZER.into());
        pod=pods.patch(&pod.name_any(),&PatchParams::default(),&Patch::Merge(json!({"metadata":{"uid":pod.uid(),"resourceVersion":pod.resource_version(),"finalizers":finalizers}}))).await.map_err(|_|())?;
    }
    let ip: std::net::IpAddr = pod
        .status
        .as_ref()
        .and_then(|s| s.pod_ip.as_deref())
        .ok_or(())?
        .parse()
        .map_err(|_| ())?;
    let port = pod
        .spec
        .as_ref()
        .and_then(|s| s.containers.iter().find(|c| c.name == "gateway"))
        .and_then(|c| c.ports.as_ref())
        .and_then(|ps| ps.iter().find(|p| p.name.as_deref() == Some("http")))
        .and_then(|p| u16::try_from(p.container_port).ok())
        .ok_or(())?;
    let address = std::net::SocketAddr::new(ip, port);
    let value: serde_json::Value = http
        .get(format!("http://{address}/healthz"))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?
        .json()
        .await
        .map_err(|_| ())?;
    if value["session_id"] != task.session_id {
        return Ok(("unbound", None));
    }
    let current = pods.get(&pod.name_any()).await.map_err(|_| ())?;
    if current.uid() != pod.uid()
        || current.metadata.deletion_timestamp.is_some()
        || !current
            .status
            .as_ref()
            .and_then(|s| s.container_statuses.as_ref())
            .is_some_and(|cs| {
                cs.iter().any(|c| {
                    c.name == "gateway"
                        && c.container_id.as_deref() == Some(&container_id)
                        && c.state.as_ref().is_some_and(|s| s.running.is_some())
                })
            })
    {
        return Err(());
    }
    Ok(("bound", Some(runtime)))
}

fn fresh_node(node: &Node, lease: &Lease) -> bool {
    let now = k8s_openapi::jiff::Timestamp::now().as_second();
    node.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .is_some_and(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
        && lease
            .owner_references()
            .iter()
            .any(|r| r.kind == "Node" && Some(&r.uid) == node.metadata.uid.as_ref())
        && lease
            .spec
            .as_ref()
            .and_then(|s| s.renew_time.as_ref())
            .is_some_and(|t| (-5..=30).contains(&(now - t.0.as_second())))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn stale_lease_or_replaced_node_never_proves_termination() {
        let mut node:Node=serde_json::from_value(json!({"metadata":{"uid":"synthetic-node"},"status":{"conditions":[{"type":"Ready","status":"True"}]}})).unwrap();
        let mut lease:Lease=serde_json::from_value(json!({"metadata":{"ownerReferences":[{"apiVersion":"v1","kind":"Node","name":"synthetic","uid":"synthetic-node"}]},"spec":{"renewTime":k8s_openapi::jiff::Timestamp::now().to_string()}})).unwrap();
        assert!(fresh_node(&node, &lease));
        node.metadata.uid = Some("replacement".into());
        assert!(!fresh_node(&node, &lease));
        node.metadata.uid = Some("synthetic-node".into());
        lease.spec.as_mut().unwrap().renew_time = None;
        assert!(!fresh_node(&node, &lease));
    }
}
