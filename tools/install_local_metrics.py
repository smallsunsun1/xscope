"""Install the pinned upstream metrics API only on Docker Desktop; do not adopt existing installs.

Local-only TLS exception: kubelet has a self-signed serving certificate. Production
must use a trusted kubelet CA and an authenticated APIService TLS configuration.
"""
import json
import subprocess
import sys
import yaml
from observability_cluster import local_only

KUBE = ["kubectl", "--context", "docker-desktop"]
OWNER = "platform.xscope.io/installed-by"


def main():
    local_only()
    with open(sys.argv[1]) as source:
        resources = [r for r in yaml.safe_load_all(source) if r]
    for resource in resources:
        meta = resource["metadata"]
        args = [resource["kind"], meta["name"]]
        if meta.get("namespace"):
            args += ["-n", meta["namespace"]]
        current = subprocess.check_output(KUBE + ["get", *args, "--ignore-not-found", "-o", "json"], text=True)
        if current and json.loads(current)["metadata"].get("annotations", {}).get(OWNER) != "xscope-local":
            raise SystemExit(f"Refusing to modify existing {resource['kind']}/{meta['name']}; use the cluster's existing metrics provider.")
        meta.setdefault("annotations", {})[OWNER] = "xscope-local"
        if resource["kind"] == "Deployment":
            container = resource["spec"]["template"]["spec"]["containers"][0]
            container["image"] = "registry.k8s.io/metrics-server/metrics-server:v0.8.1@sha256:b2d2efaf5ac3b366ed0f839d2412a2c4279d4fc2a2a733f12c52133faed36c41"
            container["args"].append("--kubelet-insecure-tls")
            container["resources"] = {"requests": {"cpu": "30m", "memory": "96Mi"}, "limits": {"cpu": "300m", "memory": "192Mi"}}
            container["env"] = [{"name": "GOMAXPROCS", "value": "1"}]
    subprocess.run(KUBE + ["apply", "--dry-run=server", "-f", "-"], input=json.dumps({"apiVersion": "v1", "kind": "List", "items": resources}), text=True, check=True)
    subprocess.run(KUBE + ["apply", "-f", "-"], input=json.dumps({"apiVersion": "v1", "kind": "List", "items": resources}), text=True, check=True)
    subprocess.run(KUBE + ["-n", "kube-system", "rollout", "status", "deployment/metrics-server", "--timeout=150s"], check=True)
    subprocess.run(KUBE + ["wait", "apiservice/v1beta1.metrics.k8s.io", "--for=condition=Available", "--timeout=90s"], check=True)
    print("Local metrics API ready (30m CPU request). Local-only kubelet TLS exception enabled; not a production manifest.")


if __name__ == "__main__":
    main()
