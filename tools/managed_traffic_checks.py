"""Real Agent/Gateways/Postgres; synthetic Kubernetes and streaming engine only."""
import http.client
import http.server
import json
import os
import select
import socket
import subprocess
import threading
import time
import urllib.request
import uuid
from datetime import datetime, timezone


def verify(api, sql, root, configs, token, gateway_binary, agent_binary, port, eventually, console_call, auto=False):
    cluster, namespace, deployment, serving = "synthetic-traffic", "synthetic-member", "synthetic-runtime", "synthetic-serving"
    model_id, project = "synthetic-managed-model", "synthetic-managed-project"
    suffix = "-auto" if auto else ""
    cluster, deployment, serving, model_id, project = [v + suffix for v in [cluster, deployment, serving, model_id, project]]
    model = {"id": model_id, "display_name": "Synthetic managed model", "max_context_tokens": 4096,
        "price_version": "synthetic-price", "input_per_million_tokens": {"currency": "CNY", "amount": 1},
        "output_per_million_tokens": {"currency": "CNY", "amount": 2}}
    assert api("PUT", "/model-catalog/" + model_id, {"expected_revision": 0, "enabled": True, "default_pool": None, "model": model}, internal=False)[0] == 200
    assert api("POST", "/projects", {"id": project, "tenant_id": "test-tenant", "name": "Synthetic managed fixture"}, internal=False)[0] == 201
    status, key = api("POST", "/api-keys", {"id": "synthetic-managed-key" + suffix, "project_id": project, "tenant_id": "test-tenant", "name": "Synthetic",
        "scopes": ["chat.completions"], "allowed_models": [model_id], "monthly_budget": {"currency": "CNY", "amount": 0}}, internal=False)
    assert status == 201
    status, identity = api("POST", "/clusters", {"id": cluster, "namespace": namespace, "credential_days": 1}, internal=False)
    assert status == 201
    desired_spec = {"model": {"id": model_id, "revision": "synthetic-v1", "uri": "s3://synthetic/model", "checksum": "sha256:" + "a" * 64},
        "runtime": {"image": "synthetic/runtime:test", "protocol": "openai", "port": 8000}, "replicas": 2, "resources": {"requests": {"cpu": "20m"}, "limits": {"cpu": "200m"}},
        "serving": {"endpointPickerService": serving, "endpointPickerPort": 9002}}
    desired = {"expected_version": 0, "deployments": [{"name": deployment, "spec": desired_spec, "delete_uid": None}]}
    if auto:
        desired_spec["autoscaling"] = {"managed": True, "minReplicas": 1, "maxReplicas": 3, "targetRunningRequests": 1}
    assert api("PUT", f"/clusters/{cluster}/desired-state", desired, internal=False)[0] == 200
    state = {"model": None, "lease": None, "ready": False, "calls": 0, "cancelled": 0, "recommendation": 2, "idle_enabled": False, "active": 0, "pods": {}, "headers": {}}
    streams_done = threading.Event()
    processes, logs, streams = [], [], []
    labels = {"app.kubernetes.io/name": deployment, "platform.xscope.io/deployment-uid": "synthetic-managed-uid"}

    def owner():
        return [{"apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelDeployment", "name": deployment, "uid": "synthetic-managed-uid", "controller": True}]

    class Kubernetes(http.server.BaseHTTPRequestHandler):
        def reply(self, value, code=200):
            body = json.dumps(value).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            if "/api/v1/query" in self.path:
                self.reply({"status": "success", "data": {"resultType": "vector", "result": [{"metric": {}, "value": [time.time(), "0"]}] if state["idle_enabled"] and state["active"] == 0 else []}})
                return
            if "/nodes/" in self.path:
                value = {"apiVersion": "v1", "kind": "Node", "metadata": {"name": "synthetic-node", "uid": "synthetic-node-uid"}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}}
            elif "/kube-node-lease/" in self.path:
                value = {"apiVersion": "coordination.k8s.io/v1", "kind": "Lease", "metadata": {"name": "synthetic-node", "ownerReferences": [{"apiVersion": "v1", "kind": "Node", "name": "synthetic-node", "uid": "synthetic-node-uid"}]}, "spec": {"renewTime": datetime.now(timezone.utc).isoformat()}}
            elif "/pods/" in self.path:
                entry = state["pods"].get(self.path.split("?")[0].rsplit("/", 1)[1])
                value = None if entry is None or entry.get("missing") else entry["pod"]
                if value is not None and entry["process"].poll() is not None:
                    cs = value["status"]["containerStatuses"][0]
                    cs["state"] = {"terminated": {"exitCode": 137, "containerID": cs["containerID"]}}
            elif "/modelscales/" in self.path:
                value = {"apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelScale", "metadata": {"name": deployment, "uid": "synthetic-recommendation", "resourceVersion": str(state["recommendation"]), "ownerReferences": owner()}, "spec": {"modelUid": "synthetic-managed-uid", "replicas": state["recommendation"]}}
            elif "/scaledobjects/" in self.path:
                value = {"apiVersion": "keda.sh/v1alpha1", "kind": "ScaledObject", "metadata": {"name": deployment, "uid": "synthetic-scaler", "ownerReferences": owner()}, "spec": {"scaleTargetRef": {"apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelScale", "name": deployment}, "triggers": []}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}}
            elif "/horizontalpodautoscalers/" in self.path:
                value = {"apiVersion": "autoscaling/v2", "kind": "HorizontalPodAutoscaler", "metadata": {"name": "keda-hpa-" + deployment, "ownerReferences": [{"apiVersion": "keda.sh/v1alpha1", "kind": "ScaledObject", "name": deployment, "uid": "synthetic-scaler", "controller": True}]}, "spec": {"maxReplicas": 3, "scaleTargetRef": {"apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelScale", "name": deployment}}, "status": {"currentReplicas": 2, "desiredReplicas": state["recommendation"], "conditions": [{"type": "ScalingActive", "status": "True"}, {"type": "AbleToScale", "status": "True"}]}}
            elif "/leases/" in self.path:
                value = state["lease"]
            elif "/modeldeployments/" in self.path:
                value = state["model"]
            elif "/deployments/" in self.path:
                runtime = self.path.split("?")[0].endswith("/" + deployment)
                count = state["model"]["spec"]["replicas"] if runtime else 1
                pod_labels = labels if runtime else {"synthetic-entry": serving}
                value = {"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": deployment if runtime else serving, "namespace": namespace,
                    "generation": 1, "resourceVersion": "1", "ownerReferences": owner() if runtime else []},
                    "spec": {"replicas": count, "selector": {"matchLabels": pod_labels}, "template": {"metadata": {"labels": pod_labels}, "spec": {"containers": []}}},
                    "status": {"replicas": count, "observedGeneration": 1, "updatedReplicas": count, "readyReplicas": count if state["ready"] else 0, "availableReplicas": count if state["ready"] else 0}}
            elif "/services/" in self.path:
                value = {"apiVersion": "v1", "kind": "Service", "metadata": {"name": serving, "namespace": namespace,
                    "annotations": {"platform.xscope.io/inference-pool": deployment, "platform.xscope.io/managed-only": "true", "platform.xscope.io/traffic-protocol": "v2"}}, "spec": {"selector": {"synthetic-entry": serving}, "ports": [{"port": 9002}, {"port": 8085}]}}
            elif "/inferencepools/" in self.path:
                value = {"apiVersion": "inference.networking.k8s.io/v1", "kind": "InferencePool", "metadata": {"name": deployment, "namespace": namespace, "ownerReferences": owner()},
                    "spec": {"selector": {"matchLabels": labels}, "targetPorts": [{"number": 8000}], "endpointPickerRef": {"name": serving, "port": {"number": 9002}, "failureMode": "FailClose"}}}
            else:
                raise AssertionError("unexpected synthetic Kubernetes API")
            self.reply(value if value is not None else {"apiVersion": "v1", "kind": "Status", "status": "Failure", "reason": "NotFound", "code": 404}, 200 if value is not None else 404)

        def do_POST(self):
            value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            value["metadata"]["resourceVersion"] = "1"
            if self.path.split("?")[0].endswith("/leases"):
                state["lease"] = value
            else:
                value["metadata"]["uid"] = "synthetic-managed-uid"
                value["metadata"]["generation"] = 1
                state["model"] = value
            self.reply(value, 201)

        def do_PATCH(self):
            if "/pods/" in self.path:
                patch = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                value = state["pods"][self.path.split("?")[0].rsplit("/", 1)[1]]["pod"]
                assert patch["metadata"]["uid"] == value["metadata"]["uid"]
                value["metadata"].update(patch["metadata"])
                self.reply(value)
                return
            assert "/leases/" in self.path
            patch = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            state["lease"]["spec"].update(patch.get("spec", {}))
            self.reply(state["lease"])

        def do_PUT(self):
            value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert value["metadata"]["uid"] == "synthetic-managed-uid"
            assert value["metadata"]["resourceVersion"] == state["model"]["metadata"]["resourceVersion"]
            value["metadata"]["resourceVersion"] = str(int(value["metadata"]["resourceVersion"]) + 1)
            value["metadata"]["generation"] += 1
            state["model"] = value
            self.reply(value)

        def do_DELETE(self):
            value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert value["preconditions"]["uid"] == "synthetic-managed-uid"
            assert value["preconditions"]["resourceVersion"] == state["model"]["metadata"]["resourceVersion"]
            state["model"] = None
            self.reply({"apiVersion": "v1", "kind": "Status", "status": "Success"})

        def log_message(self, *args):
            pass

    class Runtime(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"
        def handle(self):
            try:
                super().handle()
            except (ConnectionResetError, BrokenPipeError):
                pass  # Expected when the cancellation test closes a keepalive socket.
        def do_POST(self):
            body = bytearray()
            assert self.headers.get("x-xscope-managed-pool") == pool
            state["headers"][self.headers.get("x-xscope-traffic-session")] = {k: self.headers[k] for k in ["x-xscope-managed-pool", "x-xscope-traffic-session", "x-xscope-traffic-token"]}
            while True:
                size = int(self.rfile.readline().strip(), 16)
                if not size:
                    self.rfile.readline()
                    break
                body.extend(self.rfile.read(size))
                self.rfile.read(2)
            assert json.loads(body)["model"] == model_id
            state["calls"] += 1
            state["active"] += 1
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            def chunk(data):
                self.wfile.write(f"{len(data):x}\r\n".encode() + data + b"\r\n")
                self.wfile.flush()
            try:
                chunk(b'data: {"choices":[{"delta":{"content":"synthetic"}}]}\n\n')
                deadline = time.monotonic() + 60
                while not streams_done.wait(.05):
                    if time.monotonic() >= deadline:
                        raise AssertionError("synthetic stream timed out")
                    if select.select([self.connection], [], [], 0)[0] and not self.connection.recv(1, socket.MSG_PEEK):
                        state["cancelled"] += 1
                        return
                chunk(b'data: {"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2}}\n\ndata: [DONE]\n\n')
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                state["cancelled"] += 1
            finally:
                state["active"] -= 1
        def log_message(self, *args):
            pass

    kube = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Kubernetes)
    runtime = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Runtime)
    for server in [kube, runtime]:
        threading.Thread(target=server.serve_forever, daemon=True).start()
    private = root / ("managed-traffic" + suffix)
    private.mkdir(mode=0o700)
    def file(name, value):
        path = private / name
        path.write_text(value if isinstance(value, str) else json.dumps(value))
        os.chmod(path, 0o600)
        return str(path)
    credential = file("credential", identity["credential"])
    kubeconfig = file("kubeconfig.json", {"apiVersion": "v1", "kind": "Config", "current-context": "synthetic",
        "clusters": [{"name": "synthetic", "cluster": {"server": f"http://127.0.0.1:{kube.server_port}"}}],
        "contexts": [{"name": "synthetic", "context": {"cluster": "synthetic", "user": "synthetic"}}], "users": [{"name": "synthetic", "user": {}}]})
    config = file("config.json", {"cluster_id": cluster, "namespace": namespace, "control_url": f"http://127.0.0.1:{configs[0][1]}", "credential_file": credential, "allow_local_http": True, "prometheus_url": f"http://127.0.0.1:{kube.server_port}"})
    def launch(binary, env, name):
        log = (private / (name + ".log")).open("wb")
        logs.append(log)
        process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
        processes.append(process)
        return process
    def pool_status():
        return api("GET", f"/managed-pools/{pool}", internal=False)[1]
    def member(method, payload=None):
        return api(method, f"/clusters/{cluster}/observations", payload, credential=identity["credential"])
    def start_stream(address):
        connection = http.client.HTTPConnection("127.0.0.1", address, timeout=20)
        connection.request("POST", "/v1/chat/completions", json.dumps({"model": model_id, "messages": [{"role": "user", "content": "synthetic"}], "stream": True}),
            {"Authorization": "Bearer " + key["secret"], "Content-Type": "application/json"})
        response = connection.getresponse()
        return connection, response
    def snap(session):
        result = api("GET", "/gateway/snapshot?session=" + session)
        assert result[0] == 200
        return result[1]["traffic"]
    def ack(grant, counts=None, closed=False):
        return api("POST", "/gateway/traffic-report", {"session_id": grant["session_id"], "sequence": grant["sequence"], "nonce": grant["nonce"],
            "closed": closed, "active": {p["id"]: (counts or {}).get(p["id"], 0) for p in grant["pools"]}})
    try:
        binding = {"cluster_id": cluster, "deployment": deployment, "serving_service": serving, "expected_desired_version": 1,
            "address": f"127.0.0.1:{runtime.server_port}", "make_default": True, "traffic_protocol": 2}
        assert console_call("POST", "/managed-pools", binding, "console-smoke-member")[0] == 403
        status, bound = api("POST", "/managed-pools", binding, internal=False)
        assert status == 200, bound
        pool = bound["id"]
        assert api("PUT", "/serving-endpoints/synthetic-alias", {"expected_generation": 0, "endpoint": {
            "id": "synthetic-alias", "model": model_id, "revision": "synthetic-v1", "address": binding["address"]}}, internal=False)[0] == 409
        launch(agent_binary, dict(os.environ, KUBECONFIG=kubeconfig, XSCOPE_CLUSTER_PULL_CONFIG_FILE=config,
            XSCOPE_CLUSTER_AGENT_ADDRESS=f"127.0.0.1:{port()}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}"), "agent")
        eventually(lambda: next(c for c in api("GET", "/clusters", internal=False)[1]["data"] if c["id"] == cluster)["acknowledged_version"] == 1)
        eventually(lambda: pool_status()["observed_at"] is not None)
        assert pool_status()["state"] == "pending"  # ACK != model/serving readiness.
        assert pool_status()["deployment_uid"] == "synthetic-managed-uid"
        state["ready"] = True
        eventually(lambda: pool_status()["state"] == "active")
        assert pool_status()["deployment_uid"] == "synthetic-managed-uid"
        assert not any(e["id"] == pool for e in api("GET", "/gateway/snapshot")[1]["catalog"]["endpoints"])
        assert next(m for m in api("GET", "/model-catalog", internal=False)[1]["data"] if m["model"]["id"] == model_id)["default_pool"] == pool
        # A stale observation nonce is fenced, not a reason to reset readiness.
        tasks1, tasks2 = member("GET")[1]["tasks"], member("GET")[1]["tasks"]
        stale = {k: tasks1[0][k] for k in ["pool_id", "generation", "nonce"]}
        assert member("POST", {**stale, "deployment_uid": "synthetic-managed-uid", "ready": True, "code": "ok"})[0] == 409
        current = {k: tasks2[0][k] for k in ["pool_id", "generation", "nonce"]}
        assert member("POST", {**current, "deployment_uid": "different-uid", "ready": True, "code": "ok"})[0] == 409
        assert member("POST", {**current, "deployment_uid": "synthetic-managed-uid", "ready": True, "code": "ok"})[0] == 200
        addresses = []
        for index in range(2):
            address = port()
            addresses.append(address)
            env = dict(os.environ, XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{address}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}",
                XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{configs[index][1]}/internal/v1", XSCOPE_INTERNAL_TOKEN=token,
                XSCOPE_POLICY_REFRESH_SECONDS="1", XSCOPE_BILLING_RESERVATIONS="false", XSCOPE_REDIS_URL="", XSCOPE_USAGE_MODE="memory",
                XSCOPE_API_KEYS_JSON="[]", XSCOPE_ADDITIONAL_SERVING_JSON="[]", XSCOPE_GATEWAY_GRACE_SECONDS="1",
                XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "synthetic-legacy", "model": "xscope-demo", "revision": "synthetic", "address": f"127.0.0.1:{runtime.server_port}"}))
            if auto:
                pod_name = f"synthetic-gateway-{index}"
                pod_uid = f"synthetic-gateway-uid-{index}"
                env.update(XSCOPE_GATEWAY_CLUSTER_ID=cluster, XSCOPE_POD_NAMESPACE=namespace, XSCOPE_POD_NAME=pod_name, XSCOPE_POD_UID=pod_uid)
            process = launch(gateway_binary, env, f"gateway-{index}")
            if auto:
                state["pods"][pod_name] = {"process": process, "pod": {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": pod_name, "namespace": namespace, "uid": pod_uid, "resourceVersion": "1", "labels": {"app.kubernetes.io/name": "gateway"}}, "spec": {"nodeName": "synthetic-node", "containers": [{"name": "gateway", "ports": [{"name": "http", "containerPort": address}]}]}, "status": {"podIP": "127.0.0.1", "containerStatuses": [{"name": "gateway", "image": "synthetic/gateway", "imageID": "synthetic", "containerID": "containerd://synthetic-" + str(index), "ready": True, "restartCount": 0, "state": {"running": {}}}]}}}
        def gateway_ready(address):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{address}/readyz", timeout=1) as response:
                    return response.status == 200
            except (OSError, urllib.error.URLError):
                return False
        for address in addresses:
            eventually(lambda: gateway_ready(address))
        eventually(lambda: len(pool_status()["participants"]) >= 2)
        for address in addresses:
            connection, response = start_stream(address)
            assert response.status == 200, response.status
            assert response.readline().startswith(b"data: ")
            streams.append((connection, response))
        eventually(lambda: sum(p["active_requests"] for p in pool_status()["participants"]) == 2)
        own_sessions = {p["session_id"] for p in pool_status()["participants"] if p["active_requests"] > 0}
        if auto:
            state["recommendation"] = 1
            eventually(lambda: pool_status()["state"] == "draining")
            assert state["model"]["spec"]["replicas"] == 2
            processes[1].kill()
            processes[1].wait(timeout=5)
            streams[0][1].close()
            streams[0][0].close()
            eventually(lambda: any(p["retired"] and p["active_requests"] == -1 for p in pool_status()["participants"]))
            dead = next(p["session_id"] for p in pool_status()["participants"] if p["active_requests"] == -1)
            rejected = urllib.request.Request(f"http://127.0.0.1:{configs[0][1]}/internal/v1/traffic/authorize", headers=state["headers"][dead])
            try:
                urllib.request.urlopen(rejected, timeout=3)
                raise AssertionError("retired gateway capability accepted")
            except urllib.error.HTTPError as error:
                assert error.code == 403
            streams_done.set()
            assert b"[DONE]" in streams[1][1].read()
            streams[1][1].close()
            streams[1][0].close()
            time.sleep(2)
            assert state["model"]["spec"]["replicas"] == 2  # Missing idle evidence is not zero.
            state["idle_enabled"] = True
            eventually(lambda: state["model"]["spec"]["replicas"] == 1)
            eventually(lambda: pool_status()["state"] == "active")
            state["recommendation"] = 2
            eventually(lambda: state["model"]["spec"]["replicas"] == 2)
            eventually(lambda: pool_status()["state"] == "active")
            print("PASS automatic KEDA recommendations: drain before shrink, real Gateway process kill + kubelet fixture proof, orphan work stays unknown until post-fence idle, retired capability denied, 2 -> 1 -> 2 and automatic reopening", flush=True)
            return
        # Model revision traffic has a real per-Gateway version ACK, not just a saved DB row.
        route = f"/projects/{project}/models/{model_id}/route-policy"
        assert api("PUT", route, {"expected_revision": 0, "spec": {"stable_pool": pool, "canary_percent": 0, "headers": []}}, internal=False)[0] == 200
        eventually(lambda: api("GET", route + "/acks", internal=False)[1]["all_known_acknowledged"])
        reduced = {"expected_version": 1, "deployments": [{"name": deployment, "spec": {**desired_spec, "replicas": 1}, "delete_uid": None}]}
        assert api("PUT", f"/clusters/{cluster}/desired-state", reduced, internal=False)[0] == 409
        lost_session = str(uuid.uuid4())
        lost = snap(lost_session)  # Simulated lost GET response still creates a participant.
        generation = pool_status()["generation"]
        assert api("POST", f"/managed-pools/{pool}/drain", {"expected_generation": generation}, internal=False)[0] == 200
        generation += 1
        def denied():
            c, r = start_stream(addresses[0])
            try:
                if r.status == 200:
                    r.readline()
                    return False
                r.read()
                return r.status == 503
            finally:
                r.close()
                c.close()
        eventually(denied)
        assert api("POST", f"/managed-pools/{pool}/finish-drain", {"expected_generation": generation}, internal=False)[0] == 409
        streams[0][1].close()
        streams[0][0].close()
        eventually(lambda: state["cancelled"] > 0)
        streams_done.set()
        assert b"[DONE]" in streams[1][1].read()
        streams[1][1].close()
        streams[1][0].close()
        eventually(lambda: sum(p["active_requests"] for p in pool_status()["participants"]) == 0)
        assert api("POST", f"/managed-pools/{pool}/finish-drain", {"expected_generation": generation}, internal=False)[0] == 409
        # Neither an ancient ACK nor expiring a participant timestamp is proof of drain.
        assert ack(lost)[0] == 200
        sql("UPDATE xscope.gateway_sessions SET delivered_at=now()-interval '1 day' WHERE id='" + lost_session + "'")
        assert api("POST", f"/managed-pools/{pool}/finish-drain", {"expected_generation": generation}, internal=False)[0] == 409
        retired = snap(lost_session)
        assert ack(retired, closed=True)[0] == 200
        assert ack(retired, closed=True)[0] == 200
        assert ack(retired)[0] == 409
        assert api("GET", "/gateway/snapshot?session=" + lost_session)[0] == 409
        eventually(lambda: pool_status()["can_finish_drain"])
        assert api("POST", f"/managed-pools/{pool}/finish-drain", {"expected_generation": generation}, internal=False)[0] == 200
        assert api("PUT", f"/clusters/{cluster}/desired-state", reduced, internal=False)[0] == 200
        eventually(lambda: state["model"]["spec"]["replicas"] == 1)
        assert pool_status()["state"] == "drained"  # Observation must not silently reopen traffic.
        eventually(lambda: pool_status()["ready_until"] is not None)
        generation = pool_status()["generation"]
        assert api("POST", f"/managed-pools/{pool}/activate", {"expected_generation": generation}, internal=False)[0] == 200
        # Graceful close retires the incarnation; a future drain need not wait on a dead process.
        for process in processes[1:]:
            process.terminate()
            process.wait(timeout=20)
        eventually(lambda: all(p["retired"] for p in pool_status()["participants"] if p["session_id"] in own_sessions))
        assert state["model"]["metadata"]["uid"] == "synthetic-managed-uid"
        generation = pool_status()["generation"]
        assert api("POST", f"/managed-pools/{pool}/drain", {"expected_generation": generation}, internal=False)[0] == 200
        assert api("POST", f"/managed-pools/{pool}/finish-drain", {"expected_generation": generation + 1}, internal=False)[0] == 200
        deletion = {"expected_version": 2, "deployments": [{"name": deployment, "spec": None, "delete_uid": "wrong-uid"}]}
        assert api("PUT", f"/clusters/{cluster}/desired-state", deletion, internal=False)[0] == 409
        deletion["deployments"][0]["delete_uid"] = "synthetic-managed-uid"
        assert api("PUT", f"/clusters/{cluster}/desired-state", deletion, internal=False)[0] == 200
        eventually(lambda: state["model"] is None)
        assert pool_status()["state"] == "retired"
        assert api("PUT", f"/clusters/{cluster}/desired-state", {**reduced, "expected_version": 3}, internal=False)[0] == 409
        print("PASS real Agent readiness -> automatic managed registration -> two Gateways: SSE counts, policy ACK, lost snapshot/offline fencing, drain, UID-bound scale and graceful incarnation retirement; Kubernetes/Runtime are synthetic fixtures", flush=True)
    finally:
        streams_done.set()
        for connection, response in streams:
            response.close()
            connection.close()
        for process in processes:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
        for log in logs:
            log.close()
        for server in [kube, runtime]:
            server.shutdown()
            server.server_close()
