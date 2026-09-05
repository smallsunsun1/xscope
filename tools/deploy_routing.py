"""Targeted local rollout of RoutePolicy support; never reapplies Secrets or PVCs."""
import json
import os
from pathlib import Path
import subprocess

import yaml
from observability_cluster import KUBE, kubectl, local_only


def apply(resource):
    subprocess.run(KUBE + ["apply", "-f", "-"], input=json.dumps(resource), text=True, check=True)


def main():
    local_only()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    resources = [r for r in yaml.safe_load_all(kubectl("kustomize", str(root / "deploy/k8s/overlays/local"))) if r]
    # Existing stable serving resources and all user data are outside this scope.
    allowed = {"ServiceAccount", "Role", "RoleBinding", "ConfigMap", "Deployment", "Service", "InferencePool", "NetworkPolicy"}
    for resource in resources:
        name = resource["metadata"]["name"]
        if resource["kind"] in allowed and (name.endswith("-canary") or name.startswith(("xscope-serving-envoy-canary-", "xscope-serving-epp-canary-", "runtime-canary"))):
            apply(resource)
    for name in ("runtime-canary", "inference-serving-canary"):
        print(kubectl("rollout", "status", "deployment/" + name, "--timeout=150s"), end="")

    config = next(r for r in resources if r["kind"] == "ConfigMap" and r["metadata"]["name"] == "xscope-config")
    patch = {"data": {k: config["data"][k] for k in ("XSCOPE_ADDITIONAL_SERVING_JSON", "XSCOPE_ROUTE_POOLS_JSON")}}
    kubectl("patch", "configmap", "xscope-config", "--type=merge", "-p", json.dumps(patch))
    # Images must have been built/loaded with tools/bazel-linux.sh images.
    # CP migrates the new table additively before gateways fetch new snapshots.
    for name in ("control-plane", "gateway"):
        old_pods = [pod["metadata"]["name"] for pod in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=" + name, "-o", "json"))["items"]]
        kubectl("rollout", "restart", "deployment/" + name)
        print(kubectl("rollout", "status", "deployment/" + name, "--timeout=150s"), end="")
        # Service port-forward may otherwise pick a still-draining old Pod.
        for pod in old_pods:
            kubectl("wait", "--for=delete", "pod/" + pod, "--timeout=60s")

    metrics_policy = next(r for r in resources if r["kind"] == "NetworkPolicy" and r["metadata"]["name"] == "xscope-metrics-ingress")
    apply(metrics_policy)
    prometheus = next(r for r in resources if r["kind"] == "ConfigMap" and r["metadata"]["name"].startswith("xscope-prometheus-"))
    apply(prometheus)
    desired = next(r for r in resources if r["kind"] == "Deployment" and r["metadata"]["name"] == "prometheus")
    volume = next(v for v in desired["spec"]["template"]["spec"]["volumes"] if v["name"] == "config")
    kubectl("patch", "deployment", "prometheus", "--type=strategic", "-p",
        json.dumps({"spec": {"template": {"spec": {"volumes": [volume]}}}}))
    print(kubectl("rollout", "status", "deployment/prometheus", "--timeout=150s"), end="")
    print("Route pools ready; default traffic remains stable. Secrets, PVCs, billing data and Grafana unchanged.")


if __name__ == "__main__":
    main()
