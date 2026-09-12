"""Check real OTLP traces and Prometheus targets in the local XScope cluster."""
import http.client
import json
import os
import socket
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid


def get(url):
    with urllib.request.urlopen(url, timeout=3) as response:
        return json.load(response)


def wait(check, timeout=75):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, urllib.error.URLError):
            pass
        time.sleep(1)
    raise AssertionError("telemetry did not become queryable")


def main():
    context = subprocess.check_output(["kubectl", "config", "current-context"], text=True).strip()
    assert context == "docker-desktop", "local development cluster only"
    forwards = []
    try:
        ports = []
        for service, remote_port in [("jaeger", 16686), ("prometheus", 9090)]:
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            ports.append(port)
            forwards.append(subprocess.Popen(["kubectl", "--context", context, "-n", "xscope-system", "port-forward", f"service/{service}", f"{port}:{remote_port}", "--address=127.0.0.1"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
        jaeger = f"http://127.0.0.1:{ports[0]}"
        prometheus = f"http://127.0.0.1:{ports[1]}"
        wait(lambda: get(jaeger + "/api/services"))
        trace_id = uuid.uuid4().hex
        from observability_cluster import inference_api_key
        key = inference_api_key()
        prompt = "telemetry-sensitive-marker " + "test " * 20
        connection = http.client.HTTPConnection("127.0.0.1", 30082, timeout=10)
        try:
            connection.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "stream": True, "messages": [{"role": "user", "content": prompt}]}), {
                "Authorization": "Bearer " + key, "Content-Type": "application/json",
                "traceparent": f"00-{trace_id}-0123456789abcdef-01",
            })
            response = connection.getresponse()
            assert response.status == 200, response.status
            assert response.getheader("x-trace-id") == trace_id
            assert b"data: [DONE]" in response.read()
        finally:
            connection.close()

        expected = {"xscope-gateway", "xscope-envoy", "xscope-runtime", "xscope-control-plane"}
        def trace_complete():
            payload = get(jaeger + "/api/traces/" + trace_id)
            if not payload.get("data"):
                return None
            services = {p["serviceName"] for p in payload["data"][0]["processes"].values()}
            return payload if expected <= services else None
        payload = wait(trace_complete)
        serialized = json.dumps(payload)
        assert key not in serialized and "telemetry-sensitive-marker" not in serialized, "trace leaked credentials or prompts"
        services = {p["serviceName"] for p in payload["data"][0]["processes"].values()}
        print("PASS: trace", trace_id, "services:", ", ".join(sorted(services)))

        def targets_up():
            targets = get(prometheus + "/api/v1/targets")["data"]["activeTargets"]
            selected = [t for t in targets if t["labels"].get("job") in {"xscope", "envoy", "epp"}]
            components = {t["labels"].get("component") for t in selected}
            jobs = {t["labels"].get("job") for t in selected}
            return selected if {"gateway", "control-plane", "operator", "cluster-agent", "runtime"} <= components and {"envoy", "epp"} <= jobs and all(t["health"] == "up" for t in selected) else None
        targets = wait(targets_up)
        print("PASS: all", len(targets), "per-pod Prometheus targets are UP")
        def usage_queue_visible():
            query = urllib.parse.urlencode({"query": 'xscope_usage_queue{component="gateway"}'})
            result = get(prometheus + "/api/v1/query?" + query)["data"]["result"]
            values = {row["metric"]["kind"]: float(row["value"][1]) for row in result}
            required = {"occupied", "pending", "oldest_seconds", "capacity", "ready"}
            if not required <= values.keys():
                return None
            return values if values["capacity"] > 0 and values["occupied"] <= values["capacity"] and values["ready"] == 1 else None
        queue = wait(usage_queue_visible)
        print("PASS: Gateway HTTP usage queue metrics", json.dumps(queue, sort_keys=True))
    finally:
        for forward in forwards:
            forward.terminate()
            forward.wait(timeout=5)


if __name__ == "__main__":
    main()
