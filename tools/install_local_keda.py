"""Pinned upstream KEDA installation; local-only sizing, no adoption or TLS bypass."""
import copy
import json
import subprocess
import sys

import yaml
from observability_cluster import local_only

KUBE = ["kubectl", "--context", "docker-desktop"]
OWNER = "platform.xscope.io/installed-by"
DIGESTS = {
    "keda": "8888d1896d316c9e087ce0ca3b72bd945dfbad6f3ef2d64ca24ee394feccc023",
    "keda-admission-webhooks": "0350cbb59471123623134801a3e9a753c90e0945bb4eb7e36086746fc4b35fb8",
    "keda-metrics-apiserver": "ba32172082a2ff935d5b62ffa0bd0698424d373b55c5cae0846db057e237b28e",
}


def render(resources):
    resources = copy.deepcopy(resources)
    for resource in resources:
        resource["metadata"].setdefault("annotations", {})[OWNER] = "xscope-local"
        if resource["kind"] == "Role" and resource["metadata"].get("namespace") == "keda" and resource["metadata"]["name"] == "keda-operator":
            # The release uses events.k8s.io, but leader election still writes
            # core/v1 Events. Keep this compatibility permission namespace-local.
            resource["rules"].append({"apiGroups": [""], "resources": ["events"], "verbs": ["create", "patch"]})
        if resource["kind"] != "Deployment":
            continue
        resource["spec"]["replicas"] = 1
        for container in resource["spec"]["template"]["spec"]["containers"]:
            image = container["image"].split(":")[0].split("/")[-1]
            if image not in DIGESTS:
                raise ValueError("Unexpected upstream KEDA image: " + container["image"])
            container["image"] = f"ghcr.io/kedacore/{image}:2.20.1@sha256:{DIGESTS[image]}"
            container["imagePullPolicy"] = "IfNotPresent"
            container["resources"] = {
                "requests": {"cpu": "25m", "memory": "128Mi"},
                "limits": {"cpu": "500m", "memory": "384Mi"},
            }
            env = container.setdefault("env", [])
            env[:] = [item for item in env if item["name"] != "GOMAXPROCS"]
            env.append({"name": "GOMAXPROCS", "value": "1"})
    return resources


def apply(resources, dry=False):
    args = ["apply", "--server-side", "--field-manager=xscope-keda-installer", "-f", "-"]
    if dry:
        args.append("--dry-run=server")
    subprocess.run(KUBE + args, input=json.dumps({"apiVersion": "v1", "kind": "List", "items": resources}), text=True, check=True)


def main():
    local_only()
    with open(sys.argv[1]) as source:
        resources = render([r for r in yaml.safe_load_all(source) if r])
    # Preflight the entire bundle, especially the singleton external-metrics
    # APIService, before making any changes. Never take over another install.
    for resource in resources:
        meta = resource["metadata"]
        args = [resource["kind"], meta["name"]]
        if meta.get("namespace"):
            args += ["-n", meta["namespace"]]
        current = subprocess.check_output(KUBE + ["get", *args, "--ignore-not-found", "-o", "json"], text=True)
        if current and json.loads(current)["metadata"].get("annotations", {}).get(OWNER) != "xscope-local":
            raise SystemExit(f"Refusing to adopt {resource['kind']}/{meta['name']}; configure the existing KEDA installation instead.")
    foundations = [r for r in resources if r["kind"] in ("Namespace", "CustomResourceDefinition")]
    apply(foundations, dry=True)
    apply(foundations)
    for resource in foundations:
        if resource["kind"] == "CustomResourceDefinition":
            subprocess.run(KUBE + ["wait", "crd/" + resource["metadata"]["name"], "--for=condition=Established", "--timeout=60s"], check=True)
    remaining = [r for r in resources if r not in foundations]
    apply(remaining, dry=True)
    apply(remaining)
    for resource in resources:
        if resource["kind"] == "Deployment":
            subprocess.run(KUBE + ["-n", "keda", "rollout", "status", "deployment/" + resource["metadata"]["name"], "--timeout=180s"], check=True)
    subprocess.run(KUBE + ["wait", "apiservice/v1beta1.external.metrics.k8s.io", "--for=condition=Available", "--timeout=90s"], check=True)
    print("KEDA 2.20.1 ready; three single-replica components request 75m CPU total. Upstream TLS/webhooks retained.")


if __name__ == "__main__":
    main()
