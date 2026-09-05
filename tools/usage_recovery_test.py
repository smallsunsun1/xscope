"""Bazel-built gateway + faulting HTTP collector; no live cluster or data."""
import collections
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


def eventually(check, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError("recovery timed out")


class RecoveryTest(unittest.TestCase):
    def test_durable_cursor_rejection_lost_ack_and_live_catchup(self):
        with tempfile.TemporaryDirectory(prefix="xscope-recovery-") as directory:
            root = Path(directory)
            wal = root / "events.jsonl"
            events = [{"schema_version": "v1", "event_id": f"evt-seed-{i}", "request_id": f"seed-{i}",
                "occurred_at": "2026-09-05T00:00:00Z", "tenant_id": "tenant-test", "project_id": "project-test", "api_key_id": "key-test",
                "model_id": "xscope-demo", "model_revision": "v1", "endpoint_id": "pool-test", "region": "local", "price_version": "2026-09-01",
                "input_tokens": 1, "output_tokens": 2, "cached_input_tokens": 0, "latency_ms": 1, "status": "succeeded"} for i in range(1100)]
            wal.write_text("".join(json.dumps(e) + "\n" for e in events))
            attempts, accepted = collections.Counter(), set()
            collector = {"status": 401, "drop_ack": True}

            class Handler(http.server.BaseHTTPRequestHandler):
                def respond(self, value, status=200):
                    body = json.dumps(value).encode()
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def do_GET(self):
                    # Returning a non-success snapshot leaves the configured dev key intact.
                    if "api-keys" in self.path:
                        self.respond({}, 503)
                    else:
                        self.respond({"status": "ready"})

                def do_POST(self):
                    if self.headers.get("Transfer-Encoding") == "chunked":
                        raw = bytearray()
                        while True:
                            size = int(self.rfile.readline().strip(), 16)
                            if not size:
                                self.rfile.readline()
                                break
                            raw.extend(self.rfile.read(size))
                            self.rfile.read(2)
                    else:
                        raw = self.rfile.read(int(self.headers["Content-Length"]))
                    body = json.loads(raw)
                    if self.path.endswith("usage-events"):
                        self.assert_authorization()
                        event_id = body["event_id"]
                        attempts[event_id] += 1
                        if collector["status"] != 200:
                            self.respond({}, collector["status"])
                            return
                        accepted.add(event_id)  # Simulate a committed, deduplicated sink.
                        if collector["drop_ack"]:
                            collector["drop_ack"] = False
                            self.connection.shutdown(socket.SHUT_RDWR)
                            self.connection.close()
                            return
                        self.respond({"accepted": True})
                    else:
                        self.respond({"object": "chat.completion", "choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}})

                def assert_authorization(self):
                    assert self.headers["Authorization"] == "Bearer test-internal"

                def log_message(self, *args):
                    pass

            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            gateway_port = port()
            env = dict(os.environ, XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{gateway_port}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}",
                XSCOPE_USAGE_WAL=str(wal), XSCOPE_REDIS_URL="", XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{server.server_port}",
                XSCOPE_INTERNAL_TOKEN="test-internal", XSCOPE_POLICY_REFRESH_SECONDS="1", XSCOPE_ADDITIONAL_SERVING_JSON="[]",
                XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "pool-test", "model": "xscope-demo", "revision": "v1", "address": f"127.0.0.1:{server.server_port}"}),
                XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-test", "tenant_id": "tenant-test", "project_id": "project-test", "secret": "test-key"}]))
            log = (root / "gateway.log").open("wb")
            process = None

            def healthy():
                try:
                    c = http.client.HTTPConnection("127.0.0.1", gateway_port, timeout=1)
                    c.request("GET", "/readyz")
                    r = c.getresponse()
                    status = r.status
                    r.read()
                    c.close()
                    return status == 200
                except OSError:
                    return False

            def checkpointed():
                path = Path(str(wal) + ".checkpoint")
                return path.exists() and json.loads(path.read_text())["end"] == wal.stat().st_size

            try:
                process = subprocess.Popen([GATEWAY], env=env, stdout=log, stderr=log)
                eventually(healthy)
                eventually(lambda: attempts["evt-seed-0"] > 0)
                self.assertFalse(Path(str(wal) + ".checkpoint").exists(), "401 must not acknowledge or skip a record")
                for _ in range(8):
                    c = http.client.HTTPConnection("127.0.0.1", gateway_port, timeout=5)
                    c.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "stream": False, "messages": []}),
                        {"Authorization": "Bearer test-key", "Content-Type": "application/json"})
                    response = c.getresponse()
                    self.assertEqual(response.status, 200, response.read())
                    c.close()
                eventually(lambda: len(wal.read_text().splitlines()) == 1108)
                collector["status"] = 200
                eventually(checkpointed, timeout=60)
                self.assertEqual(len(accepted), 1108)
                self.assertGreaterEqual(attempts["evt-seed-0"], 3)  # 401 + committed/lost ACK + retry
                process.kill()
                process.wait(timeout=5)
                delivered = sum(attempts.values())
                process = subprocess.Popen([GATEWAY], env=env, stdout=log, stderr=log)
                eventually(healthy)
                time.sleep(2)
                self.assertEqual(sum(attempts.values()), delivered, "acknowledged events must not replay after process crash")
                self.assertEqual(len(wal.read_text().splitlines()), 1108, "journal history is retained")
            finally:
                if process and process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
                log.close()
                server.shutdown()
                server.server_close()
                if self._outcome and not self._outcome.success:
                    print((root / "gateway.log").read_text()[-12000:])


if __name__ == "__main__":
    GATEWAY = runfiles.Create().Rlocation(sys.argv.pop(1))
    unittest.main()
