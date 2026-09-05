"""Real PostgreSQL + two Bazel control planes; disposable local fixtures only.

Tests row-lock money admission, finalization, transactional outbox and ACK recovery.
This does not activate the reservation protocol in the live inference gateway.
"""
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
import urllib.error
import urllib.request
import uuid

from python.runfiles import runfiles
from billing_cursor_checks import verify as verify_cursors
from billing_projection_checks import verify as verify_projections, snapshot as projection_snapshot


def port():
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def eventually(check, timeout=40):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if check():
            return
        time.sleep(0.1)
    raise AssertionError("billing protocol fixture timed out")


def main():
    binary = runfiles.Create().Rlocation(sys.argv[1])
    container = None
    runtime = None
    release_runtime = threading.Event()
    processes, logs, configs = [], [], []
    token = uuid.uuid4().hex
    success = False
    with tempfile.TemporaryDirectory(prefix="xscope-money-") as directory:
        root = Path(directory)
        try:
            image = "postgres:16-alpine"
            subprocess.run(["docker", "image", "inspect", image], check=True, stdout=subprocess.DEVNULL)
            container = subprocess.check_output(["docker", "run", "--rm", "-d", "--name", "xscope-money-smoke-" + uuid.uuid4().hex[:10],
                "--cpus", "0.5", "--memory", "192m", "-p", "127.0.0.1::5432", "-e", "POSTGRES_PASSWORD=" + token, image], text=True).strip()
            pg_port = int(subprocess.check_output(["docker", "port", container, "5432/tcp"], text=True).strip().rsplit(":", 1)[1])
            # Image initialization briefly exposes a temporary Unix-socket-only
            # server. Wait for the final TCP listener used by the application.
            eventually(lambda: subprocess.run(["docker", "exec", container, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0)

            def sql(statement):
                return subprocess.check_output(["docker", "exec", container, "psql", "-U", "postgres", "-At", "-v", "ON_ERROR_STOP=1", "-c", statement], text=True).strip()

            def api(method, path, payload=None, replica=0, internal=True, authenticated=True):
                public, private, _ = configs[replica]
                prefix = "/internal/v1" if internal else "/admin/v1"
                headers = {"Content-Type": "application/json", "Idempotency-Key": "smoke-" + uuid.uuid4().hex}
                if internal and authenticated:
                    headers["Authorization"] = "Bearer " + token
                req = urllib.request.Request(f"http://127.0.0.1:{private if internal else public}" + prefix + path,
                    method=method, headers=headers, data=None if payload is None else json.dumps(payload).encode())
                try:
                    with urllib.request.urlopen(req, timeout=10) as response:
                        return response.status, json.loads(response.read() or "null")
                except urllib.error.HTTPError as error:
                    return error.code, json.loads(error.read())

            def ready(index):
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{configs[index][0]}/readyz", timeout=1) as response:
                        return response.status == 200
                except OSError:
                    return False

            def launch(index):
                _, _, env = configs[index]
                log = (root / f"control-{index}-{len(logs)}.log").open("wb")
                logs.append(log)
                process = subprocess.Popen([binary], env=env, stdout=log, stderr=log)
                processes.append(process)
                eventually(lambda: ready(index))
                return process

            for index in range(2):
                public, private = port(), port()
                env = dict(os.environ, XSCOPE_DATABASE_URL=f"postgres://postgres:{token}@127.0.0.1:{pg_port}/postgres",
                    XSCOPE_INTERNAL_TOKEN=token, XSCOPE_CONTROL_ADDRESS=f"127.0.0.1:{public}", XSCOPE_CONTROL_INTERNAL_ADDRESS=f"127.0.0.1:{private}",
                    XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}", XSCOPE_CONSOLE_AUTH="disabled", XSCOPE_BOOTSTRAP_API_KEYS_JSON="[]", XSCOPE_DEFAULT_TENANT_ID="test-tenant")
                env.pop("XSCOPE_CONSOLE_DIR", None)
                configs.append((public, private, env))
                launch(index)

            def setup(project, funded=False, budget=0):
                assert api("POST", "/projects", {"id": project, "tenant_id": "test-tenant", "name": project}, internal=False)[0] == 201
                status, result = api("POST", "/api-keys", {"id": "key-" + project, "project_id": project, "tenant_id": "test-tenant", "name": "test",
                    "scopes": ["chat.completions"], "allowed_models": ["xscope-demo"], "monthly_budget": {"currency": "CNY", "amount": budget}}, internal=False)
                assert status == 201, result
                if funded:
                    assert api("POST", "/billing/orders", {"id": "order-" + project, "project_id": project, "tenant_id": "test-tenant", "amount": {"currency": "CNY", "amount": 1}}, internal=False)[0] == 201
                    assert api("POST", "/billing/orders/order-" + project + "/payments", {"id": "pay-" + project, "provider": "manual-test", "provider_reference": project}, internal=False)[0] == 201
                    assert api("PUT", "/billing/accounts/" + project, {"enforce_balance": True}, internal=False)[0] == 200
                return result

            def request(name, project="funded", tokens=700):
                return {"id": name, "tenant_id": "test-tenant", "project_id": project, "api_key_id": "key-" + project, "request_id": "req-" + name,
                    "model_id": "xscope-demo", "model_revision": "development", "price_version": "2026-09-01", "input_token_limit": tokens, "output_token_limit": 0}

            def path(name, project="funded"):
                return f"/billing/projects/{project}/reservations/{name}"

            setup("funded", funded=True)
            setup("budget", budget=1)
            assert api("POST", "/billing/reservations", request("unauthorized"), authenticated=False)[0] == 401
            assert api("POST", "/billing/reservations", {**request("wrong-tenant"), "tenant_id": "elsewhere"})[0] == 403
            assert api("POST", "/billing/reservations", {**request("bad-price"), "price_version": "unknown"})[0] == 400

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                results = list(executor.map(lambda i: api("POST", "/billing/reservations", request(f"race-{i}"), replica=i % 2), range(8)))
            assert sorted(status for status, _ in results) == [200] + [402] * 7, results
            winner = next(row for status, row in results if status == 200)
            assert winner["reserved_microunits"] == 700000
            repeat = request(winner["id"])
            assert api("POST", "/billing/reservations", repeat, replica=1)[1] == winner
            assert api("POST", "/billing/reservations", {**repeat, "input_token_limit": 600})[0] == 409
            print("PASS two control planes: one funded admission out of eight concurrent holds; replay and payload conflict", flush=True)

            feed = "/billing/projects/funded/consumers/test-consumer"
            initial = api("POST", feed + "/poll", {"limit": 1})[1]
            assert initial["data"][0]["sequence"] == 1
            assert api("POST", feed + "/poll", {"limit": 1}, replica=1)[1]["data"] == initial["data"]
            assert api("POST", feed + "/ack", {"expected_sequence": 0, "sequence": 99})[0] == 409
            assert api("POST", feed + "/ack", {"expected_sequence": 0, "sequence": 1})[0] == 200
            assert api("POST", feed + "/ack", {"expected_sequence": 0, "sequence": 1})[0] == 200
            assert api("POST", feed + "/poll", {"limit": 1})[1]["data"][0]["sequence"] == 2
            assert api("POST", feed + "/ack", {"expected_sequence": 0, "sequence": 2})[0] == 409
            assert api("POST", feed + "/ack", {"expected_sequence": 1, "sequence": 2})[0] == 200
            print("PASS outbox redelivery, ordered batches, CAS ACK and undelivered-ACK rejection", flush=True)

            release = {"reason": "not_dispatched"}
            assert api("POST", path(winner["id"]) + "/release", release)[0] == 200
            assert api("POST", path(winner["id"]) + "/release", release)[0] == 200
            assert api("POST", path(winner["id"]) + "/dispatch", {})[0] == 409
            assert api("POST", "/billing/reservations", request("settle"))[0] == 200
            assert api("POST", path("settle") + "/dispatch", {})[0] == 200
            assert api("POST", path("settle") + "/dispatch", {}, replica=1)[0] == 200
            assert api("POST", path("settle") + "/release", release)[0] == 409
            assert api("GET", path("settle", "budget"))[0] == 404
            assert api("POST", "/usage-events", {"schema_version": "v1", "event_id": "legacy-bypass", "request_id": "req-settle",
                "occurred_at": "2026-09-05T00:00:00Z", "tenant_id": "test-tenant", "project_id": "funded", "api_key_id": "key-funded",
                "model_id": "xscope-demo", "model_revision": "development", "price_version": "2026-09-01", "region": "local", "endpoint_id": "demo-pool",
                "input_tokens": 1, "output_tokens": 0, "cached_input_tokens": 0, "latency_ms": 1, "status": "succeeded"})[0] == 409
            settlement = {"input_tokens": 200, "output_tokens": 0, "latency_ms": 10, "endpoint_id": "demo-pool", "region": "local", "status": "cancelled"}
            assert api("POST", path("settle") + "/settle", {**settlement, "input_tokens": 701})[0] == 409
            assert api("POST", path("settle") + "/settle", {**settlement, "status": "usage_pending"})[0] == 400
            assert api("POST", "/billing/refunds", {"id": "refund-held", "payment_id": "pay-funded", "amount": {"currency": "CNY", "amount": 1}, "reason": "test protected hold"}, internal=False)[0] == 402
            assert sql("SELECT count(*) FROM xscope.refunds") == "0"

            # Inject a database failure after the ledger writes but before COMMIT.
            # SQL is test-fixture fault injection only; application writes use SeaORM.
            sql("CREATE FUNCTION fail_test_event() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind = 'reservation.settled' THEN RAISE EXCEPTION 'injected outbox failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_fail BEFORE INSERT ON xscope.billing_events FOR EACH ROW EXECUTE FUNCTION fail_test_event()")
            projections_before = projection_snapshot(sql)
            assert api("POST", path("settle") + "/settle", settlement)[0] == 503
            assert projection_snapshot(sql) == projections_before
            assert api("GET", path("settle"))[1]["state"] == "dispatched"
            assert sql("SELECT count(*) FROM xscope.usage_events WHERE event_id='evt-reservation-settle'") == "0"
            assert sql("SELECT count(*) FROM xscope.ledger_transactions WHERE idempotency_key='reservation:settle'") == "0"
            sql("DROP TRIGGER test_fail ON xscope.billing_events; DROP FUNCTION fail_test_event()")
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                settled = list(executor.map(lambda i: api("POST", path("settle") + "/settle", settlement, replica=i), range(2)))
            assert [s for s, _ in settled] == [200, 200], settled
            assert settled[0][1] == settled[1][1] and settled[0][1]["settled_microunits"] == 200000
            assert api("POST", path("settle") + "/settle", {**settlement, "input_tokens": 201})[0] == 409
            assert sql("SELECT count(*),sum(e.amount_microunits) FROM xscope.ledger_entries e JOIN xscope.ledger_transactions t ON t.id=e.transaction_id WHERE t.idempotency_key='reservation:settle'") == "2|0"
            assert sql("SELECT count(*) FROM xscope.billing_events WHERE kind='reservation.settled'") == "1"
            print("PASS holds protect refunds; partial cancellation charges known usage; ledger/usage/hold/outbox rollback together and settle once", flush=True)

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                results = list(executor.map(lambda i: api("POST", "/billing/reservations", request(f"budget-{i}", "budget"), replica=i % 2), range(8)))
            assert sorted(s for s, _ in results) == [200] + [402] * 7, results
            held = next(r for s, r in results if s == 200)
            assert api("POST", path(held["id"], "budget") + "/dispatch", {})[0] == 200
            print("PASS monthly key budget admission remains atomic without balance enforcement", flush=True)

            # Restart both consumers of the shared DB; no in-memory state can satisfy this.
            for process in processes[:]:
                process.kill()
                process.wait(timeout=5)
            for index in range(2):
                launch(index)
            assert api("GET", path(held["id"], "budget"))[1]["state"] == "dispatched"
            assert api("POST", path(held["id"], "budget") + "/release", release)[0] == 409
            assert api("POST", path("settle") + "/settle", settlement)[0] == 200
            batch = api("POST", feed + "/poll", {"limit": 100}, replica=1)[1]
            assert batch["acknowledged"] == 2
            sequences = [e["sequence"] for e in batch["data"]]
            assert sequences == list(range(3, batch["delivered"] + 1))
            assert api("POST", feed + "/ack", {"expected_sequence": 2, "sequence": batch["delivered"]})[0] == 200
            assert not api("POST", feed + "/poll", {"limit": 100})[1]["data"]
            print("PASS process restart preserves ambiguous holds, idempotent settlement and durable account-ordered ACK", flush=True)

            key = setup("gateway-money", funded=True)
            runtime_calls = []

            class Runtime(http.server.BaseHTTPRequestHandler):
                def do_POST(self):
                    raw = bytearray()
                    while True:
                        line = self.rfile.readline().strip()
                        if not line:
                            return  # denied before prompt bytes
                        size = int(line, 16)
                        if not size:
                            self.rfile.readline()
                            break
                        raw.extend(self.rfile.read(size))
                        self.rfile.read(2)
                    runtime_calls.append(json.loads(raw))
                    release_runtime.wait(timeout=20)
                    data = json.dumps({"object": "chat.completion", "choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 2}}).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)

                def log_message(self, *args):
                    pass

            runtime = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Runtime)
            threading.Thread(target=runtime.serve_forever, daemon=True).start()
            gateway_ports = []
            for i in range(2):
                gateway_port = port()
                gateway_ports.append(gateway_port)
                env = dict(os.environ, XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{gateway_port}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}",
                    XSCOPE_USAGE_WAL=str(root / f"gateway-{i}.jsonl"), XSCOPE_REDIS_URL="",
                    XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{configs[i][1]}/internal/v1", XSCOPE_INTERNAL_TOKEN=token,
                    XSCOPE_BILLING_RESERVATIONS="true", XSCOPE_MODEL_CONTEXT_TOKENS="700", XSCOPE_ADDITIONAL_SERVING_JSON="[]",
                    XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "demo-pool", "model": "xscope-demo", "revision": "development", "address": f"127.0.0.1:{runtime.server_port}"}),
                    XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-gateway-money", "tenant_id": "test-tenant", "project_id": "gateway-money", "secret": key["secret"]}]))
                log = (root / f"gateway-{i}.log").open("wb")
                logs.append(log)
                processes.append(subprocess.Popen([runfiles.Create().Rlocation(sys.argv[2])], env=env, stdout=log, stderr=log))

                def gateway_ready():
                    try:
                        with urllib.request.urlopen(f"http://127.0.0.1:{gateway_port}/readyz", timeout=1) as response:
                            return response.status == 200
                    except OSError:
                        return False
                eventually(gateway_ready)

            def infer(index):
                connection = http.client.HTTPConnection("127.0.0.1", gateway_ports[index % 2], timeout=25)
                try:
                    connection.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "messages": [], "max_tokens": 4}),
                        {"Authorization": "Bearer " + key["secret"], "Content-Type": "application/json", "X-Request-Id": "duplicate-client-correlation"})
                    response = connection.getresponse()
                    result = response.status, response.getheader("x-xscope-billing-request-id"), response.read()
                    return result
                finally:
                    connection.close()

            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
                futures = [executor.submit(infer, i) for i in range(8)]
                eventually(lambda: sum(f.done() for f in futures) >= 7)
                assert len(runtime_calls) == 1, runtime_calls
                assert sql("SELECT count(*) FROM xscope.billing_reservations WHERE project_id='gateway-money' AND state='dispatched'") == "1"
                release_runtime.set()
                results = [future.result() for future in futures]
            assert sorted(result[0] for result in results) == [200] + [402] * 7, results
            first_id = next(result[1] for result in results if result[0] == 200)
            eventually(lambda: api("GET", path(first_id, "gateway-money"))[1]["state"] == "settled")
            second = infer(1)
            assert second[0] == 200 and second[1] != first_id, second
            eventually(lambda: api("GET", path(second[1], "gateway-money"))[1]["state"] == "settled")
            assert len(runtime_calls) == 2
            assert sql("SELECT count(DISTINCT t.id),count(e.id),sum(e.amount_microunits) FROM xscope.ledger_transactions t JOIN xscope.ledger_entries e ON e.transaction_id=t.id WHERE t.idempotency_key IN ('reservation:" + first_id + "','reservation:" + second[1] + "')") == "2|4|0"
            print("PASS two actual Gateways / two control planes: 8 concurrent requests -> 1 Runtime call + 7 HTTP 402; repeated client ID creates distinct balanced settlements", flush=True)
            verify_cursors(api, sql, container, setup, request)
            verify_projections(api, sql, container, setup, request)
            setup("console-pending")
            assert api("POST", "/billing/reservations", request("console-evidence", project="console-pending"))[0] == 200
            assert api("POST", "/billing/projects/console-pending/reservations/console-evidence/dispatch", {})[0] == 200
            sql("UPDATE xscope.billing_reservations SET created_at=now()-interval '1 hour' WHERE id='console-evidence'")
            # Console routes use the public session boundary, never the
            # Gateway internal token. Start an isolated trusted-header reader.
            public, private = port(), port()
            console_env = dict(configs[0][2], XSCOPE_CONTROL_ADDRESS=f"127.0.0.1:{public}",
                XSCOPE_CONTROL_INTERNAL_ADDRESS=f"127.0.0.1:{private}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}",
                XSCOPE_CONSOLE_AUTH="trusted-headers", XSCOPE_AUTO_JOIN_DEFAULT_TENANT="false", XSCOPE_BOOTSTRAP_ADMIN_USERS="console-smoke-owner")
            configs.append((public, private, console_env))
            launch(len(configs) - 1)
            def console_get(path, user=None):
                headers = {} if user is None else {"X-Auth-Request-User": user, "X-Auth-Request-Preferred-Username": user, "X-Auth-Request-Sub": user}
                req = urllib.request.Request(f"http://127.0.0.1:{public}/admin/v1" + path, headers=headers)
                try:
                    with urllib.request.urlopen(req, timeout=10) as response:
                        return response.status, json.load(response)
                except urllib.error.HTTPError as error:
                    return error.code, json.load(error)
            position_path = "/billing/accounts/funded/position"
            pending_path = "/billing/accounts/console-pending/pending-reservations"
            for endpoint in (position_path, pending_path):
                assert console_get(endpoint)[0] == 401
                assert console_get(endpoint, "console-smoke-outsider")[0] == 403
            status, user = console_get("/session", "console-smoke-member")
            assert status == 200
            sql("INSERT INTO xscope.tenant_memberships (id,user_id,tenant_id,role,created_at) VALUES ('console-read-member','" + user["id"] + "','test-tenant','member',now())")
            assert console_get(position_path, "console-smoke-member")[0] == 200
            assert console_get(pending_path, "console-smoke-member")[0] == 403
            status, position = console_get(position_path, "console-smoke-owner")
            assert status == 200 and isinstance(position["held_microunits"], str)
            assert int(position["available_microunits"]) == int(position["balance_microunits"]) - int(position["held_microunits"])
            status, pending = console_get(pending_path + "?limit=1", "console-smoke-owner")
            assert status == 200 and len(pending["data"]) == 1 and pending["data"][0]["id"] == "console-evidence"
            assert all("spec" not in row and "price" not in row and "completion" not in row and isinstance(row["reserved_microunits"], str) for row in pending["data"])
            assert console_get(pending_path + "?limit=101", "console-smoke-owner")[0] == 400
            print("PASS console positions and bounded pending discovery: anonymous/outsider denied, member financial read, owner-only evidence list, exact string money and redacted DTO", flush=True)
            success = True
        finally:
            release_runtime.set()
            for process in processes:
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
            for log in logs:
                log.close()
            if runtime:
                runtime.shutdown()
                runtime.server_close()
            if not success:
                for log in root.glob("*.log"):
                    print(log.read_text()[-8000:])
            if container:
                subprocess.run(["docker", "rm", "-f", container], check=True, stdout=subprocess.DEVNULL)
            print("Removed disposable PostgreSQL and test processes; live cluster/accounting data unchanged.", flush=True)


if __name__ == "__main__":
    main()
