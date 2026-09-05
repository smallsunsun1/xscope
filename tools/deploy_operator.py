"""Targeted local stage-4 rollout. No database, serving pool, Secret or PVC resets."""
import json
import os
from pathlib import Path
import subprocess
import yaml
from observability_cluster import KUBE, kubectl, local_only


def main():
    local_only()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    subprocess.run(KUBE + ["apply", "--dry-run=server", "-f", str(root / "deploy/k8s/crds/modeldeployment.yaml")], check=True)
    subprocess.run(KUBE + ["apply", "-f", str(root / "deploy/k8s/crds/modeldeployment.yaml")], check=True)
    role = next(r for r in yaml.safe_load_all((root / "deploy/k8s/base/rbac.yaml").read_text()) if r["kind"] == "ClusterRole" and r["metadata"]["name"] == "xscope-operator")
    subprocess.run(KUBE + ["apply", "-f", "-"], input=json.dumps(role), text=True, check=True)
    for name in ("operator", "cluster-agent", "control-plane"):
        old = [p["metadata"]["name"] for p in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=" + name, "-o", "json"))["items"]]
        kubectl("rollout", "restart", "deployment/" + name)
        print(kubectl("rollout", "status", "deployment/" + name, "--timeout=150s"), end="", flush=True)
        for pod in old:
            kubectl("wait", "--for=delete", "pod/" + pod, "--timeout=60s")
    print("Operator, cluster-agent and control-plane rolled out. Existing serving entries and all stored data unchanged.")


if __name__ == "__main__":
    main()
