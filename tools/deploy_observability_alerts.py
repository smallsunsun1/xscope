"""Update only the local Prometheus rule ConfigMap reference; retain its TSDB and other workloads."""
import json
import hashlib
import os
from pathlib import Path
import subprocess

import yaml
from observability_cluster import KUBE, forward, kubectl, local_only, request, eventually


def rules_only_config(current, alerts):
    # Retain every live non-rule entry, including local scrape customizations.
    # Content addressing makes retries idempotent without overwriting an older
    # ConfigMap, and a later normal kustomize rollout remains supported.
    data = {**current["data"], "alerts.yaml": alerts}
    fingerprint = hashlib.sha256(json.dumps(data, sort_keys=True, separators=(",", ":")).encode()).hexdigest()[:24]
    return {"apiVersion": "v1", "kind": "ConfigMap", "immutable": True,
            "metadata": {"name": "xscope-prometheus-rules-" + fingerprint, "namespace": "xscope-system"}, "data": data}


def main():
    local_only()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    resources = [r for r in yaml.safe_load_all(kubectl("kustomize", str(root / "deploy/k8s/overlays/local"))) if r]
    candidates = [r for r in resources if r["kind"] == "ConfigMap" and r["metadata"]["name"].startswith("xscope-prometheus-")]
    if len(candidates) != 1:
        raise RuntimeError("Expected exactly one rendered Prometheus configuration")
    config = candidates[0]
    deployment = json.loads(kubectl("get", "deployment", "prometheus", "-o", "json"))
    volume = next(v for v in deployment["spec"]["template"]["spec"]["volumes"] if v["name"] == "config")
    current = json.loads(kubectl("get", "configmap", volume["configMap"]["name"], "-o", "json"))
    config = rules_only_config(current, config["data"]["alerts.yaml"])
    checked = subprocess.run(KUBE + ["exec", "deployment/prometheus", "--", "promtool", "check", "rules", "/dev/stdin"],
                             input=config["data"]["alerts.yaml"], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if checked.returncode:
        raise RuntimeError("Prometheus rejected the new rule file; no rollout performed")
    subprocess.run(KUBE + ["apply", "--server-side", "--field-manager=xscope-observability-alerts", "-f", "-"], input=json.dumps(config), text=True, check=True)
    patch = {"metadata": {"resourceVersion": deployment["metadata"]["resourceVersion"]},
             "spec": {"template": {"spec": {"volumes": [{"name": "config", "configMap": {"name": config["metadata"]["name"]}}]}}}}
    subprocess.run(KUBE + ["patch", "deployment", "prometheus", "--type=strategic", "--patch-file=/dev/stdin"],
                   input=json.dumps(patch), text=True, check=True)
    print(kubectl("rollout", "status", "deployment/prometheus", "--timeout=180s"), end="", flush=True)
    required = {rule.get("alert") or rule.get("record") for group in yaml.safe_load(config["data"]["alerts.yaml"])["groups"] for rule in group["rules"]}
    with forward("prometheus", 9090) as base:
        def loaded():
            groups = request(base + "/api/v1/rules")["data"]["groups"]
            names = {rule["name"] for group in groups for rule in group["rules"] if rule.get("health") == "ok"}
            return required <= names
        eventually(loaded)
    print(f"PASS {len(required)} delivery/worker/SLO/audit rules loaded; scrape configuration, credentials, TSDB PVC and Grafana unchanged; notification receiver not configured by this tool")


if __name__ == "__main__":
    main()
