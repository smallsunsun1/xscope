"""Scoped local deployment. Private state/config stays in Secrets; no DB resets."""
import argparse
import base64
import copy
import json
import os
from pathlib import Path
import subprocess
import time
import uuid
import yaml
from observability_cluster import local_only

NS = "xscope-system"
OWNER = "platform.xscope.io/safe-scaling-local"


def kube(*args, payload=None, namespace=NS, timeout=180):
    command = ["kubectl", "--context", "docker-desktop"]
    if namespace:
        command += ["-n", namespace]
    result = subprocess.run(command + list(args), input=json.dumps(payload) if payload is not None else None,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=timeout)
    if result.returncode:
        reason = next((word for word in ["Forbidden", "Invalid", "NotFound", "Conflict", "immutable", "not found", "cannot", "required"] if word.lower() in result.stderr.lower()), "unclassified")
        print("Scoped operation failed:", args[0], reason, flush=True)
        raise RuntimeError("scoped Kubernetes operation failed; private output suppressed")
    return result.stdout


def get(kind, name, namespace=NS):
    value = kube("get", kind, name, "--ignore-not-found", "-o", "json", namespace=namespace)
    return json.loads(value) if value.strip() else None


def patch(kind, name, value):
    return kube("patch", kind, name, "--type=strategic", "--patch-file=/dev/stdin", payload=value)


def owned_apply(value):
    metadata = value["metadata"]
    current = get(value["kind"], metadata["name"], metadata.get("namespace"))
    if current and current["metadata"].get("annotations", {}).get(OWNER) != "true":
        raise RuntimeError("refusing to adopt an unrelated resource")
    metadata.setdefault("annotations", {})[OWNER] = "true"
    kube("apply", "--server-side", "--field-manager=xscope-safe-scaling", "-f", "-", payload=value, namespace=None)


def secret(name, updates):
    old = get("secret", name)
    value = {"apiVersion": "v1", "kind": "Secret", "metadata": {"name": name, "namespace": NS}, "type": "Opaque",
        "data": {**(old or {}).get("data", {}), **{k: base64.b64encode(v.encode()).decode() for k, v in updates.items()}}}
    if old:
        value["metadata"]["resourceVersion"] = old["metadata"]["resourceVersion"]
        kube("replace", "-f", "-", payload=value)
    else:
        kube("create", "-f", "-", payload=value)


def private_state(name):
    value = get("secret", name)
    return json.loads(base64.b64decode(value["data"]["state.json"])) if value else None


def maintenance(command):
    result = subprocess.run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", "-i", "deployment/control-plane", "--",
        "env", "XSCOPE_LOCAL_MAINTENANCE=enabled", "/usr/local/bin/control-plane", "--maintenance"],
        input=json.dumps(command), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=45)
    if result.returncode:
        raise RuntimeError("local maintenance operation failed; private output suppressed")
    return json.loads(result.stdout)


def wait(name):
    kube("rollout", "status", "deployment/" + name, "--timeout=180s", timeout=200)
    print("Deployment ready:", name if name in ("control-plane", "gateway", "operator", "cluster-agent", "jaeger", "prometheus", "xscope-member-agent") else "managed serving", flush=True)


