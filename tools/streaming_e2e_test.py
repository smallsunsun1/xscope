"""Real TCP gateway/runtime regression: Bazel-built binaries, no live cluster."""

import http.client
import http.server
import base64
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import threading
import unittest
import urllib.error
import urllib.request

from python.runfiles import runfiles


def available_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def eventually(check, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(0.025)
    raise AssertionError("condition did not become true before timeout")


class StreamingIntegrationTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="xscope-stream-test-")
        cls.root = Path(cls.temp.name)
        cls.processes = []
        cls.logs = []
        cls.receipts = {}
        cls.receipt_traces = {}
        cls.receipts_lock = threading.Lock()
        cls.snapshot = {"keys": [{"id": "key-test", "tenant_id": "tenant-test", "project_id": "project-test",
            "secret_hash": base64.b64encode(hashlib.sha256(b"test-key").digest()).decode(),
            "scopes": ["chat.completions"], "allowed_models": ["xscope-demo"], "rate_limit_rpm": 600,
            "monthly_budget": {"amount": 0}, "current_month_spend": {"amount": 0}}], "route_policies": []}

        class SnapshotHandler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                if self.headers.get("Authorization") != "Bearer test-internal":
                    self.send_error(401)
                    return
                body = json.dumps(cls.snapshot).encode()
                self.send_response(200)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                if self.path != "/usage-events" or self.headers.get("Authorization") != "Bearer test-internal":
                    self.send_error(403)
                    return
                event = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
                with cls.receipts_lock:
                    cls.receipts.setdefault(event["event_id"], event)
                    cls.receipt_traces[event["event_id"]] = self.headers.get("traceparent")
                self.send_response(200)
                self.end_headers()

            def log_message(self, *args):
                pass

        cls.snapshot_server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SnapshotHandler)
        cls.snapshot_thread = threading.Thread(target=cls.snapshot_server.serve_forever, daemon=True)
        cls.snapshot_thread.start()
        cls.gateway_port = available_port()
        cls.metrics_port = available_port()
        while cls.metrics_port == cls.gateway_port:
            cls.metrics_port = available_port()
        runtime_port = available_port()
        while runtime_port in (cls.gateway_port, cls.metrics_port):
            runtime_port = available_port()
        cls.runtime_log = cls.root / "runtime.log"
        cls.runtime_port = runtime_port
        canary_port = available_port()
        while canary_port in (runtime_port, cls.gateway_port, cls.metrics_port):
            canary_port = available_port()
        cls.canary_port = canary_port
        resolver = runfiles.Create()
        runtime_env = dict(os.environ, XSCOPE_RUNTIME_HOST="127.0.0.1",
                           XSCOPE_RUNTIME_PORT=str(runtime_port),
                           XSCOPE_RUNTIME_STREAM_DELAY_SECONDS="0.04")
        gateway_env = dict(os.environ,
            XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{cls.gateway_port}",
            XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{cls.metrics_port}",
            XSCOPE_USAGE_MODE="memory", XSCOPE_BILLING_RESERVATIONS="false", XSCOPE_REDIS_URL="",
            XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{cls.snapshot_server.server_port}", XSCOPE_INTERNAL_TOKEN="test-internal",
            XSCOPE_POLICY_REFRESH_SECONDS="1",
            XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "pool-test", "model": "xscope-demo", "revision": "stable-v1", "address": f"127.0.0.1:{runtime_port}"}),
            XSCOPE_ADDITIONAL_SERVING_JSON=json.dumps([{"id": "pool-canary", "model": "xscope-demo", "revision": "canary-v2", "address": f"127.0.0.1:{canary_port}"}]),
            XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-test", "tenant_id": "tenant-test", "project_id": "project-test", "secret": "test-key"}]),
        )
        try:
            for name, binary, env in [("runtime", RUNTIME, runtime_env),
                ("canary", RUNTIME, dict(runtime_env, XSCOPE_RUNTIME_PORT=str(canary_port))), ("gateway", GATEWAY, gateway_env)]:
                log = (cls.root / f"{name}.log").open("wb")
                cls.logs.append(log)
                cls.processes.append(subprocess.Popen([resolver.Rlocation(binary)], env=env,
                                                     stdout=log, stderr=subprocess.STDOUT))
            for port in [runtime_port, canary_port, cls.gateway_port]:
                def ready():
                    try:
                        with urllib.request.urlopen(f"http://127.0.0.1:{port}/readyz", timeout=0.3) as response:
                            return response.status == 200
                    except (OSError, urllib.error.URLError):
                        return False
                eventually(ready, 15)
        except Exception:
            cls.tearDownClass()
            raise

    @classmethod
    def tearDownClass(cls):
        for process in cls.processes:
            process.terminate()
        for process in cls.processes:
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)
        for log in cls.logs:
            log.close()
        cls.snapshot_server.shutdown()
        cls.snapshot_server.server_close()
        cls.snapshot_thread.join(timeout=2)
        # Include logs in Bazel's captured output for diagnosis on test failure.
        for path in cls.root.glob("*.log"):
            print(path.name, path.read_text())
        cls.temp.cleanup()

    def request(self, request_id, *, stream=True, model="xscope-demo", text="hello model", headers=None):
        body = {"model": model, "messages": [{"role": "user", "content": text}], "stream": stream}
        connection = http.client.HTTPConnection("127.0.0.1", self.gateway_port, timeout=10)
        connection.request("POST", "/v1/chat/completions", body=json.dumps(body), headers={
            "Authorization": "Bearer test-key", "Content-Type": "application/json", "X-Request-Id": request_id,
            "traceparent": "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
            "baggage": "private=must-not-propagate",
            **(headers or {}),
        })
        return connection, connection.getresponse()

    def events(self, request_id):
        with self.receipts_lock:
            return [event for event in self.receipts.values() if event["request_id"] == request_id]

    def test_complete_stream_is_incremental_and_metered_once(self):
        request_id = "complete-stream"
        text = "one two three four five six seven eight nine ten"
        started = time.monotonic()
        connection, response = self.request(request_id, text=text)
        try:
            self.assertEqual(response.status, 200)
            self.assertEqual(response.getheader("X-Trace-Id"), "0123456789abcdef0123456789abcdef")
            self.assertIn("text/event-stream", response.getheader("Content-Type"))
            first = response.readline()
            first_elapsed = time.monotonic() - started
            rest = response.read()
        finally:
            response.close()
            connection.close()
        total_elapsed = time.monotonic() - started
        self.assertGreater(total_elapsed - first_elapsed, 0.2)
        frames = [line[6:] for line in (first + rest).splitlines() if line.startswith(b"data: ")]
        self.assertEqual(frames.pop(), b"[DONE]")
        chunks = [json.loads(frame) for frame in frames]
        self.assertEqual(len([chunk for chunk in chunks if chunk.get("usage")]), 1)
        event = eventually(lambda: self.events(request_id))[0]
        self.assertEqual(event["status"], "succeeded")
        self.assertTrue(self.receipt_traces[event["event_id"]].startswith("00-0123456789abcdef0123456789abcdef-"))
        self.assertNotIn("trace_headers", event)
        self.assertEqual((event["input_tokens"], event["output_tokens"]), (10, 12))
        self.assertEqual(len(self.events(request_id)), 1)
        def metric_recorded():
            with urllib.request.urlopen(f"http://127.0.0.1:{self.metrics_port}/metrics", timeout=1) as metrics:
                return metrics.read().decode()
        metrics = eventually(lambda: (value if 'xscope_http_requests_total{' in (value := metric_recorded()) else None))
        self.assertIn('outcome="succeeded"', metrics)
        self.assertNotIn("test-key", metrics)
        self.assertNotIn(request_id, metrics)
        eventually(lambda: "trace_id=0123456789abcdef0123456789abcdef" in self.runtime_log.read_text())

    def test_disconnect_cancels_runtime_and_is_not_zero_cost_success(self):
        request_id = "cancel-stream"
        connection, response = self.request(request_id, text="word " * 100)
        self.assertEqual(response.status, 200)
        while True:
            line = response.readline()
            self.assertTrue(line, "stream ended before its first content delta")
            if line.startswith(b"data: {"):
                choices = json.loads(line[6:])["choices"]
                if choices and choices[0].get("delta", {}).get("content"):
                    break
        response.close()
        connection.close()
        eventually(lambda: f"request_id={request_id} outcome=cancelled" in self.runtime_log.read_text(), 3)
        event = eventually(lambda: self.events(request_id), 3)[0]
        self.assertEqual(event["status"], "cancelled")
        self.assertEqual(len(self.events(request_id)), 1)

    def test_forbidden_model_never_starts_inference_even_with_large_body(self):
        request_id = "forbidden-model"
        connection, response = self.request(request_id, model="forbidden", text="private " * 12000)
        try:
            self.assertEqual(response.status, 403)
            response.read()
        finally:
            response.close()
            connection.close()
        self.assertNotIn(f"inference_started request_id={request_id}", self.runtime_log.read_text())
        self.assertFalse(self.events(request_id))

    def test_non_streaming_completion_still_works(self):
        request_id = "non-stream"
        connection, response = self.request(request_id, stream=False)
        try:
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(response.read())["choices"][0]["message"]["content"], "development echo: hello model")
        finally:
            response.close()
            connection.close()
        self.assertEqual(eventually(lambda: self.events(request_id))[0]["status"], "succeeded")

    def test_multimodel_catalog_hot_reload_and_large_body(self):
        original = self.snapshot
        def entry(model, pool, version):
            return {"revision": 1, "enabled": True, "default_pool": pool, "model": {
                "id": model, "display_name": model, "max_context_tokens": 32768,
                "price_version": version, "input_per_million_tokens": {"currency": "CNY", "amount": 1000},
                "output_per_million_tokens": {"currency": "CNY", "amount": 3000}}}
        catalog = {"models": [entry("xscope-demo", "pool-test", "price-one"), entry("second-model", "second-entry", "price-two")],
            "endpoints": [{"id": "second-entry", "model": "second-model", "revision": "second-revision",
                "address": f"127.0.0.1:{self.canary_port}"}]}
        def publish(value):
            type(self).snapshot = {**original, "catalog": value, "keys": [
                {**original["keys"][0], "allowed_models": ["xscope-demo", "second-model"]}]}
        def listed():
            request = urllib.request.Request(f"http://127.0.0.1:{self.gateway_port}/v1/models",
                headers={"Authorization": "Bearer test-key"})
            with urllib.request.urlopen(request, timeout=2) as response:
                return {m["id"] for m in json.load(response)["data"]}
        def call(request_id, model, **kwargs):
            connection, response = self.request(request_id, model=model, stream=False, **kwargs)
            try:
                data = response.read()
                return response.status, response.getheader("x-xscope-pool"), data
            finally:
                response.close()
                connection.close()
        try:
            publish(catalog)
            eventually(lambda: "second-model" in listed())
            self.assertEqual(call("second-model-ok", "second-model")[:2], (200, "second-entry"))
            event = eventually(lambda: self.events("second-model-ok"))[0]
            self.assertEqual((event["model_id"], event["model_revision"], event["price_version"]),
                ("second-model", "second-revision", "price-two"))
            self.assertNotIn("request_id=second-model-ok", self.runtime_log.read_text())
            self.assertEqual(call("large-allowed", "xscope-demo", text="word " * 18000)[:2], (200, "pool-test"))
            self.assertEqual(eventually(lambda: self.events("large-allowed"))[0]["input_tokens"], 18000)
            self.assertEqual(call("oversize", "xscope-demo", text="x" * (1 << 20))[0], 413)
            self.assertFalse(self.events("oversize"))
            # Body model must not select the default model's canary policy.
            type(self).snapshot = {**self.snapshot, "route_policies": [{"tenant_id": "tenant-test", "project_id": "project-test",
                "model": "second-model", "revision": 1, "spec": {"stable_pool": "pool-test", "canary_percent": 0, "headers": []}}]}
            eventually(lambda: call("waiting-for-model-policy", "second-model")[0] == 503)
            self.assertEqual(call("wrong-model-pool", "second-model")[0], 503)
            self.assertFalse(self.events("wrong-model-pool"))
            publish({**catalog, "models": [catalog["models"][0], {**catalog["models"][1], "enabled": False, "revision": 2}]})
            eventually(lambda: "second-model" not in listed())
            self.assertEqual(call("disabled-model", "second-model")[0], 404)
        finally:
            type(self).snapshot = original
            eventually(lambda: "second-model" not in listed())

    def test_zz_default_model_outage_does_not_remove_other_models(self):
        # Last test deliberately stops the default runtime. A new independent
        # runtime proves Gateway readiness is no longer tied to that default.
        other_port = available_port()
        log = (self.root / "independent-runtime.log").open("wb")
        self.logs.append(log)
        process = subprocess.Popen([runfiles.Create().Rlocation(RUNTIME)],
            env=dict(os.environ, XSCOPE_RUNTIME_HOST="127.0.0.1", XSCOPE_RUNTIME_PORT=str(other_port)),
            stdout=log, stderr=subprocess.STDOUT)
        self.processes.append(process)
        def ready(port):
            try:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/readyz", timeout=1) as response:
                    return response.status == 200
            except (OSError, urllib.error.URLError):
                return False
        eventually(lambda: ready(other_port))
        original = self.snapshot
        definition = {"id": "independent", "display_name": "Synthetic independent model", "max_context_tokens": 4096,
            "price_version": "synthetic-price", "input_per_million_tokens": {"currency": "CNY", "amount": 1},
            "output_per_million_tokens": {"currency": "CNY", "amount": 2}}
        type(self).snapshot = {**original, "keys": [{**original["keys"][0], "allowed_models": ["independent"]}],
            "catalog": {"models": [{"revision": 1, "enabled": True, "default_pool": "independent-entry", "model": definition}],
                "endpoints": [{"id": "independent-entry", "model": "independent", "revision": "synthetic-revision", "address": f"127.0.0.1:{other_port}"}]}}
        self.processes[0].terminate()
        self.processes[0].wait(timeout=3)
        time.sleep(5.5)  # Allow the old default's background TCP check to fail.
        self.assertTrue(ready(self.gateway_port))
        connection, response = self.request("independent-after-outage", model="independent", stream=False)
        try:
            self.assertEqual(response.status, 200)
            self.assertEqual(response.getheader("x-xscope-pool"), "independent-entry")
            response.read()
        finally:
            response.close()
            connection.close()
        self.assertEqual(eventually(lambda: self.events("independent-after-outage"))[0]["model_id"], "independent")

    def test_route_snapshot_headers_weights_and_fail_closed(self):
        # These are real gateway + two Bazel runtime processes. The snapshot
        # fixture isolates dynamic policy delivery from database availability.
        policy = {"tenant_id": "tenant-test", "project_id": "project-test", "model": "xscope-demo", "revision": 1,
            "spec": {"stable_pool": "pool-test", "canary_pool": "pool-canary", "canary_percent": 0,
                "headers": [{"name": "x-route-cohort", "value": "qa", "target": "canary"}]}}
        original = self.snapshot
        counter = 0

        def publish(value):
            type(self).snapshot = {**original, "route_policies": [value]}

        def observe(revision, pool, headers=None, status=200):
            nonlocal counter
            counter += 1
            request_id = f"route-{counter}"
            connection, response = self.request(request_id, stream=True, headers=headers)
            try:
                body = response.read()
                if response.status != status:
                    return False
                if status != 200:
                    return True
                if response.getheader("x-xscope-route-revision") != str(revision):
                    return False
                self.assertEqual(response.getheader("x-xscope-pool"), pool)
                self.assertIn(b"[DONE]", body)
                event = eventually(lambda: self.events(request_id))[0]
                self.assertEqual(event["endpoint_id"], pool)
                self.assertEqual(event["model_revision"], "canary-v2" if pool == "pool-canary" else "stable-v1")
                return True
            finally:
                response.close()
                connection.close()

        try:
            publish(policy)
            eventually(lambda: observe(1, "pool-test"))
            self.assertTrue(observe(1, "pool-canary", {"X-Route-Cohort": "qa", "X-Xscope-Pool": "evil"}))
            policy = {**policy, "revision": 2, "spec": {**policy["spec"], "canary_percent": 100, "headers": []}}
            publish(policy)
            eventually(lambda: observe(2, "pool-canary"))
            # A malformed newer snapshot must leave the last good keys + routes intact.
            publish({**policy, "revision": 3, "spec": {**policy["spec"], "canary_percent": 101}})
            time.sleep(1.2)
            self.assertTrue(observe(2, "pool-canary"))
            # Another tenant's route must never attach to this principal.
            publish({**policy, "tenant_id": "other-tenant", "revision": 4})
            eventually(lambda: observe(0, "pool-test"))
            publish({**policy, "revision": 5, "spec": {**policy["spec"], "canary_pool": "missing-pool"}})
            eventually(lambda: observe(5, "missing-pool", status=503))
            publish({**policy, "revision": 6})
            eventually(lambda: observe(6, "pool-canary"))
            # A dead selected transport never spills a POST into stable.
            self.processes[1].terminate()
            self.processes[1].wait(timeout=3)
            connection, response = self.request("canary-outage", stream=False)
            try:
                self.assertIn(response.status, (502, 503))
                response.read()
            finally:
                response.close()
                connection.close()
            self.assertNotIn("request_id=canary-outage", self.runtime_log.read_text())
        finally:
            type(self).snapshot = original
            eventually(lambda: observe(0, "pool-test"))


if __name__ == "__main__":
    GATEWAY, RUNTIME = sys.argv[1:3]
    unittest.main(argv=[sys.argv[0], *sys.argv[3:]])
