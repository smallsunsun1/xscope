"""Real Operator/KEDA/PDB/InferencePool lifecycle smoke on Docker Desktop only.

Temporary installation-owned Envoy/EPP and gateway fixtures exercise the managed
pool without registering it as public customer capacity. All fixtures are removed.
"""
import base64
import copy
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.error
import urllib.request
import uuid
import yaml

from observability_cluster import KUBE, eventually, forward, kubectl, local_only


def main():
    local_only()
    kubectl("get", "crd", "scaledobjects.keda.sh")
    name = "operator-smoke-" + uuid.uuid4().hex[:10]
    serving_name = name + "-serving"
    gateway_name = name + "-gateway"
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    fixtures = []
    model_uid = None
    burner = None
    scaler_uid = None
    token = base64.b64decode(kubectl("get", "secret", "xscope-platform-secrets", "-o", "jsonpath={.data.internal-token}")).decode()

    def obj(kind, name):
        # kubectl 1.25 may default the short name to autoscaling/v1, whose
        # status encodes v2 conditions only in compatibility annotations.
        if kind == "hpa":
            kind = "horizontalpodautoscalers.v2.autoscaling"
        raw = kubectl("get", kind, name, "--ignore-not-found", "-o", "json")
        return json.loads(raw) if raw.strip() else None

    def create(resource):
        raw = subprocess.check_output(KUBE + ["create", "-f", "-", "-o", "json"], input=json.dumps(resource), text=True)
        resource = json.loads(raw)
        fixtures.append((resource["kind"], resource["metadata"]["name"], resource["metadata"]["uid"]))
        return resource

    def remove_fixture(kind, resource_name, uid):
        existing = obj(kind, resource_name)
        if existing:
            assert existing["metadata"]["uid"] == uid, "refusing to delete replaced fixture"
            kubectl("delete", kind, resource_name, "--wait=true", "--timeout=60s")

    with forward("cluster-agent", 8083) as agent:
        path = "/v1/model-deployments/xscope-system/" + name

        def call(method, path, payload=None):
            req = urllib.request.Request(agent + path, method=method, data=None if payload is None else json.dumps(payload).encode(),
                headers={"Authorization": "Bearer " + token, "Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    return response.status, json.loads(response.read() or "null")
            except urllib.error.HTTPError as error:
                return error.code, json.loads(error.read())

        try:
            rendered = [r for r in yaml.safe_load_all(kubectl("kustomize", str(root / "deploy/k8s/serving"))) if r]
            serving = copy.deepcopy(next(r for r in rendered if r["kind"] == "Deployment"))
            serving["metadata"]["name"] = serving_name
            serving["spec"]["selector"]["matchLabels"]["app.kubernetes.io/name"] = serving_name
            template = serving["spec"]["template"]
            template["metadata"]["labels"]["app.kubernetes.io/name"] = serving_name
            template["spec"]["containers"][1]["args"][0] = "--pool-name=" + name
            create(serving)
            service = copy.deepcopy(next(r for r in rendered if r["kind"] == "Service"))
            service["metadata"]["name"] = serving_name
            service["metadata"]["annotations"] = {"platform.xscope.io/inference-pool": name}
            service["spec"]["selector"]["app.kubernetes.io/name"] = serving_name
            picker = create(service)

            # This same-name object is intentionally NOT owned by ModelDeployment.
            foreign = create({"apiVersion": "policy/v1", "kind": "PodDisruptionBudget", "metadata": {"name": name},
                "spec": {"maxUnavailable": 1, "selector": {"matchLabels": {"unrelated": name}}}})
            spec = {"model": {"id": "xscope-demo", "revision": "operator-v1", "uri": "echo://development", "checksum": "sha256:" + "a" * 64},
                "runtime": {"image": "xscope/runtime:dev", "protocol": "openai", "port": 8090}, "replicas": 1,
                "resources": {"requests": {"cpu": "20m", "memory": "48Mi"}, "limits": {"cpu": "100m", "memory": "192Mi"}},
                "autoscaling": {"minReplicas": 1, "maxReplicas": 2, "targetCpuUtilizationPercentage": 10},
                "disruptionBudget": {"maxUnavailable": 1}, "serving": {"endpointPickerService": serving_name}}
            status, model = call("POST", "/v1/model-deployments", {"apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelDeployment", "metadata": {"name": name, "namespace": "xscope-system"}, "spec": spec})
            assert status == 201, model
            model_uid = model["metadata"]["uid"]
            eventually(lambda: any(c["type"] == "ResourcesReady" and c["status"] == "False" for c in obj("modeldeployment", name).get("status", {}).get("conditions", [])))
            assert obj("deployment", name) is None and obj("hpa", name) is None
            assert obj("pdb", name)["metadata"]["uid"] == foreign["metadata"]["uid"]
            # Simulate the old Operator's exact direct-owner HPA. It must be
            # deleted before KEDA creates its distinct ScaledObject/HPA pair.
            legacy = create({"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler",
                "metadata": {"name": name, "ownerReferences": [{"apiVersion": "platform.xscope.io/v1alpha1",
                    "kind": "ModelDeployment", "name": name, "uid": model_uid, "controller": True}]},
                "spec": {"scaleTargetRef": {"apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelDeployment", "name": name},
                    "minReplicas": 1, "maxReplicas": 2,
                    "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 10}}}]}})
            remove_fixture("PodDisruptionBudget", name, foreign["metadata"]["uid"])
            fixtures.remove(("PodDisruptionBudget", name, foreign["metadata"]["uid"]))
            print("PASS foreign same-name PDB refused before any owned children were created", flush=True)

            def reconciled():
                model = obj("modeldeployment", name)
                status = model.get("status", {})
                return status.get("observedGeneration") == model["metadata"]["generation"] and status.get("readyReplicas", 0) >= 1 and status.get("inferencePool") == name
            eventually(reconciled)
            assert obj("hpa", name) is None
            fixtures.remove(("HorizontalPodAutoscaler", name, legacy["metadata"]["uid"]))
            print("PASS legacy direct-owned HPA removed before KEDA activation", flush=True)
            children = {kind: obj(kind, name) for kind in ("deployment", "service", "scaledobject", "pdb", "inferencepool")}
            scaler_uid = children["scaledobject"]["metadata"]["uid"]
            for child in children.values():
                assert child["metadata"]["ownerReferences"][0]["uid"] == model_uid
            assert children["scaledobject"]["spec"]["scaleTargetRef"]["kind"] == "ModelDeployment"
            eventually(lambda: obj("hpa", "keda-hpa-" + name) is not None)
            assert obj("hpa", "keda-hpa-" + name)["metadata"]["ownerReferences"][0]["uid"] == children["scaledobject"]["metadata"]["uid"]
            assert children["inferencepool"]["spec"]["selector"]["matchLabels"]["platform.xscope.io/deployment-uid"] == model_uid
            scale = json.loads(kubectl("get", "--raw", f"/apis/platform.xscope.io/v1alpha1/namespaces/xscope-system/modeldeployments/{name}/scale"))
            assert scale["status"]["replicas"] >= 1 and scale["status"]["selector"]
            assert call("PUT", path + "/scale", {"replicas": 0})[0] == 400
            assert call("PUT", path, {"resourceVersion": "stale", "spec": spec})[0] == 409
            print("PASS owned Deployment/Service/ScaledObject/PDB/InferencePool, KEDA-owned HPA, /scale contract, manual scale guard and update CAS", flush=True)

            # Deliberate child drift must be repaired, never adopted from elsewhere.
            kubectl("patch", "pdb", name, "--type=merge", "-p", '{"spec":{"maxUnavailable":0}}')
            eventually(lambda: obj("pdb", name)["spec"]["maxUnavailable"] == 1)
            print("PASS PDB drift repaired", flush=True)

            secret = "test-" + uuid.uuid4().hex
            create({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": gateway_name}, "stringData": {"keys.json": json.dumps([
                {"id": name, "tenant_id": "smoke", "project_id": "smoke", "secret": secret}])}})
            create({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": gateway_name}, "spec": {"replicas": 1,
                "selector": {"matchLabels": {"app.kubernetes.io/name": gateway_name}}, "template": {"metadata": {"labels": {"app.kubernetes.io/name": gateway_name}},
                    "spec": {"automountServiceAccountToken": False, "containers": [{"name": "gateway", "image": "xscope/gateway:dev", "imagePullPolicy": "IfNotPresent",
                        "env": [{"name": "XSCOPE_API_KEYS_JSON", "valueFrom": {"secretKeyRef": {"name": gateway_name, "key": "keys.json"}}},
                            {"name": "XSCOPE_SERVING_ENTRY_JSON", "value": json.dumps({"id": name, "model": "xscope-demo", "revision": "operator-v1", "address": serving_name + ":8085"})}],
                        "ports": [{"name": "http", "containerPort": 8080}], "readinessProbe": {"httpGet": {"path": "/readyz", "port": "http"}},
                        "resources": {"requests": {"cpu": "10m", "memory": "48Mi"}, "limits": {"cpu": "200m", "memory": "192Mi"}}}]}}}})
            create({"apiVersion": "v1", "kind": "Service", "metadata": {"name": gateway_name}, "spec": {"selector": {"app.kubernetes.io/name": gateway_name}, "ports": [{"port": 8080}]}})
            kubectl("rollout", "status", "deployment/" + serving_name, "--timeout=90s")
            kubectl("rollout", "status", "deployment/" + gateway_name, "--timeout=90s")
            with forward(gateway_name, 8080) as gateway:
                request_id = name + "-sse"
                req = urllib.request.Request(gateway + "/v1/chat/completions", data=json.dumps({"model": "xscope-demo", "stream": True,
                    "messages": [{"role": "user", "content": "operator managed pool"}]}).encode(),
                    headers={"Authorization": "Bearer " + secret, "Content-Type": "application/json", "X-Request-Id": request_id})
                with urllib.request.urlopen(req, timeout=15) as response:
                    assert response.status == 200 and response.headers["x-xscope-pool"] == name
                    assert b"[DONE]" in response.read()
                logs = kubectl("logs", "deployment/" + serving_name, "-c", "envoy", "--since=5m")
                selected = next(json.loads(line)["selected_endpoint"] for line in logs.splitlines() if line.startswith("{") and json.loads(line).get("request_id") == request_id)
                pods = json.loads(kubectl("get", "pods", "-l", "platform.xscope.io/deployment-uid=" + model_uid, "-o", "json"))["items"]
                assert selected in {p["status"].get("podIP", "") + ":8090" for p in pods}
            print("PASS real Pingora -> Envoy/EPP -> Operator-created pool -> managed echo Pod: " + selected, flush=True)

            # Generate real bounded CPU usage, not synthetic metrics. Limit is 100m.
            # Docker Desktop configures a 60s HPA sync period (not the usual
            # 15s). Allow metrics initialization plus multiple stabilization
            # cycles without changing shared controller-manager settings.
            burner = subprocess.Popen(KUBE + ["exec", "deployment/" + name, "--", "python3", "-c",
                "import time\nend=time.monotonic()+240\nwhile time.monotonic()<end: sum(range(10000))"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            def scaled():
                model = obj("modeldeployment", name)
                hpa = obj("hpa", "keda-hpa-" + name)
                return model["spec"]["replicas"] == 2 and hpa and hpa.get("status", {}).get("lastScaleTime") and any(c["type"] == "ScalingActive" and c["status"] == "True" for c in hpa.get("status", {}).get("conditions", []))
            eventually(scaled, timeout=300)
            eventually(lambda: obj("deployment", name).get("status", {}).get("readyReplicas") == 2)
            print("PASS real CPU metrics drove KEDA-owned HPA -> ModelDeployment -> two ready Runtime replicas", flush=True)
            burner.wait(timeout=250)
            burner = None

            def disable():
                current = obj("modeldeployment", name)
                disabled = copy.deepcopy(current["spec"])
                for field in ("autoscaling", "disruptionBudget", "serving"):
                    disabled.pop(field, None)
                disabled["replicas"] = 1
                return call("PUT", path, {"resourceVersion": current["metadata"]["resourceVersion"], "spec": disabled})[0] == 200
            eventually(disable)
            eventually(lambda: all(obj(kind, name) is None for kind in ("scaledobject", "pdb", "inferencepool")) and obj("hpa", "keda-hpa-" + name) is None)
            for kind in ("deployment", "service"):
                assert obj(kind, name)["metadata"]["uid"] == children[kind]["metadata"]["uid"]
            assert obj("service", serving_name)["metadata"]["uid"] == picker["metadata"]["uid"]
            assert obj("service", serving_name)["spec"] == picker["spec"]
            assert call("PUT", path + "/scale", {"replicas": 0})[0] == 200
            print("PASS disabling optional resources deletes only owned children; manual scaling resumes; external EPP Service unchanged", flush=True)
        finally:
            cleanup_errors = []
            if burner:
                burner.terminate()
                burner.wait(timeout=5)
            try:
                if model_uid:
                    current = obj("modeldeployment", name)
                    if current:
                        assert current["metadata"]["uid"] == model_uid
                        assert call("DELETE", path)[0] == 204
                    def owned_gone():
                        for kind in ("deployment", "service", "scaledobject", "pdb", "inferencepool"):
                            child = obj(kind, name)
                            if child and any(owner["uid"] == model_uid for owner in child["metadata"].get("ownerReferences", [])):
                                return False
                        hpa = obj("hpa", "keda-hpa-" + name)
                        if hpa and any(owner["uid"] == scaler_uid for owner in hpa["metadata"].get("ownerReferences", [])):
                            return False
                        return True
                    eventually(owned_gone, timeout=90)
                    print("PASS ModelDeployment garbage collection", flush=True)
            except Exception as error:
                cleanup_errors.append(str(error))
            for fixture in reversed(fixtures):
                try:
                    remove_fixture(*fixture)
                except Exception as error:
                    cleanup_errors.append(str(error))
            if cleanup_errors:
                raise RuntimeError("Fixture cleanup needs attention: " + "; ".join(cleanup_errors))
            print("Removed only this smoke's temporary echo, gateway, EPP and Secret fixtures; existing pools and business data unchanged.", flush=True)


if __name__ == "__main__":
    main()
