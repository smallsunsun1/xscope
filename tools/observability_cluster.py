"""Shared local-only helpers for observability migration and restart verification."""
import contextlib
import base64
import json
import os
import socket
import subprocess
import time
import urllib.request

KUBE = ["kubectl", "--context", "docker-desktop", "-n", "xscope-system"]


def local_only():
    context = subprocess.check_output(["kubectl", "config", "current-context"], text=True).strip()
    if context != "docker-desktop":
        raise SystemExit("This operation is scoped to Docker Desktop; context was not changed.")


def kubectl(*args):
    return subprocess.check_output(KUBE + list(args), text=True, timeout=180)


def inference_api_key():
    explicit = os.environ.get("XSCOPE_TEST_API_KEY")
    if explicit:
        return explicit
    # Runtime-only capture: never copy key material into arguments/logs/files.
    process = subprocess.run(KUBE + ["get", "secret", "xscope-gateway-keys", "-o", "json"],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, timeout=30)
    if process.returncode:
        raise RuntimeError("Gateway key Secret unavailable; configure a runtime test key")
    keys = json.loads(base64.b64decode(json.loads(process.stdout)["data"]["keys.json"]))
    for key in keys:
        if key.get("secret") and not key.get("revoked"):
            return key["secret"]
    raise RuntimeError("No usable runtime test key in Gateway Secret")


def request(url, payload=None, headers=None, method=None):
    req = urllib.request.Request(url, data=json.dumps(payload).encode() if payload is not None else None,
                                 headers={"Content-Type": "application/json", **(headers or {})}, method=method)
    with urllib.request.urlopen(req, timeout=10) as response:
        return json.load(response)


def eventually(check, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError):
            pass
        time.sleep(1)
    raise AssertionError("Observability verification timed out")


@contextlib.contextmanager
def forward(service, remote):
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    process = subprocess.Popen(KUBE + ["port-forward", "svc/" + service, f"{port}:{remote}", "--address=127.0.0.1"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        def connected():
            if process.poll() is not None:
                raise RuntimeError("port-forward exited")
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return True
        eventually(connected)
        yield f"http://127.0.0.1:{port}"
    finally:
        process.terminate()
        process.wait(timeout=10)