def upgrade(root):
    for name in ["gateway", "control-plane", "operator", "cluster-agent"]:
        subprocess.run(["docker", "image", "inspect", "xscope/" + name + ":dev"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["bazel", "run", "//tools:prepare_usage_cutover", "--", "--accept-volatile-cutover"], cwd=root, check=True)
    gateway = get("deployment", "gateway")
    count = gateway["spec"].get("replicas", 1)
    if count < 1:
        raise RuntimeError("gateway already stopped; inspect previous maintenance before resuming")
    print("Beginning scoped maintenance: Gateway drains while control plane remains available", flush=True)
    kube("scale", "deployment/gateway", "--replicas=0")
    kube("wait", "--for=delete", "pod", "-l", "app.kubernetes.io/name=gateway", "--timeout=90s")
    subprocess.run(["bazel", "run", "//tools:deploy_billing_protocol", "--", "--quiesce-only"], cwd=root, check=True)
    resume(root, count)


def merge(old, new):
    if isinstance(old, dict) and isinstance(new, dict):
        result = copy.deepcopy(old)
        for key, value in new.items():
            result[key] = merge(result.get(key), value)
        return result
    return copy.deepcopy(new)


def resume(root, count=1):
    for filename in ["modeldeployment.yaml", "modelscale.yaml"]:
        wanted = yaml.safe_load((root / "deploy/k8s/crds" / filename).read_text())
        current = get("crd", wanted["metadata"]["name"], None)
        if current:
            versions = current["spec"]["versions"]
            for version in wanted["spec"]["versions"]:
                match = next((i for i, v in enumerate(versions) if v["name"] == version["name"]), None)
                if match is None:
                    versions.append(version)
                else:
                    versions[match] = merge(versions[match], version)
            current.pop("status", None)
            kube("replace", "-f", "-", payload=current, namespace=None)
        else:
            kube("create", "-f", "-", payload=wanted, namespace=None)
    role = get("clusterrole", "xscope-operator", None)
    wanted = {"apiGroups": ["platform.xscope.io"], "resources": ["modelscales", "modelscales/status"], "verbs": ["get", "list", "watch", "create", "patch", "delete"]}
    if wanted not in role["rules"]:
        role["rules"].append(wanted)
        kube("replace", "-f", "-", payload=role, namespace=None)
    secret("xscope-backend-runtime", {"XSCOPE_REQUIRE_GATEWAY_IDENTITY": "true"})
    kube("scale", "deployment/control-plane", "--replicas=1")
    wait("control-plane")
    for name in ["operator", "cluster-agent"]:
        kube("rollout", "restart", "deployment/" + name)
        wait(name)
    kube("rollout", "restart", "deployment/gateway")
    kube("scale", "deployment/gateway", f"--replicas={count}")
    wait("gateway")
    print("Release services upgraded; financial schema backup retained, no identities or PVCs reset", flush=True)


def member():
    state = private_state("xscope-member-runtime")
    if state is None:
        identifier = "local-" + uuid.uuid4().hex[:12]
        result = maintenance({"action": "register_cluster", "request": {"id": identifier, "namespace": NS, "credential_days": 90}})
        state = {"cluster_id": identifier, "credential": result["credential"]}
    config = {"cluster_id": state["cluster_id"], "namespace": NS, "control_url": "http://control-plane.xscope-system.svc:8081",
        "credential_file": "/etc/xscope-member/credential", "allow_local_http": True, "prometheus_url": "http://prometheus.xscope-system.svc:9090"}
    # Member API is on the internal listener, not the console listener.
    config["control_url"] = "http://control-plane.xscope-system.svc:8084"
    secret("xscope-member-runtime", {"state.json": json.dumps(state), "cluster-id": state["cluster_id"], "credential": state["credential"], "config.json": json.dumps(config)})
    name = "xscope-member-agent"
    owned_apply({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": name, "namespace": NS}})
    rules = [
        {"apiGroups": ["platform.xscope.io"], "resources": ["modeldeployments"], "verbs": ["get", "create", "update", "delete"]},
        {"apiGroups": ["platform.xscope.io"], "resources": ["modelscales"], "verbs": ["get"]},
        {"apiGroups": ["apps"], "resources": ["deployments"], "verbs": ["get"]},
        {"apiGroups": [""], "resources": ["services"], "verbs": ["get"]},
        {"apiGroups": [""], "resources": ["pods"], "verbs": ["get", "patch"]},
        {"apiGroups": ["inference.networking.k8s.io"], "resources": ["inferencepools"], "verbs": ["get"]},
        {"apiGroups": ["autoscaling"], "resources": ["horizontalpodautoscalers"], "verbs": ["get"]},
        {"apiGroups": ["keda.sh"], "resources": ["scaledobjects"], "verbs": ["get"]},
        {"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["get", "create", "patch"]},
    ]
    for kind, namespace, ruleset, suffix in [("Role", NS, rules, ""), ("ClusterRole", None, [{"apiGroups": [""], "resources": ["nodes"], "verbs": ["get"]}], "-nodes"),
        ("Role", "kube-node-lease", [{"apiGroups": ["coordination.k8s.io"], "resources": ["leases"], "verbs": ["get"]}], "-leases")]:
        metadata = {"name": name + suffix, **({"namespace": namespace} if namespace else {})}
        owned_apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind, "metadata": copy.deepcopy(metadata), "rules": ruleset})
        owned_apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": kind + "Binding", "metadata": metadata,
            "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": kind, "name": name + suffix}, "subjects": [{"kind": "ServiceAccount", "name": name, "namespace": NS}]})
    owned_apply({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "namespace": NS}, "spec": {"replicas": 1,
        "selector": {"matchLabels": {"app.kubernetes.io/name": name}}, "template": {"metadata": {"labels": {"app.kubernetes.io/name": name, "app.kubernetes.io/component": "kubernetes-member", "app.kubernetes.io/part-of": "xscope"}},
            "spec": {"serviceAccountName": name, "securityContext": {"runAsNonRoot": True, "runAsUser": 65532, "fsGroup": 65532},
                "containers": [{"name": "cluster-agent", "image": "xscope/cluster-agent:dev", "imagePullPolicy": "IfNotPresent",
                    "env": [{"name": "XSCOPE_CLUSTER_PULL_CONFIG_FILE", "value": "/etc/xscope-member/config.json"}],
                    "ports": [{"name": "http", "containerPort": 8083}], "readinessProbe": {"httpGet": {"path": "/readyz", "port": "http"}},
                    "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}, "limits": {"cpu": "200m", "memory": "128Mi"}},
                    "securityContext": {"allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True}, "volumeMounts": [{"name": "config", "mountPath": "/etc/xscope-member", "readOnly": True}]}],
                "volumes": [{"name": "config", "secret": {"secretName": "xscope-member-runtime", "defaultMode": 288}}]}}}})
    policy = get("networkpolicy", "control-plane-ingress")
    rule = {"from": [{"podSelector": {"matchLabels": {"app.kubernetes.io/component": component}}} for component in ["api-gateway", "kubernetes-member", "inference-routing"]], "ports": [{"protocol": "TCP", "port": 8084}]}
    if rule not in policy["spec"]["ingress"]:
        policy["spec"]["ingress"].append(rule)
        kube("replace", "-f", "-", payload=policy)
    owned_apply({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": "xscope-scaling-metrics", "namespace": NS},
        "spec": {"podSelector": {"matchLabels": {"app.kubernetes.io/name": "prometheus"}}, "policyTypes": ["Ingress"], "ingress": [{"from": [
            {"podSelector": {"matchLabels": {"app.kubernetes.io/component": "kubernetes-member"}}},
            {"namespaceSelector": {"matchLabels": {"kubernetes.io/metadata.name": "keda"}}}], "ports": [{"protocol": "TCP", "port": 9090}]}]}})
    wait(name)
    # Optional identity Secret lets older local installs keep their working console.
    desired = next(v for v in yaml.safe_load_all((Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / "deploy/k8s/base/workloads.yaml").read_text()) if v["metadata"]["name"] == "gateway")
    identity_env = [e for e in desired["spec"]["template"]["spec"]["containers"][0]["env"] if e["name"] in ["XSCOPE_GATEWAY_CLUSTER_ID", "XSCOPE_POD_NAME", "XSCOPE_POD_UID", "XSCOPE_POD_NAMESPACE"]]
    patch("deployment", "gateway", {"spec": {"template": {"metadata": {"labels": {"app.kubernetes.io/component": "api-gateway"}}, "spec": {"automountServiceAccountToken": False, "containers": [{"name": "gateway", "env": identity_env}]}}}})
    wait("gateway")
    return state


def observability(root):
    # Preserve all non-scrape local entries; replace only the two serving jobs.
    deployment = get("deployment", "prometheus")
    volume = next(v for v in deployment["spec"]["template"]["spec"]["volumes"] if v["name"] == "config")
    old = get("configmap", volume["configMap"]["name"]) if "configMap" in volume else get("secret", volume["secret"]["secretName"])
    data = old["data"] if old["kind"] == "ConfigMap" else {k: base64.b64decode(v).decode() for k, v in old["data"].items()}
    live = yaml.safe_load(data["prometheus.yaml"])
    desired = yaml.safe_load((root / "deploy/k8s/observability/prometheus.yaml").read_text())
    replacement = {s["job_name"]: s for s in desired["scrape_configs"] if s["job_name"] in ("envoy", "epp")}
    live["scrape_configs"] = [replacement.get(s["job_name"], s) for s in live["scrape_configs"]]
    secret("xscope-prometheus-runtime", {**data, "prometheus.yaml": yaml.safe_dump(live)})
    volumes = [{"name": "config", "secret": {"secretName": "xscope-prometheus-runtime"}} if v["name"] == "config" else v for v in deployment["spec"]["template"]["spec"]["volumes"]]
    kube("patch", "deployment", "prometheus", "--type=merge", "--patch-file=/dev/stdin", payload={"metadata": {"resourceVersion": deployment["metadata"]["resourceVersion"]}, "spec": {"template": {"spec": {"volumes": volumes}}}})
    wait("prometheus")
    secret("xscope-jaeger-runtime", {"jaeger.yaml": (root / "deploy/k8s/observability/jaeger.yaml").read_text()})
    jaeger = get("deployment", "jaeger")
    volumes = [{"name": "config", "secret": {"secretName": "xscope-jaeger-runtime"}} if v["name"] == "config" else v for v in jaeger["spec"]["template"]["spec"]["volumes"]]
    kube("patch", "deployment", "jaeger", "--type=merge", "--patch-file=/dev/stdin", payload={"metadata": {"resourceVersion": jaeger["metadata"]["resourceVersion"]}, "spec": {"template": {"spec": {"volumes": volumes}}}})
    patch("deployment", "jaeger", {"spec": {"template": {"spec": {"containers": [{"name": "jaeger", "env": [{"name": "GOMEMLIMIT", "value": "512MiB"}], "resources": {"requests": {"cpu": "20m", "memory": "128Mi"}, "limits": {"cpu": "300m", "memory": "768Mi"}}}]}}}})
    wait("jaeger")


def bootstrap(root):
    state = member()
    observability(root)
    saved = private_state("xscope-managed-validation")
    if saved and saved.get("complete"):
        print("Existing managed validation state retained; no keys recreated")
        return
    if not saved:
        suffix = uuid.uuid4().hex[:10]
        saved = {"model": "managed-demo-" + suffix, "deployment": "model-" + suffix, "serving": "inference-serving-" + suffix,
            "project": "validation-" + suffix, "key_id": "validation-key-" + suffix, "cluster_id": state["cluster_id"]}
        secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    serving = saved["serving"]
    objects = [v for v in yaml.safe_load_all((root / "deploy/k8s/serving/resources.yaml").read_text()) if v]
    for value in objects:
        if value["kind"] not in ["ServiceAccount", "Role", "RoleBinding", "Service", "Deployment"]:
            continue
        value["metadata"]["name"] = serving
        if value["kind"] == "RoleBinding":
            value["roleRef"]["name"] = serving
            value["subjects"][0]["name"] = serving
        if value["kind"] == "Service":
            value["metadata"].setdefault("annotations", {}).update({"platform.xscope.io/inference-pool": saved["deployment"], "platform.xscope.io/managed-only": "true", "platform.xscope.io/traffic-protocol": "v2"})
            value["spec"]["selector"]["app.kubernetes.io/name"] = serving
        if value["kind"] == "Deployment":
            value["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"] = serving
            template = value["spec"]["template"]
            template["metadata"]["labels"]["app.kubernetes.io/name"] = serving
            template["spec"]["serviceAccountName"] = serving
            for container in template["spec"]["containers"]:
                if container["name"] == "epp":
                    container["args"] = [("--pool-name=" + saved["deployment"]) if a.startswith("--pool-name=") else a for a in container["args"]]
            for volume in template["spec"]["volumes"]:
                if volume["name"] in ["envoy-config", "epp-config"]:
                    volume.pop("configMap", None)
                    key = "envoy.yaml" if volume["name"] == "envoy-config" else "plugins.yaml"
                    volume["secret"] = {"secretName": "xscope-managed-entry", "items": [{"key": key, "path": key}]}
        owned_apply(value)
    # Generated profile is a Bazel output, never injected with credentials at build time.
    from python.runfiles import runfiles
    profile = Path(runfiles.Create().Rlocation(os.environ.get("XSCOPE_MANAGED_PROFILE_RUNFILE", "_main/deploy/inference/envoy-managed.json"))).read_text()
    secret("xscope-managed-entry", {"envoy.yaml": profile, "plugins.yaml": (root / "deploy/k8s/serving/plugins.yaml").read_text()})
    model = {"id": saved["model"], "display_name": "Local managed inference demo", "max_context_tokens": 32768, "price_version": "local-v1",
        "input_per_million_tokens": {"currency": "CNY", "amount": 1}, "output_per_million_tokens": {"currency": "CNY", "amount": 2}}
    if not saved.get("model_created"):
        maintenance({"action": "put_model", "id": saved["model"], "request": {"expected_revision": 0, "enabled": True, "default_pool": None, "model": model}})
        saved["model_created"] = True
        secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    if not saved.get("project_created"):
        maintenance({"action": "create_project", "project": {"id": saved["project"], "tenant_id": "tenant-local", "name": "Managed scaling validation"}})
        saved["project_created"] = True
        secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    if not saved.get("key"):
        key = maintenance({"action": "create_key", "request": {"id": saved["key_id"], "project_id": saved["project"], "tenant_id": "tenant-local", "name": "Local validation",
            "scopes": ["chat.completions"], "allowed_models": [saved["model"]], "rate_limit_rpm": 600, "rate_limit_tpm": 600000, "monthly_budget": {"currency": "CNY", "amount": 100}}})
        saved["key"] = key["secret"]
        secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    spec = {"model": {"id": saved["model"], "revision": "managed-v1", "uri": "synthetic://local-echo", "checksum": "sha256:" + "a" * 64},
        "runtime": {"image": "xscope/runtime:dev", "protocol": "openai", "port": 8000}, "replicas": 2,
        "resources": {"requests": {"cpu": "20m", "memory": "64Mi"}, "limits": {"cpu": "200m", "memory": "192Mi"}},
        "serving": {"endpointPickerService": serving, "endpointPickerPort": 9002}, "disruptionBudget": {"maxUnavailable": 1},
        "autoscaling": {"managed": True, "minReplicas": 1, "maxReplicas": 2, "targetRunningRequests": 1}}
    if not saved.get("desired_created"):
        maintenance({"action": "put_desired", "id": state["cluster_id"], "request": {"expected_version": 0, "deployments": [{"name": saved["deployment"], "spec": spec, "delete_uid": None}]}})
        saved["desired_created"] = True
        secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    if not saved.get("pool"):
        bound = maintenance({"action": "bind_pool", "request": {"cluster_id": state["cluster_id"], "deployment": saved["deployment"], "serving_service": serving,
            "address": f"{serving}.{NS}.svc:8085", "expected_desired_version": 1, "traffic_protocol": 2, "make_default": True}})
        saved["pool"] = bound["id"]
        secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    owned_apply({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": serving, "namespace": NS},
        "spec": {"podSelector": {"matchLabels": {"app.kubernetes.io/name": serving}}, "policyTypes": ["Ingress"], "ingress": [
            {"from": [{"podSelector": {"matchLabels": {"app.kubernetes.io/component": "api-gateway"}}}], "ports": [{"port": 8085, "protocol": "TCP"}]},
            {"from": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": "prometheus"}}}], "ports": [{"port": 9090, "protocol": "TCP"}, {"port": 19001, "protocol": "TCP"}]}]}})
    owned_apply({"apiVersion": "networking.k8s.io/v1", "kind": "NetworkPolicy", "metadata": {"name": saved["deployment"], "namespace": NS},
        "spec": {"podSelector": {"matchLabels": {"app.kubernetes.io/name": saved["deployment"]}}, "policyTypes": ["Ingress"], "ingress": [
            {"from": [{"podSelector": {"matchLabels": {"app.kubernetes.io/name": serving}}}], "ports": [{"port": 8000, "protocol": "TCP"}]}]}})
    saved["complete"] = True
    secret("xscope-managed-validation", {"state.json": json.dumps(saved)})
    wait(serving)
    print("Managed model and KEDA recommendation path configured; private test identity retained in Secret", flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("phase", choices=["upgrade", "resume", "bootstrap", "observability"])
    args = parser.parse_args()
    local_only()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    {"upgrade": upgrade, "resume": resume, "bootstrap": bootstrap, "observability": observability}[args.phase](root)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        detail = (" missing field " + str(error)) if isinstance(error, KeyError) else ""
        raise SystemExit("Safe scaling deployment paused (" + type(error).__name__ + detail + "); private outputs suppressed.") from None
