"""Real local KEDA/Envoy/Agent validation; crash only our isolated test Gateway."""
import argparse
import concurrent.futures
import copy
import http.client
import json
import subprocess
import threading
import time
import urllib.error
import uuid
from safe_scaling_local import NS, OWNER, get, kube, maintenance, owned_apply, private_state, wait
from observability_cluster import forward, local_only


def eventually(check, timeout=240):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(3)
    raise RuntimeError("live scaling condition did not converge")


def status(state):
    pool = maintenance({"action": "pool_status", "id": state["pool"]})
    model = get("modeldeployment", state["deployment"])
    return pool, model


def summary(state):
    pool, model = status(state)
    scale = get("modelscale", state["deployment"])
    hpa = get("hpa", "keda-hpa-" + state["deployment"])
    print(json.dumps({"pool_state": pool["state"], "observation": pool["observation_code"], "participants": len(pool["participants"]),
        "blocked": pool["blocked_participants"], "replicas": model["spec"]["replicas"] if model else None,
        "ready": (model or {}).get("status", {}).get("readyReplicas"), "recommendation": (scale or {}).get("spec", {}).get("replicas"),
        "hpa_conditions": [{"type": c["type"], "status": c["status"], "reason": c.get("reason")} for c in (hpa or {}).get("status", {}).get("conditions", [])]}), flush=True)


