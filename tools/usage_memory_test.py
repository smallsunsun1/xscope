"""Real Bazel Pingora process, volatile outbox and fault-injected HTTP peers.

Synthetic local fixtures only. Deliberately verifies the loss boundary, not a
claim of exactly-once delivery or crash durability.
"""
import collections
import concurrent.futures
import http.client
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from python.runfiles import runfiles


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def eventually(check, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError("volatile usage fixture timed out")


class MemoryUsageTest(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="xscope-volatile-test-")
        self.root = Path(self.directory.name)
        self.rows, self.settled, self.unresolved = {}, {}, {}
        self.attempts = collections.Counter()
        self.calls = 0
        self.failure = 0
        self.drop_ack = False
        self.drop_admission_ack = None
        self.cancel_stream = False
        self.runtime_cancelled = threading.Event()
        self.unknown = False
        self.over_limit = False
        self.gate = threading.Event()
        self.gate.set()
        self.lock = threading.Lock()
        fixture = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def respond(self, body, status=200, stream=False):
                raw = body if isinstance(body, bytes) else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "text/event-stream" if stream else "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                try:
                    self.wfile.write(raw)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def do_GET(self):
                if "/reservations/" in self.path:
                    with fixture.lock:
                        self.respond(fixture.rows[self.path.rsplit("/", 1)[1]])
                else:
                    self.respond({}, 503)  # retain the static synthetic key

            def do_POST(self):
                raw = bytearray()
                if self.headers.get("Transfer-Encoding") == "chunked":
                    while True:
                        line = self.rfile.readline().strip()
                        if not line:
                            return
                        size = int(line, 16)
                        if not size:
                            self.rfile.readline()
                            break
                        raw.extend(self.rfile.read(size))
                        self.rfile.read(2)
                else:
                    raw.extend(self.rfile.read(int(self.headers.get("Content-Length", 0))))
                body = json.loads(raw or "{}")
                action = self.path.rsplit("/", 1)[1]
                if action == "completions":
                    with fixture.lock:
                        fixture.calls += 1
                    fixture.gate.wait(15)
                    if body.get("stream") and fixture.cancel_stream:
                        self.send_response(200)
                        self.send_header("Content-Type", "text/event-stream")
                        self.end_headers()
                        self.wfile.write(b'data: {"choices":[{"delta":{"content":"fixture"}}]}\n\n')
                        self.wfile.flush()
                        self.connection.settimeout(5)
                        try:
                            if self.connection.recv(1) == b"":
                                fixture.runtime_cancelled.set()
                        except OSError:
                            pass
                        return
                    result = {"choices": []}
                    if not fixture.unknown:
                        result["usage"] = {"prompt_tokens": 1, "completion_tokens": 5 if fixture.over_limit else 2}
                    if body.get("stream"):
                        self.respond(b"data: " + json.dumps(result).encode() + b"\n\ndata: [DONE]\n\n", stream=True)
                    else:
                        self.respond(result)
                    return
                assert self.headers.get("Authorization") == "Bearer test-internal"
                identity = body["id"] if action == "reservations" else self.path.split("/")[-2]
                drop = False
                with fixture.lock:
                    fixture.attempts[(identity, action)] += 1
                    if action in ("settle", "unresolved") and fixture.failure:
                        self.respond({}, fixture.failure)
                        return
                    if action == "reservations":
                        fixture.rows.setdefault(identity, {"state": "reserved", "spec": body})
                    elif action == "dispatch":
                        assert fixture.rows[identity]["state"] == "reserved"
                        fixture.rows[identity]["state"] = "dispatched"
                    elif action == "release":
                        assert fixture.rows[identity]["state"] == "reserved"
                        fixture.rows[identity]["state"] = "released"
                    elif action == "settle":
                        assert identity not in fixture.settled or fixture.settled[identity] == body
                        fixture.settled[identity] = body
                        fixture.rows[identity]["state"] = "settled"
                        drop, fixture.drop_ack = fixture.drop_ack, False
                    elif action == "unresolved":
                        if fixture.over_limit:
                            assert body["reason"] == "usage_over_limit"
                            assert (body["input_tokens"], body["output_tokens"]) == (1, 5)
                        else:
                            assert body["reason"] == "usage_missing"
                            assert body["input_tokens"] is None and body["output_tokens"] is None
                        assert identity not in fixture.unresolved or fixture.unresolved[identity] == body
                        fixture.unresolved[identity] = body
                    else:
                        raise AssertionError(action)
                    result = dict(fixture.rows[identity])
                    if action == fixture.drop_admission_ack:
                        fixture.drop_admission_ack = None
                        drop = True
                if drop:
                    self.connection.shutdown(socket.SHUT_RDWR)
                    self.connection.close()
                else:
                    self.respond(result)

            def log_message(self, *args):
                pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.gateway_port, self.metrics_port = port(), port()
        self.env = dict(os.environ, XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{self.gateway_port}",
            XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{self.metrics_port}",
            XSCOPE_USAGE_WAL=str(self.root / "never-created" / "events.jsonl"),
            XSCOPE_INTERNAL_TOKEN="test-internal", XSCOPE_REDIS_URL="", XSCOPE_BILLING_RESERVATIONS="true",
            XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{self.server.server_port}/internal/v1",
            XSCOPE_MODEL_CONTEXT_TOKENS="128", XSCOPE_USAGE_QUEUE_CAPACITY="4", XSCOPE_USAGE_REPORT_WORKERS="1",
            XSCOPE_USAGE_MAX_PENDING_SECONDS="1", XSCOPE_USAGE_DRAIN_SECONDS="2", XSCOPE_GATEWAY_GRACE_SECONDS="1",
            XSCOPE_ADDITIONAL_SERVING_JSON="[]", XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "test-pool", "model": "xscope-demo",
                "revision": "development", "address": f"127.0.0.1:{self.server.server_port}"}),
            XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-test", "tenant_id": "test-tenant", "project_id": "test-project", "secret": "test-key"}]))
        self.env.pop("XSCOPE_USAGE_MODE", None)  # verify the NEW default
        self.logs, self.process = [], None

    def start(self):
        log = (self.root / f"gateway-{len(self.logs)}.log").open("wb")
        self.logs.append(log)
        self.process = subprocess.Popen([GATEWAY], env=self.env, stdout=log, stderr=log)
        eventually(lambda: self.ready() == 200)

    def ready(self):
        try:
            c = http.client.HTTPConnection("127.0.0.1", self.gateway_port, timeout=1)
            c.request("GET", "/readyz")
            r = c.getresponse()
            r.read()
            c.close()
            return r.status
        except OSError:
            return 0

    def infer(self, stream=False):
        c = http.client.HTTPConnection("127.0.0.1", self.gateway_port, timeout=20)
        try:
            c.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "messages": [], "max_tokens": 4, "stream": stream}),
                {"Authorization": "Bearer test-key", "Content-Type": "application/json"})
            r = c.getresponse()
            return r.status, r.getheader("x-xscope-billing-request-id"), r.read()
        finally:
            c.close()

    def metrics(self):
        c = http.client.HTTPConnection("127.0.0.1", self.metrics_port, timeout=1)
        c.request("GET", "/metrics")
        raw = c.getresponse().read().decode()
        c.close()
        return raw

    def tearDown(self):
        self.gate.set()
        if self.process and self.process.poll() is None:
            self.process.kill()
            self.process.wait(timeout=5)
        self.server.shutdown()
        self.server.server_close()
        for log in self.logs:
            log.close()
        self.directory.cleanup()

    def test_removed_mode_and_missing_reporter_fail_startup(self):
        for config, message in [
            ({"XSCOPE_USAGE_MODE": "wal"}, "WAL mode was removed"),
            ({"XSCOPE_CONTROL_INTERNAL_URL": ""}, "HTTP usage reporting requires"),
            ({"XSCOPE_INTERNAL_TOKEN": ""}, "HTTP usage reporting requires"),
        ]:
            with self.subTest(config=list(config)):
                result = subprocess.run([GATEWAY], env={**self.env, **config},
                    stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=10, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stdout)
        self.assertEqual(self.calls, 0)
        self.assertFalse((self.root / "never-created").exists())

    def test_over_limit_reports_evidence_without_settlement(self):
        self.over_limit = True
        self.start()
        status, identity, _ = self.infer()
        self.assertEqual(status, 200)
        eventually(lambda: identity in self.unresolved)
        self.assertEqual(self.rows[identity]["state"], "dispatched")
        self.assertNotIn(identity, self.settled)
        self.assertEqual(self.calls, 1)

    def test_http_ack_loss_stream_and_unknown(self):
        self.start()
        self.drop_ack = True
        status, identity, _ = self.infer(stream=True)
        self.assertEqual(status, 200)
        eventually(lambda: self.attempts[(identity, "settle")] >= 2)
        eventually(lambda: self.ready() == 200)
        self.assertEqual(self.settled[identity]["output_tokens"], 2)
        self.assertEqual(self.calls, 1)  # retry usage, NEVER inference
        self.unknown = True
        status, unknown, _ = self.infer()
        self.assertEqual(status, 200)
        eventually(lambda: unknown in self.unresolved)
        self.assertEqual(self.rows[unknown]["state"], "dispatched")
        self.assertNotIn(unknown, self.settled)
        self.assertFalse((self.root / "never-created").exists())

    def test_age_backpressure_retry_and_permanent_record(self):
        self.start()
        self.failure = 503
        status, identity, _ = self.infer()
        self.assertEqual(status, 200)
        eventually(lambda: self.ready() == 503)
        self.assertEqual(self.infer()[0], 503)
        self.assertEqual(self.calls, 1)
        self.failure = 0
        eventually(lambda: identity in self.settled and self.ready() == 200)
        self.failure = 400
        _, bad, _ = self.infer()
        eventually(lambda: self.attempts[(bad, "settle")] == 1 and self.ready() == 200)
        self.failure = 0
        _, good, _ = self.infer()
        eventually(lambda: good in self.settled)
        self.assertEqual(self.attempts[(bad, "settle")], 1)
        self.assertIn('operation="usage_export",outcome="permanent_failure"', self.metrics())

    def test_capacity_includes_inflight_and_credentials_fail_closed(self):
        self.env.update(XSCOPE_USAGE_QUEUE_CAPACITY="2", XSCOPE_USAGE_MAX_PENDING_SECONDS="30")
        self.start()
        self.gate.clear()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            work = [pool.submit(self.infer) for _ in range(2)]
            eventually(lambda: self.calls == 2)
            self.assertEqual(self.infer()[0], 503)
            self.gate.set()
            self.assertTrue(all(f.result()[0] == 200 for f in work))
        eventually(lambda: len(self.settled) == 2 and self.ready() == 200)
        self.failure = 401
        _, identity, _ = self.infer()
        eventually(lambda: self.attempts[(identity, "settle")] == 1 and self.ready() == 503)
        self.failure = 0
        self.assertEqual(self.infer()[0], 503)  # explicit operator repair/restart

    def test_crash_loses_queue_not_central_hold_and_sigterm_drains(self):
        self.start()
        self.failure = 503
        _, lost, _ = self.infer()
        eventually(lambda: self.attempts[(lost, "settle")] > 0)
        self.process.kill()
        self.process.wait(timeout=5)
        attempts = self.attempts[(lost, "settle")]
        self.failure = 0
        self.start()
        _, fresh, _ = self.infer()
        eventually(lambda: fresh in self.settled)
        self.assertEqual(self.attempts[(lost, "settle")], attempts)
        self.assertEqual(self.rows[lost]["state"], "dispatched")
        self.failure = 503
        _, draining, _ = self.infer()
        eventually(lambda: self.attempts[(draining, "settle")] > 0)
        self.process.terminate()
        self.failure = 0
        self.process.wait(timeout=25)
        self.assertEqual(self.process.returncode, 0)
        self.assertIn(draining, self.settled)
        self.assertFalse((self.root / "never-created").exists())

    def test_redirect_is_not_a_commit_ack(self):
        self.start()
        self.failure = 302
        _, identity, _ = self.infer()
        eventually(lambda: self.ready() == 503)
        self.assertEqual(self.attempts[(identity, "settle")], 1)
        self.assertNotIn(identity, self.settled)
        self.assertEqual(self.rows[identity]["state"], "dispatched")

    def test_admission_lost_replies_never_replay_inference(self):
        self.start()
        self.drop_admission_ack = "reservations"
        self.assertEqual(self.infer()[0], 503)
        eventually(lambda: any(row["state"] == "released" for row in self.rows.values()))
        self.drop_admission_ack = "dispatch"
        self.assertEqual(self.infer()[0], 503)
        eventually(lambda: any(row["state"] == "dispatched" for row in self.rows.values()))
        self.assertEqual(self.calls, 0)
        self.assertEqual(self.infer()[0], 200)
        eventually(lambda: len(self.settled) == 1)

    def test_sse_cancel_propagates_and_records_unknown_centrally(self):
        self.start()
        self.cancel_stream = True
        c = http.client.HTTPConnection("127.0.0.1", self.gateway_port, timeout=10)
        c.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "messages": [], "max_tokens": 4, "stream": True}),
            {"Authorization": "Bearer test-key", "Content-Type": "application/json"})
        response = c.getresponse()
        identity = response.getheader("x-xscope-billing-request-id")
        self.assertEqual(response.status, 200)
        self.assertIn(b"fixture", response.readline())
        response.close()
        c.close()
        eventually(lambda: self.runtime_cancelled.is_set() and identity in self.unresolved)
        self.assertEqual(self.rows[identity]["state"], "dispatched")


if __name__ == "__main__":
    GATEWAY = runfiles.Create().Rlocation(sys.argv.pop(1))
    unittest.main()
