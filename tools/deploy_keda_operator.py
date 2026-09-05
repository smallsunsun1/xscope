"""Roll only the KEDA-related XScope components; leave gateway/billing untouched."""
import json
import os
from pathlib import Path
import subprocess
import yaml
from observability_cluster import KUBE, kubectl, local_only


def main():
    local_only()
    kubectl("get", "crd", "scaledobjects.keda.sh")
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    resources = list(yaml.safe_load_all((root / "deploy/k8s/crds/modeldeployment.yaml").read_text()))
    resources += [r for r in yaml.safe_load_all((root / "deploy/k8s/base/rbac.yaml").read_text())
                  if r["kind"] == "ClusterRole" and r["metadata"]["name"] == "xscope-operator"]
    resources += list(yaml.safe_load_all((root / "deploy/k8s/autoscaling/rbac.yaml").read_text()))
    payload = json.dumps({"apiVersion": "v1", "kind": "List", "items": resources})
    for dry in (True, False):
        subprocess.run(KUBE + ["apply", "-f", "-"] + (["--dry-run=server"] if dry else []), input=payload, text=True, check=True)
    for name in ("operator", "cluster-agent"):
        old = [p["metadata"]["name"] for p in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=" + name, "-o", "json"))["items"]]
        kubectl("rollout", "restart", "deployment/" + name)
        print(kubectl("rollout", "status", "deployment/" + name, "--timeout=150s"), end="", flush=True)
        for pod in old:
            kubectl("wait", "--for=delete", "pod/" + pod, "--timeout=60s")
    print("KEDA CRD contract, Operator RBAC, operator and cluster-agent updated; no other services or data reset.")


if __name__ == "__main__":
    main()
