"""Real TCP gateway/runtime regression: Bazel-built binaries, no live cluster."""

import http.client
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
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
        cls.gateway_port = available_port()
        cls.metrics_port = available_port()
        while cls.metrics_port == cls.gateway_port:
            cls.metrics_port = available_port()
        runtime_port = available_port()
        while runtime_port in (cls.gateway_port, cls.metrics_port):
            runtime_port = available_port()
        cls.wal = cls.root / "usage.jsonl"
        cls.runtime_log = cls.root / "runtime.log"
        resolver = runfiles.Create()
        runtime_env = dict(os.environ, XSCOPE_RUNTIME_HOST="127.0.0.1",
                           XSCOPE_RUNTIME_PORT=str(runtime_port),
                           XSCOPE_RUNTIME_STREAM_DELAY_SECONDS="0.04")
        gateway_env = dict(os.environ,
            XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{cls.gateway_port}",
            XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{cls.metrics_port}",
            XSCOPE_USAGE_WAL=str(cls.wal), XSCOPE_REDIS_URL="",
            XSCOPE_CONTROL_INTERNAL_URL="", XSCOPE_INTERNAL_TOKEN="",
            XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "pool-test", "model": "xscope-demo", "address": f"127.0.0.1:{runtime_port}"}),
            XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-test", "tenant_id": "tenant-test", "project_id": "project-test", "secret": "test-key"}]),
        )
        try:
            for name, binary, env in [("runtime", RUNTIME, runtime_env), ("gateway", GATEWAY, gateway_env)]:
                log = (cls.root / f"{name}.log").open("wb")
                cls.logs.append(log)
                cls.processes.append(subprocess.Popen([resolver.Rlocation(binary)], env=env,
                                                     stdout=log, stderr=subprocess.STDOUT))
            for port in [runtime_port, cls.gateway_port]:
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
        # Include logs in Bazel's captured output for diagnosis on test failure.
        for path in cls.root.glob("*.log"):
            print(path.name, path.read_text())
        cls.temp.cleanup()

    def request(self, request_id, *, stream=True, model="xscope-demo", text="hello model"):
        body = {"model": model, "messages": [{"role": "user", "content": text}], "stream": stream}
        connection = http.client.HTTPConnection("127.0.0.1", self.gateway_port, timeout=10)
        connection.request("POST", "/v1/chat/completions", body=json.dumps(body), headers={
            "Authorization": "Bearer test-key", "Content-Type": "application/json", "X-Request-Id": request_id,
            "traceparent": "00-0123456789abcdef0123456789abcdef-0123456789abcdef-01",
            "baggage": "private=must-not-propagate",
        })
        return connection, connection.getresponse()

    def events(self, request_id):
        if not self.wal.exists():
            return []
        result = []
        for line in self.wal.read_text().splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue  # A writer can be in the middle of its final append.
            if event["request_id"] == request_id:
                result.append(event)
        return result

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


if __name__ == "__main__":
    GATEWAY, RUNTIME = sys.argv[1:3]
    unittest.main(argv=[sys.argv[0], *sys.argv[3:]])
