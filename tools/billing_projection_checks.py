"""Disposable PostgreSQL only: projections vs source truth, repair and warm paths."""
import concurrent.futures
import contextlib
from datetime import datetime, timedelta, timezone
import subprocess
import time
import urllib.parse
import uuid


def snapshot(sql):
    return sql("""SELECT jsonb_build_object(
        'a',(SELECT jsonb_agg(t ORDER BY billing_account_id) FROM xscope.billing_balances t),
        'k',(SELECT jsonb_agg(t ORDER BY api_key_id) FROM xscope.billing_key_holds t),
        'm',(SELECT jsonb_agg(t ORDER BY api_key_id,month) FROM xscope.billing_month_spend t))""")


def verify(api, sql, container, setup, request):
    period = datetime.now(timezone.utc).date().replace(day=1)
    prior = (period - timedelta(days=1)).replace(day=1)

    def check(project, month=period, rebuild=False, **kwargs):
        body = {"api_key_id": "key-" + project, "month": str(month)}
        path = f"/billing/projects/{project}/projections/"
        return api("POST", path + "rebuild", body, **kwargs) if rebuild else api("GET", path + "check?" + urllib.parse.urlencode(body), **kwargs)

    def truth():
        # Only compare initialized projections: absence deliberately triggers
        # an account-locked backfill, never an assumed zero balance.
        assert sql("""SELECT count(*) FROM xscope.billing_balances p WHERE
            balance_microunits <> COALESCE((SELECT sum(amount_microunits) FROM xscope.ledger_entries e WHERE e.billing_account_id=p.billing_account_id AND ledger_account='customer_balance'),0)
            OR held_microunits <> COALESCE((SELECT sum(reserved_microunits) FROM xscope.billing_reservations r WHERE r.billing_account_id=p.billing_account_id AND state IN ('reserved','dispatched')),0)""") == "0"
        assert sql("""SELECT count(*) FROM xscope.billing_key_holds p WHERE held_microunits <>
            COALESCE((SELECT sum(reserved_microunits) FROM xscope.billing_reservations r WHERE r.api_key_id=p.api_key_id AND state IN ('reserved','dispatched')),0)""") == "0"
        assert sql("""SELECT count(*) FROM xscope.billing_month_spend p WHERE spent_microunits <>
            COALESCE((SELECT sum(cost_microunits) FROM xscope.usage_events u WHERE u.api_key_id=p.api_key_id AND occurred_at >= p.month::timestamp AT TIME ZONE 'UTC' AND occurred_at < (p.month::timestamp+interval '1 month') AT TIME ZONE 'UTC'),0)""") == "0"

    truth()
    assert check("funded")[1]["consistent_before"]
    assert check("funded", authenticated=False)[0] == 401
    assert api("GET", "/billing/projects/budget/projections/check?api_key_id=key-funded&month=" + str(period))[0] == 404
    assert check("funded", month=period + timedelta(days=1))[0] == 400

    # Simulate upgrading an account with real historical ledger/usage/holds.
    # Delete ONLY disposable derived rows, never financial source records.
    sql("DELETE FROM xscope.billing_balances WHERE billing_account_id='account-funded'; DELETE FROM xscope.billing_key_holds WHERE api_key_id='key-funded'; DELETE FROM xscope.billing_month_spend WHERE api_key_id='key-funded'")
    assert not check("funded")[1]["consistent_before"]
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        results = list(executor.map(lambda i: api("POST", "/billing/reservations", request(f"projection-backfill-{i}"), replica=i%2), range(8)))
    assert sorted(s for s, _ in results) == [200] + [402] * 7, results
    winner = next(row for status, row in results if status == 200)
    assert api("POST", f"/billing/projects/funded/reservations/{winner['id']}/release", {"reason": "not_dispatched"})[0] == 200
    truth()
    assert check("funded", rebuild=True)[0] == 200
    assert check("funded")[1]["consistent_before"]

    # Drift detection/rebuild touches derived rows only and writes an outbox
    # audit record. A failed audit must roll back the complete repair.
    sql("UPDATE xscope.billing_balances SET held_microunits=123 WHERE billing_account_id='account-funded'")
    assert not check("funded")[1]["consistent_before"]
    before = snapshot(sql)
    sql("CREATE FUNCTION fail_rebuild() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='projection.rebuilt' THEN RAISE EXCEPTION 'injected repair failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_rebuild BEFORE INSERT ON xscope.billing_events FOR EACH ROW EXECUTE FUNCTION fail_rebuild()")
    assert check("funded", rebuild=True)[0] == 503
    assert snapshot(sql) == before
    sql("DROP TRIGGER fail_rebuild ON xscope.billing_events; DROP FUNCTION fail_rebuild()")
    assert check("funded", rebuild=True)[0] == 200
    assert check("funded")[1]["consistent_before"]

    # Late v1 WAL usage is booked into its original UTC month, not receipt month.
    setup("projection-late")
    event = {"schema_version": "v1", "event_id": "projection-old-usage", "request_id": "projection-old-request",
        "occurred_at": str(prior) + "T00:00:00Z", "tenant_id": "test-tenant", "project_id": "projection-late", "api_key_id": "key-projection-late",
        "model_id": "xscope-demo", "model_revision": "development", "price_version": "2026-09-01", "region": "local", "endpoint_id": "demo-pool",
        "input_tokens": 10, "output_tokens": 0, "cached_input_tokens": 0, "latency_ms": 1, "status": "succeeded"}
    result = api("POST", "/usage-events", event)
    assert result[0] == 201, result
    before = snapshot(sql)
    result = api("POST", "/usage-events", event, replica=1)
    assert result[0] == 200, result
    assert snapshot(sql) == before, "duplicate v1 usage changed projections"
    assert sql("SELECT spent_microunits FROM xscope.billing_month_spend WHERE api_key_id='key-projection-late' AND month='" + str(prior) + "'") == "10000"

    # Zero known settlement unfreezes its full hold, but creates no ledger charge.
    assert api("POST", "/billing/reservations", request("projection-zero", "projection-late", tokens=10))[0] == 200
    path = "/billing/projects/projection-late/reservations/projection-zero"
    assert api("POST", path + "/dispatch", {})[0] == 200
    zero = {"input_tokens": 0, "output_tokens": 0, "latency_ms": 1, "endpoint_id": "demo-pool", "region": "local", "status": "cancelled"}
    assert api("POST", path + "/settle", zero)[0] == 200
    before = snapshot(sql)
    assert api("POST", path + "/settle", zero, replica=1)[0] == 200
    assert snapshot(sql) == before
    assert check("projection-late")[1]["consistent_before"]
    assert check("projection-late", month=prior)[1]["expected"]["spent_microunits"] == 10000

    # Credits, refunds and their idempotent replays use the same balance writer.
    setup("projection-refund", funded=True)
    refund = {"id": "projection-refund", "payment_id": "pay-projection-refund", "amount": {"currency": "CNY", "amount": 1}, "reason": "projection regression"}
    assert api("POST", "/billing/refunds", refund, internal=False)[0] == 201
    before = snapshot(sql)
    assert api("POST", "/billing/refunds", refund, internal=False, replica=1)[0] == 201
    assert snapshot(sql) == before
    assert sql("SELECT balance_microunits FROM xscope.billing_balances WHERE billing_account_id='account-projection-refund'") == "0"

    # Arithmetic overflow fails the entire money transaction, including payment.
    assert api("POST", "/billing/orders", {"id": "projection-overflow", "project_id": "projection-refund", "tenant_id": "test-tenant", "amount": {"currency": "CNY", "amount": 1}}, internal=False)[0] == 201
    sql("UPDATE xscope.billing_balances SET balance_microunits=9223372036854775807 WHERE billing_account_id='account-projection-refund'")
    result = api("POST", "/billing/orders/projection-overflow/payments", {"id": "projection-overflow", "provider": "manual-test", "provider_reference": "projection-overflow"}, internal=False)
    assert result[0] == 500, result
    assert sql("SELECT count(*) FROM xscope.payments WHERE id='projection-overflow'") == "0"
    assert check("projection-refund", rebuild=True)[0] == 200

    # A pre-existing hold from an older month still consumes this month's budget.
    sql("UPDATE xscope.billing_reservations SET created_at='" + str(prior) + "' WHERE project_id='budget' AND state='dispatched'")
    assert api("POST", "/billing/reservations", request("projection-old-hold", "budget", tokens=400))[0] == 402

    # Warm snapshots may not SELECT either historical table. Holding their
    # ACCESS EXCLUSIVE locks turns accidental history reads into a hard timeout.
    assert api("GET", "/gateway/snapshot")[0] == 200  # initialize all current months
    @contextlib.contextmanager
    def locked(tables):
        tag = "projection-lock-" + uuid.uuid4().hex
        proc = subprocess.Popen(["docker", "exec", "-i", container, "psql", "-U", "postgres", "-At", "-v", "ON_ERROR_STOP=1"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            proc.stdin.write(f"BEGIN; SET LOCAL application_name='{tag}'; LOCK TABLE {tables} IN ACCESS EXCLUSIVE MODE;\n")
            proc.stdin.flush()
            deadline = time.monotonic()+10
            while sql(f"SELECT count(*) FROM pg_stat_activity WHERE application_name='{tag}' AND state='idle in transaction'") != "1":
                assert proc.poll() is None and time.monotonic() < deadline
                time.sleep(0.05)
            yield
        finally:
            proc.stdin.write("ROLLBACK;\n\\q\n"); proc.stdin.flush(); proc.stdin.close()
            proc.wait(timeout=10)
            assert proc.returncode == 0, proc.stderr.read()
            proc.stderr.close()
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        with locked("xscope.ledger_entries, xscope.usage_events"):
            result = executor.submit(lambda: api("GET", "/gateway/snapshot")).result(timeout=2)
            assert result[0] == 200, result
        with locked("xscope.ledger_entries"):
            result = executor.submit(lambda: api("POST", "/billing/reservations", request("projection-warm", tokens=1))).result(timeout=2)
            assert result[0] == 200, result
    truth()
    print("PASS transactional projection/source equality; concurrent lazy backfill; rollback and audited drift repair; duplicate/zero/late-month usage; old holds protect budget; warm snapshot/admission avoid historical ledger reads", flush=True)
