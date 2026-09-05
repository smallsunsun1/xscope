"""Bazel Gateway against a fault-injecting protocol peer; no cluster writes.

Protocol database atomicity is covered separately by billing_protocol_smoke.
This test checks the actual Pingora admission/WAL/provider ordering under faults.
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


def eventually(check, timeout=25):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError("gateway billing recovery timed out")


class GatewayBillingTest(unittest.TestCase):
    def test_faults_and_crash_recovery(self):
        with tempfile.TemporaryDirectory(prefix="xscope-gateway-money-") as directory:
            root = Path(directory)
            rows, settlements, attempts = {}, {}, collections.Counter()
            state = {"runtime_calls": 0, "fault": None, "block": None, "deny": False, "usage": "known"}
            entered, unblock = threading.Event(), threading.Event()
            lock = threading.Lock()

            class Handler(http.server.BaseHTTPRequestHandler):
                def respond(self, value, status=200):
                    raw = json.dumps(value).encode()
                    self.send_response(status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(raw)))
                    self.end_headers()
                    try:
                        self.wfile.write(raw)
                    except (BrokenPipeError, ConnectionResetError):
                        pass

                def do_GET(self):
                    if "/reservations/" in self.path:
                        assert self.headers["Authorization"] == "Bearer test-internal"
                        with lock:
                            row = dict(rows[self.path.rsplit("/", 1)[1]])
                        self.respond(row)
                    else:
                        self.respond({}, 503)  # retain static fixture key

                def do_POST(self):
                    if self.headers.get("Transfer-Encoding") == "chunked":
                        raw = bytearray()
                        while True:
                            line = self.rfile.readline().strip()
                            if not line:
                                return  # admission rejected before any prompt bytes
                            size = int(line, 16)
                            if not size:
                                self.rfile.readline()
                                break
                            raw.extend(self.rfile.read(size))
                            self.rfile.read(2)
                    else:
                        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
                    body = json.loads(raw or "{}")
                    action = "reserve" if self.path.endswith("/billing/reservations") else self.path.rsplit("/", 1)[1]
                    status = 200
                    with lock:
                        if action == "completions":
                            state["runtime_calls"] += 1
                            assert body.get("max_tokens", body.get("max_completion_tokens")) == 4
                            assert "Authorization" not in self.headers
                            assert any(row["state"] == "dispatched" for row in rows.values())
                            result = {"object": "chat.completion", "choices": []}
                            if state["usage"] != "unknown":
                                result["usage"] = {"prompt_tokens": 1, "completion_tokens": 2 if state["usage"] == "known" else 100}
                        else:
                            assert self.headers["Authorization"] == "Bearer test-internal"
                            assert action != "usage-events", "new requests must not use legacy settlement"
                            identity = body["id"] if action == "reserve" else self.path.split("/")[-2]
                            attempts[(identity, action)] += 1
                            if action == "reserve":
                                intents = Path(str(wal) + ".reservations.jsonl").read_text()
                                assert intents.endswith("\n")
                                assert any(json.loads(line)["request"]["id"] == identity for line in intents.splitlines())
                                if identity in rows:
                                    assert rows[identity]["spec"] == body
                                elif state["deny"]:
                                    status = 402
                                else:
                                    rows[identity] = {"state": "reserved", "spec": body}
                            elif action == "dispatch":
                                assert rows[identity]["state"] in ("reserved", "dispatched")
                                rows[identity]["state"] = "dispatched"
                            elif action == "release":
                                assert rows[identity]["state"] in ("reserved", "released"), "never release dispatched work"
                                rows[identity]["state"] = "released"
                            elif action == "settle":
                                assert rows[identity]["state"] in ("dispatched", "settled")
                                if identity in settlements:
                                    assert settlements[identity] == body
                                settlements[identity] = body
                                rows[identity]["state"] = "settled"
                            else:
                                raise AssertionError(action)
                            result = dict(rows.get(identity, {}))
                        drop = state["fault"] == action
                        if drop:
                            state["fault"] = None
                        block = state["block"] == action
                        if block:
                            state["block"] = None
                    if block:
                        entered.set()
                        unblock.wait(timeout=15)
                    if drop:
                        self.connection.shutdown(socket.SHUT_RDWR)
                        self.connection.close()
                    else:
                        self.respond(result, status)

                def log_message(self, *args):
                    pass

            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            threading.Thread(target=server.serve_forever, daemon=True).start()
            gateway_port = port()
            wal = root / "events.jsonl"
            env = dict(os.environ, XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{gateway_port}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}",
                XSCOPE_USAGE_WAL=str(wal), XSCOPE_REDIS_URL="", XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{server.server_port}/internal/v1",
                XSCOPE_INTERNAL_TOKEN="test-internal", XSCOPE_BILLING_RESERVATIONS="true", XSCOPE_MODEL_CONTEXT_TOKENS="128",
                XSCOPE_ADDITIONAL_SERVING_JSON="[]", XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "test-pool", "model": "xscope-demo", "revision": "development", "address": f"127.0.0.1:{server.server_port}"}),
                XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-test", "tenant_id": "test-tenant", "project_id": "test-project", "secret": "test-key"}]))
            process, logs = None, []

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

            def launch():
                log = (root / f"gateway-{len(logs)}.log").open("wb")
                logs.append(log)
                p = subprocess.Popen([GATEWAY], env=env, stdout=log, stderr=log)
                eventually(healthy)
                return p

            def infer(**overrides):
                c = http.client.HTTPConnection("127.0.0.1", gateway_port, timeout=12)
                try:
                    c.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "messages": [], "max_tokens": 4, **overrides}),
                        {"Authorization": "Bearer test-key", "Content-Type": "application/json", "X-Request-Id": "same-client-id"})
                    r = c.getresponse()
                    result = r.status, r.getheader("x-xscope-billing-request-id"), r.read()
                    return result
                finally:
                    c.close()

            def caught_up():
                if len(wal.read_text().splitlines()) != state["runtime_calls"]:
                    return False  # logging runs after the client sees the last byte
                for path in (wal, Path(str(wal) + ".reservations.jsonl")):
                    checkpoint = Path(str(path) + ".checkpoint")
                    if path.stat().st_size and (not checkpoint.exists() or json.loads(checkpoint.read_text())["end"] != path.stat().st_size):
                        return False
                return True

            try:
                process = launch()
                first, second = infer(), infer()
                self.assertEqual([first[0], second[0]], [200, 200])
                self.assertNotEqual(first[1], second[1])
                eventually(lambda: len(settlements) == 2)
                self.assertEqual(rows[first[1]]["spec"]["input_token_limit"], 124)
                self.assertEqual(rows[first[1]]["spec"]["request_id"], first[1])
                before = state["runtime_calls"]
                for bad in ({"max_tokens": 0}, {"max_tokens": 128}, {"max_completion_tokens": 3}, {"n": 2}):
                    self.assertEqual(infer(**bad)[0], 400)
                state["deny"] = True
                self.assertEqual(infer()[0], 402)
                eventually(healthy)
                self.assertEqual(state["runtime_calls"], before)
                state["deny"] = False

                for action, expected in (("reserve", "released"), ("dispatch", "dispatched")):
                    old_ids = set(rows)
                    state["fault"] = action
                    self.assertEqual(infer()[0], 503)
                    eventually(healthy)
                    identity = (set(rows) - old_ids).pop()
                    self.assertEqual(rows[identity]["state"], expected)
                    self.assertEqual(state["runtime_calls"], before)

                state["fault"] = "settle"
                success = infer()
                self.assertEqual(success[0], 200)
                eventually(caught_up)
                self.assertGreaterEqual(attempts[(success[1], "settle")], 2)
                self.assertEqual(len(settlements), 3)
                self.assertEqual(state["runtime_calls"], before + 1)

                for usage in ("unknown", "over-limit"):
                    state["usage"] = usage
                    pending = infer()
                    self.assertEqual(pending[0], 200)
                    eventually(caught_up)
                    self.assertEqual(rows[pending[1]]["state"], "dispatched")
                    self.assertNotIn(pending[1], settlements)
                state["usage"] = "known"
                self.assertEqual(infer()[0], 200)
                eventually(caught_up)

                for action, expected in (("reserve", "released"), ("dispatch", "dispatched")):
                    old_ids, before = set(rows), state["runtime_calls"]
                    entered.clear()
                    unblock.clear()
                    state["block"] = action
                    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
                        future = executor.submit(infer)
                        self.assertTrue(entered.wait(timeout=10))
                        process.kill()
                        process.wait(timeout=5)
                        unblock.set()
                        try:
                            future.result(timeout=5)
                        except OSError:
                            pass
                    process = launch()
                    eventually(caught_up)
                    identity = (set(rows) - old_ids).pop()
                    self.assertEqual(rows[identity]["state"], expected)
                    self.assertEqual(state["runtime_calls"], before)
                    self.assertTrue(healthy())
                self.assertEqual(len(settlements), 4)
                entered.clear()
                unblock.clear()
                state["block"] = "settle"
                final = infer()
                self.assertEqual(final[0], 200)
                self.assertTrue(entered.wait(timeout=10))
                before = state["runtime_calls"]
                process.kill()
                process.wait(timeout=5)
                unblock.set()
                process = launch()
                eventually(caught_up)
                self.assertEqual(state["runtime_calls"], before)
                self.assertEqual(len(settlements), 5)
                self.assertGreaterEqual(attempts[(final[1], "settle")], 2)
            except Exception:
                for log in root.glob("*.log"):
                    print(log.read_text()[-8000:])
                raise
            finally:
                unblock.set()
                if process and process.poll() is None:
                    process.kill()
                if process:
                    process.wait(timeout=5)
                server.shutdown()
                server.server_close()
                for log in logs:
                    log.close()


if __name__ == "__main__":
    GATEWAY = runfiles.Create().Rlocation(sys.argv.pop(1))
    unittest.main()
