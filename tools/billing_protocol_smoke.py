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
from billing_review_checks import verify as verify_reviews
from event_worker_checks import verify as verify_workers, verify_console as verify_worker_console
from cluster_protocol_checks import verify as verify_clusters, verify_console as verify_cluster_console
from cluster_pull_checks import verify as verify_cluster_pull
from payment_checks import verify as verify_payments
from model_catalog_checks import verify as verify_catalog
from managed_traffic_checks import verify as verify_managed_traffic
from ops_checks import verify as verify_ops
from billing_load_checks import verify as verify_load


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
                "--cpus", "0.5", "--memory", "192m", "-p", "127.0.0.1::5432", "-e", "POSTGRES_PASSWORD", image], env={**os.environ, "POSTGRES_PASSWORD": token}, text=True).strip()
            pg_port = int(subprocess.check_output(["docker", "port", container, "5432/tcp"], text=True).strip().rsplit(":", 1)[1])
            # Image initialization briefly exposes a temporary Unix-socket-only
            # server. Wait for the final TCP listener used by the application.
            eventually(lambda: subprocess.run(["docker", "exec", container, "pg_isready", "-h", "127.0.0.1", "-U", "postgres"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0)

            def sql(statement):
                return subprocess.check_output(["docker", "exec", container, "psql", "-U", "postgres", "-At", "-v", "ON_ERROR_STOP=1", "-c", statement], text=True).strip()

            def api(method, path, payload=None, replica=0, internal=True, authenticated=True, credential=None):
                public, private, _ = configs[replica]
                prefix = "/internal/v1" if internal else "/admin/v1"
                headers = {"Content-Type": "application/json", "Idempotency-Key": "smoke-" + uuid.uuid4().hex}
                if internal and authenticated:
                    headers["Authorization"] = "Bearer " + (credential or token)
                req = urllib.request.Request(f"http://127.0.0.1:{private if internal else public}" + prefix + path,
                    method=method, headers=headers, data=payload if isinstance(payload, bytes) else None if payload is None else json.dumps(payload).encode())
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
                    XSCOPE_INTERNAL_TOKEN=token, XSCOPE_ALERT_WEBHOOK_TOKEN=token, XSCOPE_CONTROL_ADDRESS=f"127.0.0.1:{public}", XSCOPE_CONTROL_INTERNAL_ADDRESS=f"127.0.0.1:{private}",
                    XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}", XSCOPE_EVENT_WORKER_ENABLED="false", XSCOPE_CONSOLE_AUTH="disabled", XSCOPE_BOOTSTRAP_API_KEYS_JSON="[]", XSCOPE_DEFAULT_TENANT_ID="test-tenant")
                env.pop("XSCOPE_CONSOLE_DIR", None)
                configs.append((public, private, env))
                launch(index)

            def setup(project, funded=False, budget=0, scopes=None):
                assert api("POST", "/projects", {"id": project, "tenant_id": "test-tenant", "name": project}, internal=False)[0] == 201
                status, result = api("POST", "/api-keys", {"id": "key-" + project, "project_id": project, "tenant_id": "test-tenant", "name": "test",
                    "scopes": scopes or ["chat.completions"], "allowed_models": ["xscope-demo"], "monthly_budget": {"currency": "CNY", "amount": budget}}, internal=False)
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

            if "--load-only" in sys.argv:
                verify_load(api, sql, setup, request)
                # SIGTERM must drain an already accepted internal HTTP body,
                # not abort the financial listener as soon as public HTTP stops.
                connection = http.client.HTTPConnection("127.0.0.1", configs[0][1], timeout=5)
                connection.putrequest("POST", "/internal/v1/billing/reservations")
                connection.putheader("Content-Type", "application/json")
                connection.putheader("Authorization", "Bearer " + token)
                connection.putheader("Content-Length", "2")
                connection.endheaders(); connection.send(b"{")
                def in_progress():
                    with urllib.request.urlopen("http://" + configs[0][2]["XSCOPE_METRICS_ADDRESS"] + "/metrics", timeout=2) as response:
                        return 'xscope_http_inflight{route="/internal/v1/billing/reservations"} 1' in response.read().decode()
                eventually(in_progress)
                processes[0].terminate()
                time.sleep(.15)
                assert processes[0].poll() is None
                connection.send(b"}")
                response = connection.getresponse()
                assert response.status in (400, 422)
                response.read(); connection.close()
                assert processes[0].wait(timeout=10) == 0
                print("PASS SIGTERM drains the accepted internal HTTP request and exits cleanly; no financial hold created by shutdown probe", flush=True)
                success = True
                return

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

            key = setup("gateway-money", funded=True, scopes=["chat.completions", "completions"])
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
            gateway_processes = []
            for i in range(2):
                gateway_port = port()
                gateway_ports.append(gateway_port)
                env = dict(os.environ, XSCOPE_GATEWAY_ADDRESS=f"127.0.0.1:{gateway_port}", XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}",
                    XSCOPE_USAGE_MODE="memory", XSCOPE_REDIS_URL="", XSCOPE_GATEWAY_GRACE_SECONDS="1",
                    XSCOPE_CONTROL_INTERNAL_URL=f"http://127.0.0.1:{configs[i][1]}/internal/v1", XSCOPE_INTERNAL_TOKEN=token,
                    XSCOPE_BILLING_RESERVATIONS="true", XSCOPE_MODEL_CONTEXT_TOKENS="700", XSCOPE_ADDITIONAL_SERVING_JSON="[]",
                    XSCOPE_SERVING_ENTRY_JSON=json.dumps({"id": "demo-pool", "model": "xscope-demo", "revision": "development", "address": f"127.0.0.1:{runtime.server_port}"}),
                    XSCOPE_API_KEYS_JSON=json.dumps([{"id": "key-gateway-money", "tenant_id": "test-tenant", "project_id": "gateway-money", "secret": key["secret"]}]))
                log = (root / f"gateway-{i}.log").open("wb")
                logs.append(log)
                processes.append(subprocess.Popen([runfiles.Create().Rlocation(sys.argv[2])], env=env, stdout=log, stderr=log))
                gateway_processes.append(processes[-1])

                def gateway_ready():
                    try:
                        with urllib.request.urlopen(f"http://127.0.0.1:{gateway_port}/readyz", timeout=1) as response:
                            return response.status == 200
                    except OSError:
                        return False
                eventually(gateway_ready)

            def infer(index, route="/v1/chat/completions", prompt=None):
                connection = http.client.HTTPConnection("127.0.0.1", gateway_ports[index % 2], timeout=25)
                try:
                    connection.request("POST", route, json.dumps({"model": "xscope-demo", **({"messages": []} if prompt is None else {"prompt": prompt}), "max_tokens": 4}),
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
            completion = infer(0, "/v1/completions", "synthetic text")
            assert completion[0] == 200, completion[0]
            eventually(lambda: api("GET", path(completion[1], "gateway-money"))[1]["state"] == "settled")
            assert runtime_calls[-1]["prompt"] == "synthetic text" and "messages" not in runtime_calls[-1]
            assert runtime_calls[-1]["max_tokens"] == 4
            before_calls = len(runtime_calls)
            assert infer(1, "/v1/completions", ["a", "b"])[0] == 400
            assert len(runtime_calls) == before_calls
            print("PASS text completions: scoped Gateway route, original prompt forwarded, enforced output bound and same balanced settlement; batch prompts denied before Runtime", flush=True)
            # The following tests deliberately compare global projection rows.
            # Quiesce unrelated snapshot cold-backfills rather than racing them
            # with assertions that a specific idempotent write changed nothing.
            for gateway_process in gateway_processes:
                gateway_process.terminate()
            for gateway_process in gateway_processes:
                gateway_process.wait(timeout=20)
            verify_workers(api, sql, setup, request)
            verify_clusters(api, sql)
            verify_cluster_pull(api, runfiles.Create().Rlocation(sys.argv[3]), root, configs[0][1], port, eventually)
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
                XSCOPE_CONSOLE_AUTH="trusted-headers", XSCOPE_AUTO_JOIN_DEFAULT_TENANT="false", XSCOPE_BOOTSTRAP_ADMIN_USERS="console-smoke-owner",
                XSCOPE_PLATFORM_ADMIN_SUBJECTS="console-smoke-owner",
                XSCOPE_BILLING_REVIEWER_SUBJECTS="console-smoke-owner,finance-reviewer,finance-reviewer-2")
            configs.append((public, private, console_env))
            launch(len(configs) - 1)
            def console_call(method, path, payload=None, user=None):
                headers = {} if user is None else {"X-Auth-Request-User": user, "X-Auth-Request-Preferred-Username": user, "X-Auth-Request-Sub": user}
                headers["Content-Type"] = "application/json"
                req = urllib.request.Request(f"http://127.0.0.1:{public}/admin/v1" + path, headers=headers, method=method,
                    data=None if payload is None else json.dumps(payload).encode())
                try:
                    with urllib.request.urlopen(req, timeout=10) as response:
                        return response.status, json.load(response)
                except urllib.error.HTTPError as error:
                    return error.code, json.load(error)
            def console_get(path, user=None):
                return console_call("GET", path, user=user)
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
            verify_worker_console(console_call, sql)
            verify_ops(api, console_call, sql)
            verify_cluster_console(console_call)
            verify_catalog(api, sql, setup, request, console_call)
            verify_managed_traffic(api, sql, root, configs, token, runfiles.Create().Rlocation(sys.argv[2]), runfiles.Create().Rlocation(sys.argv[3]), port, eventually, console_call)
            verify_managed_traffic(api, sql, root, configs, token, runfiles.Create().Rlocation(sys.argv[2]), runfiles.Create().Rlocation(sys.argv[3]), port, eventually, console_call, auto=True)
            verify_reviews(console_call, api, sql, setup, request)
            review_path = "/billing/accounts/review-money/reviews/case-a"
            persisted_review = console_get(review_path, "finance-reviewer")[1]
            processes[-1].kill()
            processes[-1].wait(timeout=5)
            launch(len(configs) - 1)
            assert console_get(review_path, "finance-reviewer")[1] == persisted_review
            assert console_call("POST", review_path + "/decision", persisted_review["decision"], "finance-reviewer")[0] == 200
            print("PASS review evidence/audit survive process restart; committed decision replay does not charge again", flush=True)
            for table in ("operation_audits", "billing_effects", "cluster_revisions", "model_prices", "route_revisions"):
                result = subprocess.run(["docker", "exec", container, "psql", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-c", "DELETE FROM xscope." + table], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                assert result.returncode != 0
            assert console_get("/audit", "console-smoke-member")[0] == 403
            assert console_get("/audit?limit=101", "console-smoke-owner")[0] == 400
            assert len(console_get("/audit?limit=2", "console-smoke-owner")[1]["data"]) == 2
            assert sql("SELECT count(*)>0 FROM xscope.operation_audits WHERE action='console.intent'") == "t"
            print("PASS append-only audit/effects/revisions, bounded admin-only audit API and durable console mutation intents", flush=True)
            processes[-1].kill()
            processes[-1].wait(timeout=5)
            console_env["XSCOPE_EVENT_WORKER_ENABLED"] = "true"
            launch(len(configs) - 1)
            eventually(lambda: sql("SELECT state FROM xscope.billing_jobs WHERE billing_account_id='account-worker-test'") == "idle")
            metrics_url = "http://" + console_env["XSCOPE_METRICS_ADDRESS"] + "/metrics"
            with urllib.request.urlopen(metrics_url, timeout=3) as response:
                metrics = response.read().decode()
            assert 'xscope_event_worker_state{kind="jobs",state="pending"}' in metrics
            assert 'xscope_billing_pending_oldest_seconds{state="dispatched"}' in metrics
            assert 'xscope_billing_pending_oldest_seconds{state="reserved"}' in metrics
            assert sql("SELECT state,settled_microunits IS NULL FROM xscope.billing_reservations WHERE id='worker-held'") == "dispatched|t"
            print("PASS actual background consumer drains jobs and exports bounded backlog metrics; existing uncertain hold remains frozen", flush=True)
            verify_payments(api, sql, setup, root, runfiles.Create().Rlocation(sys.argv[4]), configs, launch, port)
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
