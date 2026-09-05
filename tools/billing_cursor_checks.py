"""Real-PostgreSQL consumer isolation and a small, explicitly bounded load probe.

All SQL below is fixture setup, lock/failure injection or read-only assertions.
Application persistence remains SeaORM. Never import this against a live DB.
"""
import concurrent.futures
import contextlib
from datetime import datetime, timezone
import json
import subprocess
import time
import urllib.parse
import uuid


def verify(api, sql, container, setup, request):
    setup("cursor")
    feed = "/billing/projects/cursor/consumers/"
    account = sql("SELECT id FROM xscope.billing_accounts WHERE project_id='cursor'")

    def poll(name="batch", limit=100, ack=None, replica=0):
        body = {"limit": limit}
        if ack is not None:
            body["ack"] = ack
        return api("POST", feed + name + "/poll", body, replica=replica)

    def state(name="batch"):
        return sql(f"SELECT acknowledged,delivered,updated_at,xmin FROM xscope.billing_consumers WHERE billing_account_id='{account}' AND consumer='{name}'")

    def event(name):
        result = api("POST", "/billing/reservations", request(name, "cursor", tokens=1))
        assert result[0] == 200, result

    @contextlib.contextmanager
    def holding(statement, commit=False):
        tag = "cursor-lock-" + uuid.uuid4().hex
        process = subprocess.Popen(["docker", "exec", "-i", container, "psql", "-U", "postgres", "-At", "-v", "ON_ERROR_STOP=1"],
            stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            process.stdin.write(f"BEGIN; SET LOCAL application_name='{tag}'; {statement}; SELECT 1;\n")
            process.stdin.flush()
            deadline = time.monotonic() + 10
            while sql(f"SELECT count(*) FROM pg_stat_activity WHERE application_name='{tag}' AND state='idle in transaction'") != "1":
                assert process.poll() is None
                assert time.monotonic() < deadline, "fixture failed to acquire lock"
                time.sleep(0.05)
            yield
        finally:
            if process.poll() is None:
                process.stdin.write(("COMMIT" if commit else "ROLLBACK") + ";\n\\q\n")
                process.stdin.flush()
            process.stdin.close()
            process.wait(timeout=10)
            error = process.stderr.read()
            process.stderr.close()
            assert process.returncode == 0, error

    def without_waiting_for_lock(call):
        # A separate executor is managed by the caller so locks are released
        # even if an old implementation times out and leaves an RPC pending.
        future = executor.submit(call)
        result = future.result(timeout=2)
        assert result[0] == 200, result
        return result[1]

    for i in range(5):
        event(f"cursor-{i}")
    first = poll(limit=2)[1]
    assert [e["sequence"] for e in first["data"]] == [1, 2] and first["has_more"]
    before = state()
    assert poll(limit=2, replica=1)[1]["data"] == first["data"]
    assert state() == before, "redelivery must not UPDATE"
    ack = {"expected_sequence": 0, "sequence": 2}
    second = poll(limit=2, ack=ack)[1]
    assert [e["sequence"] for e in second["data"]] == [3, 4]
    assert (second["acknowledged"], second["delivered"]) == (2, 4)
    before = state()
    assert poll(limit=2, ack=ack, replica=1)[1] == second, "lost combined response must replay the same unacknowledged batch"
    assert state() == before
    assert poll(ack={"expected_sequence": 2, "sequence": 5})[0] == 409
    assert state() == before, "invalid piggyback ACK must not advance either cursor"
    third = poll(limit=2, ack={"expected_sequence": 2, "sequence": 4})[1]
    assert [e["sequence"] for e in third["data"]] == [5] and not third["has_more"]
    idle = poll(ack={"expected_sequence": 4, "sequence": 5})[1]
    assert not idle["data"] and idle["retry_after_ms"] == 1000
    before = state()
    for i in range(20):
        assert not poll(replica=i % 2)[1]["data"]
    assert state() == before, "idle polls must preserve tuple version and progress timestamp"

    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        with holding(f"SELECT 1 FROM xscope.billing_consumers WHERE billing_account_id='{account}' AND consumer='batch' FOR UPDATE"):
            assert not without_waiting_for_lock(lambda: poll())["data"]
            without_waiting_for_lock(lambda: api("POST", feed + "batch/ack", {"expected_sequence": 4, "sequence": 5}))
        event("cursor-5")
        with holding(f"SELECT id FROM xscope.billing_accounts WHERE id='{account}' FOR UPDATE"):
            assert without_waiting_for_lock(lambda: poll())["delivered"] == 6
            assert without_waiting_for_lock(lambda: api("POST", feed + "batch/ack", {"expected_sequence": 5, "sequence": 6}))["acknowledged"] == 6
        poll("independent", limit=1)
        with holding(f"SELECT 1 FROM xscope.billing_consumers WHERE billing_account_id='{account}' AND consumer='batch' FOR UPDATE"):
            assert without_waiting_for_lock(lambda: poll("independent"))["delivered"] == 6
            without_waiting_for_lock(lambda: api("POST", "/billing/reservations", request("cursor-6", "cursor", tokens=1)))
        # Account-serialized writers still prevent a cursor from crossing an
        # uncommitted sequence. Readers see committed events without waiting.
        with holding(f"SELECT id FROM xscope.billing_accounts WHERE id='{account}' FOR UPDATE; "
            f"INSERT INTO xscope.billing_events (billing_account_id,sequence,kind,aggregate_id,payload,created_at) VALUES ('{account}',8,'test.commit','test','{{}}',now())", commit=True):
            page = without_waiting_for_lock(lambda: poll())
            assert [e["sequence"] for e in page["data"]] == [7]
            assert api("POST", feed + "batch/ack", {"expected_sequence": 6, "sequence": 8})[0] == 409
        assert poll()[1]["delivered"] == 8
        with holding("LOCK TABLE xscope.ledger_entries IN ACCESS EXCLUSIVE MODE"):
            without_waiting_for_lock(lambda: api("POST", "/billing/reservations", request("cursor-postpaid-no-history", "cursor", tokens=1)))
    print("PASS empty/replay polls and duplicate ACKs are read-only; consumer progress bypasses account locks; independent consumers and money writes bypass consumer locks; no uncommitted event skipped", flush=True)

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        results = list(executor.map(lambda i: poll("first-race", replica=i % 2), range(8)))
    assert all(status == 200 for status, _ in results), results
    assert sql(f"SELECT count(*) FROM xscope.billing_consumers WHERE billing_account_id='{account}' AND consumer='first-race'") == "1"
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        results = list(executor.map(lambda n: api("POST", feed + "first-race/ack", {"expected_sequence": 0, "sequence": n}, replica=n % 2), [1, 2]))
    assert sorted(status for status, _ in results) == [200, 409], results
    print("PASS concurrent consumer registration, competing CAS ACKs, atomic combined ACK/poll and lost-response redelivery", flush=True)

    # Pending discovery is read-only, project-scoped, bounded and age-filtered.
    # Equal timestamps exercise the ID tie-breaker, not just timestamp paging.
    sql("UPDATE xscope.billing_reservations SET created_at=now()-interval '1 second' WHERE id IN ('cursor-0','cursor-1','cursor-2')")
    for i in range(3):
        assert api("POST", f"/billing/projects/cursor/reservations/cursor-{i}/dispatch", {})[0] == 200
    pending = "/billing/projects/cursor/pending-reservations"
    assert api("GET", pending)[1]["data"] == [], "new in-flight requests excluded by default age filter"
    query = {"created_before": datetime.now(timezone.utc).isoformat(), "limit": 1}
    found = []
    while True:
        result = api("GET", pending + "?" + urllib.parse.urlencode(query))
        assert result[0] == 200, result
        page = result[1]
        assert page["requires_usage_evidence"]
        found.extend(r["id"] for r in page["data"])
        if not page["next"]:
            break
        query = {**page["next"], "limit": 1}
    assert found == [f"cursor-{i}" for i in range(3)], found
    assert api("GET", pending + "?after_id=bad")[0] == 400
    assert api("GET", pending + "?limit=101")[0] == 400
    assert api("GET", pending, authenticated=False)[0] == 401
    assert api("GET", "/billing/projects/missing/pending-reservations")[0] == 404
    assert api("GET", "/billing/projects/gateway-money/pending-reservations?" + urllib.parse.urlencode(query))[1]["data"] == []
    assert sql("SELECT count(*) FROM xscope.billing_reservations WHERE project_id='cursor' AND state='dispatched'") == "3"
    print("PASS pending-reservation keyset pages, age filter, auth/project bounds; listing never releases or settles holds", flush=True)

    # A cardinality/idle-poll probe, NOT an inference throughput or production
    # capacity certification. Keep resource use bounded on the user's machine.
    sql("""BEGIN;
        INSERT INTO xscope.projects (id,tenant_id,name,created_at)
          SELECT 'scale-'||i,'test-tenant','scale',now() FROM generate_series(1,10000) i;
        INSERT INTO xscope.billing_accounts (id,tenant_id,project_id,created_at,updated_at)
          SELECT 'scale-account-'||i,'test-tenant','scale-'||i,now(),now() FROM generate_series(1,10000) i;
        INSERT INTO xscope.billing_events (billing_account_id,sequence,kind,aggregate_id,payload,created_at)
          SELECT 'scale-account-'||i,n,'test.event','test','{}',now() FROM generate_series(1,10000) i CROSS JOIN generate_series(1,10) n;
        INSERT INTO xscope.billing_consumers (billing_account_id,consumer,acknowledged,delivered,updated_at)
          SELECT 'scale-account-'||i,'probe',10,10,now() FROM generate_series(1,10000) i;
        COMMIT;
        ANALYZE xscope.billing_accounts; ANALYZE xscope.billing_events; ANALYZE xscope.billing_consumers;
    """)
    before = sql("SELECT count(DISTINCT xmin::text),min(updated_at),max(updated_at) FROM xscope.billing_consumers WHERE consumer='probe'")
    def sample(i):
        project = "scale-" + str((i * 7919) % 10000 + 1)
        start = time.monotonic()
        result = api("POST", f"/billing/projects/{project}/consumers/probe/poll", {"limit": 100}, replica=i % 2)
        assert result[0] == 200 and not result[1]["data"], result
        return (time.monotonic() - start) * 1000
    started = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        samples = sorted(executor.map(sample, range(1000)))
    elapsed = time.monotonic() - started
    assert sql("SELECT count(DISTINCT xmin::text),min(updated_at),max(updated_at) FROM xscope.billing_consumers WHERE consumer='probe'") == before
    print("PROBE " + json.dumps({"accounts": 10000, "events": 100000, "polls": 1000, "concurrency": 8,
        "observed_rps": round(1000 / elapsed, 1), "p50_ms": round(samples[499], 1), "p95_ms": round(samples[949], 1),
        "consumer_updates": 0, "production_capacity_claim": False}), flush=True)
