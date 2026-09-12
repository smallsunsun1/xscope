"""Disposable PostgreSQL fault injection; never run SQL against live accounts."""
import concurrent.futures

PREFIX = "/billing/workers/"


def verify(api, sql, setup, request):
    def call(action, payload, replica=0):
        return api("POST", PREFIX + action, payload, replica=replica)

    def claim(limit=100, replica=0):
        status, body = call("claim", {"limit": limit, "lease_seconds": 30}, replica)
        assert status == 200, body
        return body["lease"]

    assert api("POST", PREFIX + "claim", {"limit": 1, "lease_seconds": 30}, authenticated=False)[0] == 401
    assert call("claim", {"limit": 101, "lease_seconds": 30})[0] == 400
    for _ in range(100):
        lease = claim()
        if lease is None:
            break
        status, body = call("complete", lease, 1)
        assert status == 200, body
    else:
        raise AssertionError("initial audit jobs did not drain")
    before = sql("SELECT string_agg(ctid::text, ',' ORDER BY billing_account_id) FROM xscope.billing_jobs")
    for _ in range(5):
        assert claim() is None
    assert sql("SELECT string_agg(ctid::text, ',' ORDER BY billing_account_id) FROM xscope.billing_jobs") == before

    setup("worker-test", funded=True)
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        leases = list(pool.map(lambda replica: claim(replica=replica), range(2)))
    assert sum(lease is not None for lease in leases) == 1
    lease = next(lease for lease in leases if lease)
    assert lease["billing_account_id"] == "account-worker-test"
    for forged in ({**lease, "lease_token": "synthetic-invalid"},
                   {**lease, "lease_epoch": lease["lease_epoch"] + 1},
                   {**lease, "through_sequence": lease["through_sequence"] + 1}):
        assert call("complete", forged)[0] == 409

    # Effect writes and ACK must both roll back if the final job update fails.
    sql("CREATE FUNCTION fail_worker_ack() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.acknowledged > OLD.acknowledged AND NEW.billing_account_id='account-worker-test' THEN RAISE EXCEPTION 'synthetic ACK failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_worker_ack BEFORE UPDATE ON xscope.billing_jobs FOR EACH ROW EXECUTE FUNCTION fail_worker_ack()")
    assert call("complete", lease)[0] == 503
    assert sql("SELECT count(*) FROM xscope.billing_effects WHERE billing_account_id='account-worker-test'") == "0"
    assert sql("SELECT acknowledged FROM xscope.billing_jobs WHERE billing_account_id='account-worker-test'") == "0"
    sql("DROP TRIGGER fail_worker_ack ON xscope.billing_jobs; DROP FUNCTION fail_worker_ack()")
    assert call("complete", lease, 1)[0] == 200
    effect_count = sql("SELECT count(*) FROM xscope.billing_effects WHERE billing_account_id='account-worker-test'")
    assert call("complete", lease)[1]["duplicate"] is True
    assert sql("SELECT count(*) FROM xscope.billing_effects WHERE billing_account_id='account-worker-test'") == effect_count
    print("PASS event jobs: exclusive two-process claim, fencing, read-only idle polls, effect/ACK atomic rollback and replay", flush=True)

    assert api("POST", "/billing/reservations", request("worker-held", "worker-test", 1))[0] == 200
    old = claim(limit=1)
    assert api("POST", "/billing/projects/worker-test/reservations/worker-held/dispatch", {})[0] == 200
    assert call("complete", old)[0] == 200
    lease = claim()
    assert lease["expected_sequence"] == old["through_sequence"]
    # Simulate a crashed process by advancing the DB lease clock boundary.
    sql("UPDATE xscope.billing_jobs SET lease_until=clock_timestamp()-interval '1 second' WHERE billing_account_id='account-worker-test'")
    assert claim(replica=1) is None
    assert call("complete", lease)[0] == 409
    assert claim() is None  # failure backoff, no tight retry loop
    sql("UPDATE xscope.billing_jobs SET available_at=clock_timestamp()-interval '1 second' WHERE billing_account_id='account-worker-test'")
    new = claim(replica=1)
    assert new["lease_epoch"] > lease["lease_epoch"]
    assert call("complete", lease)[0] == 409
    assert call("complete", new)[0] == 200
    assert sql("SELECT state,settled_microunits IS NULL FROM xscope.billing_reservations WHERE id='worker-held'") == "dispatched|t"

    assert api("POST", "/billing/reservations", request("worker-dead", "worker-test", 1))[0] == 200
    ack = sql("SELECT acknowledged FROM xscope.billing_jobs WHERE billing_account_id='account-worker-test'")
    for attempt in range(1, 9):
        lease = claim()
        assert lease is not None
        status, result = call("fail", {"lease": lease, "reason": "dependency_unavailable"}, 1)
        assert status == 200 and result["attempts"] == attempt, result
        assert result["state"] == ("dead" if attempt == 8 else "pending")
        assert sql("SELECT acknowledged FROM xscope.billing_jobs WHERE billing_account_id='account-worker-test'") == ack
        assert claim() is None
        sql("UPDATE xscope.billing_jobs SET available_at=clock_timestamp()-interval '1 second' WHERE billing_account_id='account-worker-test'")
    assert claim() is None  # dead state cannot silently resume
    print("PASS event jobs: new high-watermark retained, expired worker fenced, bounded backoff/dead state; ambiguous money hold untouched", flush=True)


def verify_console(console, sql):
    path = "/billing/accounts/worker-test/event-worker"
    for user, status in ((None, 401), ("console-smoke-outsider", 403), ("console-smoke-member", 403)):
        assert console("GET", path, user=user)[0] == status
        assert console("POST", path + "/retry", {"reason": "synthetic repair"}, user)[0] == status
    status, body = console("GET", path, user="console-smoke-owner")
    assert status == 200 and body["state"] == "dead" and "lease_token" not in body, body
    before = sql("SELECT acknowledged FROM xscope.billing_jobs WHERE billing_account_id='account-worker-test'")
    assert console("POST", path + "/retry", {"reason": "synthetic repaired dependency"}, "console-smoke-owner")[0] == 200
    assert sql("SELECT acknowledged FROM xscope.billing_jobs WHERE billing_account_id='account-worker-test'") == before
    assert sql("SELECT count(*) FROM xscope.operation_audits WHERE action='event_job.retry' AND resource_id='account-worker-test'") == "1"
    assert console("POST", path + "/retry", {"reason": "synthetic repeat"}, "console-smoke-owner")[0] == 409
    print("PASS event dead-job recovery: owner-only, bounded redacted status, transactional audit and no skipped ACK", flush=True)
