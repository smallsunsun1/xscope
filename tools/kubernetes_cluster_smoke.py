"""Bazel-run local kube-rs CRUD/reconcile smoke; creates and removes one echo deployment."""
import base64
import json
import socket
import subprocess
import time
import urllib.error
import urllib.request
import uuid


def kubectl(*args):
    return subprocess.check_output(["kubectl", "--context", "docker-desktop", "-n", "xscope-system", *args], text=True)


def eventually(check, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(1)
    raise AssertionError("Kubernetes reconciliation timed out")


def main():
    context = subprocess.check_output(["kubectl", "config", "current-context"], text=True).strip()
    assert context == "docker-desktop", "This test is restricted to the local development cluster"
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    token = base64.b64decode(kubectl("get", "secret", "xscope-platform-secrets", "-o", "jsonpath={.data.internal-token}")).decode()
    forward = subprocess.Popen(["kubectl", "--context", context, "-n", "xscope-system", "port-forward", "service/cluster-agent", f"{port}:8083", "--address=127.0.0.1"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    name = "kube-rs-smoke-" + uuid.uuid4().hex[:10]
    created_uid = None

    def call(method, path, payload=None, authenticated=True):
        headers = {"Content-Type": "application/json"}
        if authenticated:
            headers["Authorization"] = "Bearer " + token
        request = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method, headers=headers,
            data=None if payload is None else json.dumps(payload).encode())
        with urllib.request.urlopen(request, timeout=10) as response:
            return response.status, json.loads(response.read() or "null")

    path = f"/v1/model-deployments/xscope-system/{name}"
    try:
        def ready():
            try:
                return call("GET", "/readyz")[0] == 200
            except (OSError, urllib.error.URLError, json.JSONDecodeError):
                return False
        # /readyz is JSON; /healthz and old/new implementations both supported.
        eventually(ready, 15)
        try:
            call("GET", "/v1/model-deployments", authenticated=False)
            raise AssertionError("unauthenticated access was accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 401
        status, model = call("POST", "/v1/model-deployments", {
            "apiVersion": "platform.xscope.io/v1alpha1", "kind": "ModelDeployment",
            "metadata": {"name": name, "namespace": "xscope-system", "labels": {"xscope.io/test": "kube-rs-smoke"}},
            "spec": {
                "model": {"id": "xscope-demo", "revision": "smoke", "uri": "echo://development", "checksum": "sha256:" + "a" * 64},
                "runtime": {"image": "xscope/runtime:dev", "protocol": "openai", "port": 8090},
                "replicas": 0,
                "resources": {"requests": {"cpu": "10m", "memory": "48Mi"}, "limits": {"cpu": "200m", "memory": "192Mi"}}
            }
        })
        assert status == 201
        created_uid = model["metadata"]["uid"]

        def reconciled(replicas):
            _, listing = call("GET", "/v1/model-deployments")
            resource = next(item for item in listing["data"] if item["metadata"]["name"] == name)
            state = resource.get("status", {})
            return state.get("observedGeneration") == resource["metadata"]["generation"] and state.get("readyReplicas", 0) == replicas and state.get("endpoint", "").endswith(":8090")
        eventually(lambda: reconciled(0))
        deployment = json.loads(kubectl("get", "deployment", name, "-o", "json"))
        child_uid = deployment["metadata"]["uid"]
        assert deployment["metadata"]["ownerReferences"][0]["uid"] == created_uid
        assert call("PUT", path + "/scale", {"replicas": 1})[0] == 200
        eventually(lambda: reconciled(1))
        deployment = json.loads(kubectl("get", "deployment", name, "-o", "json"))
        assert deployment["metadata"]["uid"] == child_uid, "scaling must not recreate the Deployment"
        assert call("PUT", path + "/scale", {"replicas": 0})[0] == 200
        eventually(lambda: reconciled(0))
        print("PASS: Rust cluster-agent authentication + create/list/scale; kube-rs Operator ownership/status/ready replicas")
    finally:
        try:
            if created_uid:
                current = json.loads(kubectl("get", "modeldeployment", name, "-o", "json"))
                assert current["metadata"]["uid"] == created_uid, "refusing to delete a replaced resource"
                assert call("DELETE", path)[0] == 204
                eventually(lambda: all(item["metadata"]["name"] != name
                    for item in json.loads(kubectl("get", "deployment,service", "-o", "json"))["items"]))
                print("PASS: delete + owner-reference garbage collection; temporary echo workload removed")
        finally:
            forward.terminate()
            forward.wait(timeout=5)


if __name__ == "__main__":
    main()
