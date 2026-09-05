"""Apply only the rendered dashboard ConfigMap and Grafana volume reference in local K8s.

Preserves NodePort, credentials, PVCs and all other workload configuration. For
an existing Grafana installation; first installation uses tools/deploy-local.sh.
"""
import json
import os
from pathlib import Path
import subprocess

import yaml

from observability_cluster import KUBE, kubectl, local_only


def main():
    local_only()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    rendered = list(yaml.safe_load_all(kubectl("kustomize", str(root / "deploy/k8s/overlays/local"))))
    configs = [r for r in rendered if r and r["kind"] == "ConfigMap" and r["metadata"]["name"].startswith("xscope-grafana-dashboards-")]
    assert len(configs) == 1
    config = configs[0]
    assert {"xscope-overview.json", "xscope-traces.json"} <= config["data"].keys()
    deployment = json.loads(kubectl("get", "deployment", "grafana", "-o", "json"))
    volume = next(v for v in deployment["spec"]["template"]["spec"]["volumes"] if v["name"] == "dashboards")
    assert "configMap" in volume
    subprocess.run(KUBE + ["apply", "-f", "-"], input=json.dumps(config), text=True, check=True)
    patch = {"spec": {"template": {"spec": {"volumes": [{"name": "dashboards", "configMap": {"name": config["metadata"]["name"]}}]}}}}
    kubectl("patch", "deployment", "grafana", "--type=strategic", "-p", json.dumps(patch))
    print(kubectl("rollout", "status", "deployment/grafana", "--timeout=180s"), end="")
    print("Dashboard configuration updated; Service, credentials and persistent volumes unchanged.")


if __name__ == "__main__":
    main()