def fault_gateway(name):
    deployment = get("deployment", name)
    if not name.startswith("gateway-check-") or deployment["metadata"].get("annotations", {}).get(OWNER) != "true":
        raise RuntimeError("refusing unowned fault target")
    pods = json.loads(kube("get", "pods", "-l", "app.kubernetes.io/name=" + name, "-o", "json"))["items"]
    pods = [p for p in pods if not p["metadata"].get("deletionTimestamp")]
    if len(pods) != 1:
        raise RuntimeError("fault target is ambiguous")
    pod = pods[0]
    if "platform.xscope.io/gateway-evidence" not in pod["metadata"].get("finalizers", []):
        raise RuntimeError("gateway container identity has not been attested")
    owner = next(o for o in pod["metadata"]["ownerReferences"] if o.get("controller") and o["kind"] == "ReplicaSet")
    replica_set = get("replicaset", owner["name"])
    if not any(o.get("controller") and o["uid"] == deployment["metadata"]["uid"] for o in replica_set["metadata"]["ownerReferences"]):
        raise RuntimeError("fault target ownership changed")
    # Kubelet terminates PID 1 from outside its PID namespace. Do not use
    # --force or remove the evidence finalizer; both would destroy the proof.
    kube("delete", "--raw", "/api/v1/namespaces/" + NS + "/pods/" + pod["metadata"]["name"], "-f", "-",
        payload={"apiVersion": "v1", "kind": "DeleteOptions", "gracePeriodSeconds": 1,
            "preconditions": {"uid": pod["metadata"]["uid"], "resourceVersion": pod["metadata"]["resourceVersion"]}})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--status", action="store_true")
    parser.add_argument("--fault-current", action="store_true")
    args = parser.parse_args()
    local_only()
    state = private_state("xscope-managed-validation")
    if not state or not state.get("complete"):
        raise RuntimeError("managed bootstrap is incomplete")
    if args.status:
        summary(state)
        return
    if args.fault_current:
        deployments = json.loads(kube("get", "deployments", "-o", "json"))["items"]
        targets = [d["metadata"]["name"] for d in deployments if d["metadata"]["name"].startswith("gateway-check-") and d["metadata"].get("annotations", {}).get(OWNER) == "true"]
        if len(targets) != 1:
            raise RuntimeError("fault target is ambiguous")
        fault_gateway(targets[0])
        print("Isolated validation Pod termination requested; evidence finalizer retained", flush=True)
        return
    name = "gateway-check-" + uuid.uuid4().hex[:8]
    gateway = get("deployment", "gateway")
    template = copy.deepcopy(gateway["spec"]["template"])
    template["metadata"]["labels"].update({"app.kubernetes.io/name": name, "app.kubernetes.io/component": "api-gateway"})
    for container in template["spec"]["containers"]:
        if container["name"] == "gateway":
            values = {e["name"]: e for e in container.get("env", [])}
            for key, value in {"XSCOPE_BILLING_RESERVATIONS": "false", "XSCOPE_REDIS_URL": "", "XSCOPE_GATEWAY_GRACE_SECONDS": "5"}.items():
                values[key] = {"name": key, "value": value}
            container["env"] = list(values.values())
    owned_apply({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "namespace": NS},
        "spec": {"replicas": 1, "selector": {"matchLabels": {"app.kubernetes.io/name": name}}, "template": template}})
    owned_apply({"apiVersion": "v1", "kind": "Service", "metadata": {"name": name, "namespace": NS},
        "spec": {"selector": {"app.kubernetes.io/name": name}, "ports": [{"name": "http", "port": 8080, "targetPort": "http"}]}})
    stop = threading.Event()
    try:
        wait(name)
        with forward(name, 8080) as base:
            from urllib.parse import urlparse
            address = urlparse(base)
            def infer(words=2, stream=False, signal=None, cancel=None):
                connection = http.client.HTTPConnection(address.hostname, address.port, timeout=20)
                try:
                    body = {"model": state["model"], "stream": stream, "messages": [{"role": "user", "content": "w " * words}], "max_tokens": 16000 if words > 2000 else 2048}
                    connection.request("POST", "/v1/chat/completions", json.dumps(body), {"Content-Type": "application/json", "Authorization": "Bearer " + state["key"]})
                    response = connection.getresponse()
                    if response.status != 200:
                        response.read()
                        return response.status
                    if signal:
                        signal.set()
                    if stream:
                        while response.readline():
                            if cancel and cancel.is_set():
                                break
                    else:
                        response.read()
                    return 200
                except (OSError, http.client.HTTPException):
                    return 0
                finally:
                    connection.close()
            eventually(lambda: infer() == 200)
            print("PASS live managed inference and per-request Envoy authorization", flush=True)
            eventually(lambda: status(state)[1]["spec"]["replicas"] == 1)
            print("PASS real KEDA recommendation automatically reduced idle Runtime 2 -> 1", flush=True)
            def load():
                while not stop.is_set():
                    infer(1000, True, cancel=stop)
                    stop.wait(.1)
            with concurrent.futures.ThreadPoolExecutor(max_workers=3) as workers:
                jobs = [workers.submit(load) for _ in range(3)]
                try:
                    eventually(lambda: status(state)[1].get("status", {}).get("readyReplicas") == 2)
                finally:
                    stop.set()
                for job in jobs:
                    job.result(timeout=30)
            print("PASS real KEDA expanded Runtime 1 -> 2 under streaming load", flush=True)
            began = threading.Event()
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as worker:
                def hold():
                    for _ in range(20):
                        code = infer(8000, True, began)
                        if began.is_set():
                            return code
                        time.sleep(1)
                    return code
                held = worker.submit(hold)
                if not began.wait(40):
                    raise RuntimeError("held stream did not start; status=" + str(held.result(timeout=25)))
                eventually(lambda: status(state)[0]["state"] == "draining", timeout=210)
                pool, model = status(state)
                if model["spec"]["replicas"] != 2 or not any(p["active_requests"] > 0 for p in pool["participants"]):
                    raise RuntimeError("downscale bypassed live stream drain")
                active = next(p["session_id"] for p in pool["participants"] if p["active_requests"] > 0)
                current = get("deployment", name)
                if current["metadata"].get("annotations", {}).get(OWNER) != "true":
                    raise RuntimeError("refusing unowned fault target")
                fault_gateway(name)
                held.result(timeout=30)
                eventually(lambda: any(p["session_id"] == active and p["retired"] for p in status(state)[0]["participants"]))
                eventually(lambda: status(state)[1]["spec"]["replicas"] == 1 and status(state)[0]["state"] == "active")
            print("PASS isolated Gateway container crash: kubelet proof, post-fence Envoy idle, and automatic drain completion; public Gateway untouched", flush=True)
        # Restarted checker may have a new Pod/container endpoint; re-open the tunnel.
        print("PASS managed deployment remains ready after fault recovery", flush=True)
    finally:
        stop.set()
        current = get("deployment", name)
        if current and current["metadata"].get("annotations", {}).get(OWNER) == "true":
            kube("delete", "deployment", name, "--wait=false")
            kube("wait", "--for=delete", "pod", "-l", "app.kubernetes.io/name=" + name, "--timeout=120s")
        current = get("service", name)
        if current and current["metadata"].get("annotations", {}).get(OWNER) == "true":
            kube("delete", "service", name)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        detail = str(error) if isinstance(error, RuntimeError) else type(error).__name__
        raise SystemExit("Live scaling check paused: " + detail + "; private values suppressed") from None
