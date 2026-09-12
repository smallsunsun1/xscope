"""Actual Bazel Agent -> actual control plane -> disposable Kubernetes HTTP fixture."""
import http.server
import json
import os
import subprocess
import threading
import urllib.request


def verify(api, binary, root, private_port, port, eventually):
    name = "synthetic-outbound"
    status, registration = api("POST", "/clusters", {"id": name, "namespace": "synthetic-member", "credential_days": 1}, internal=False)
    assert status == 201
    state = {"model": None, "lease": None}
    class Kubernetes(http.server.BaseHTTPRequestHandler):
        def reply(self, value, code=200):
            data = json.dumps(value).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        def do_GET(self):
            if "/leases/" in self.path:
                if state["lease"] is None:
                    self.reply({"apiVersion": "v1", "kind": "Status", "status": "Failure", "message": "synthetic missing", "reason": "NotFound", "code": 404}, 404)
                else:
                    self.reply(state["lease"])
                return
            assert self.path.startswith("/apis/platform.xscope.io/v1alpha1/namespaces/synthetic-member/modeldeployments/")
            if state["model"] is None:
                self.reply({"apiVersion": "v1", "kind": "Status", "status": "Failure", "message": "synthetic missing", "reason": "NotFound", "code": 404}, 404)
            else:
                self.reply(state["model"])
        def do_POST(self):
            value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path.split("?", 1)[0].endswith("/leases"):
                value["metadata"]["resourceVersion"] = "1"
                state["lease"] = value
                self.reply(value, 201)
                return
            assert value["metadata"]["namespace"] == "synthetic-member"
            assert value["metadata"]["labels"]["platform.xscope.io/desired-cluster"] == name
            value["metadata"]["uid"] = "synthetic-uid"
            value["metadata"]["resourceVersion"] = "1"
            state["model"] = value
            self.reply(value, 201)
        def do_PATCH(self):
            assert "/leases/" in self.path
            value = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            state["lease"]["spec"].update(value.get("spec", {}))
            self.reply(state["lease"])
        def log_message(self, *args):
            pass
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Kubernetes)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    credential = root / "member-credential"
    config = root / "member-config.json"
    kubeconfig = root / "member-kubeconfig.json"
    fixture = {"apiVersion": "v1", "kind": "Config", "current-context": "synthetic",
        "clusters": [{"name": "synthetic", "cluster": {"server": f"http://127.0.0.1:{server.server_port}"}}],
        "contexts": [{"name": "synthetic", "context": {"cluster": "synthetic", "user": "synthetic"}}],
        "users": [{"name": "synthetic", "user": {}}]}
    for path, value in ((credential, registration["credential"]), (kubeconfig, json.dumps(fixture)), (config, json.dumps({"cluster_id": name,
        "namespace": "synthetic-member", "control_url": f"http://127.0.0.1:{private_port}", "credential_file": str(credential), "allow_local_http": True}))):
        path.write_text(value)
        os.chmod(path, 0o600)
    model = {"model": {"id": "demo", "revision": "v1", "uri": "s3://example/demo", "checksum": "sha256:" + "a" * 64},
        "runtime": {"image": "example/runtime:v1", "protocol": "openai"}, "replicas": 0, "resources": {}}
    assert api("PUT", f"/clusters/{name}/desired-state", {"expected_version": 0, "deployments": [{"name": "synthetic-model", "spec": model, "delete_uid": None}]}, internal=False)[0] == 200
    address = port()
    env = dict(os.environ, XSCOPE_CLUSTER_PULL_CONFIG_FILE=str(config), KUBECONFIG=str(kubeconfig),
        XSCOPE_CLUSTER_AGENT_ADDRESS=f"127.0.0.1:{address}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}")
    try:
        with (root / "member-agent.log").open("wb") as log:
            process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
            try:
                eventually(lambda: next(row for row in api("GET", "/clusters", internal=False)[1]["data"] if row["id"] == name)["acknowledged_version"] == 1)
                assert state["model"]["spec"]["replicas"] == 0
                # Outbound mode has no second inbound CRUD writer.
                try:
                    urllib.request.urlopen(f"http://127.0.0.1:{address}/v1/model-deployments", timeout=3)
                    raise AssertionError("legacy writer exposed in outbound mode")
                except urllib.error.HTTPError as error:
                    assert error.code == 404
                print("PASS real outbound Agent: private projected config, kube-rs HTTP apply, durable ACK, no inbound mutation route; Kubernetes server is a fixture, not a second real cluster", flush=True)
            finally:
                process.kill()
                process.wait(timeout=5)
    finally:
        server.shutdown()
        server.server_close()
