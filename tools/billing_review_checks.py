"""Disposable PostgreSQL review tests; never settle live unknown-usage records."""
import concurrent.futures
import subprocess


def verify(console, api, sql, setup, reservation):
    owner, reviewer, other = "console-smoke-owner", "finance-reviewer", "finance-reviewer-2"
    setup("review-money")
    base = "/billing/accounts/review-money"

    def held(name):
        status, result = api("POST", "/billing/reservations", reservation(name, project="review-money"))
        assert status == 200, result
        assert api("POST", f"/billing/projects/review-money/reservations/{name}/dispatch", {})[0] == 200

    def evidence(case, tokens=200):
        return {"id": case, "source": "test-runtime", "source_request_id": "provider-" + case,
            "document": f'{{"provider_request":"provider-{case}","input_tokens":{tokens},"output_tokens":0,"final":true}}',
            "explanation": "Verified final receipt against runtime request identity (test fixture)",
            "usage": {"input_tokens": tokens, "output_tokens": 0, "latency_ms": 5,
                "endpoint_id": "runtime-test", "region": "local", "status": "succeeded"}}

    def submit(name, case, tokens=200, user=owner):
        return console("POST", base + f"/reservations/{name}/reviews", evidence(case, tokens), user)

    def decide(case, sha, action="approve", user=reviewer, reason="Receipt identity and final usage verified"):
        return console("POST", base + f"/reviews/{case}/decision",
            {"evidence_sha256": sha, "action": action, "reason": reason}, user)

    def state(name):
        return api("GET", f"/billing/projects/review-money/reservations/{name}")[1]["state"]

    def snapshot():
        return sql("SELECT (SELECT count(*) FROM xscope.ledger_transactions),(SELECT count(*) FROM xscope.usage_events),"
            "(SELECT count(*) FROM xscope.billing_events),(SELECT count(*) FROM xscope.billing_review_audit),"
            "(SELECT string_agg(state,',' ORDER BY id) FROM xscope.billing_reviews),"
            "(SELECT string_agg(balance_microunits||':'||held_microunits,',' ORDER BY billing_account_id) FROM xscope.billing_balances)")

    held("review-one")
    endpoint = base + "/reservations/review-one/reviews"
    assert console("POST", endpoint, evidence("case-a"), None)[0] == 401
    for user in ("console-smoke-outsider", "console-smoke-member", reviewer):
        assert submit("review-one", "case-a", user=user)[0] == 403
    # Auth-disabled private fixture cannot be used as a financial recovery bypass.
    assert api("POST", endpoint, evidence("case-a"), internal=False)[0] == 403
    assert console("POST", endpoint, {**evidence("case-a"), "document": " "}, owner)[0] == 400
    status, submitted = submit("review-one", "case-a")
    assert status == 200 and submitted["state"] == "submitted", submitted
    sha = submitted["evidence_sha256"]
    before = snapshot()
    assert submit("review-one", "case-a")[1] == submitted
    assert submit("review-one", "case-a", tokens=201)[0] == 409
    assert snapshot() == before
    # Even an explicitly configured finance reviewer cannot approve their own evidence.
    assert decide("case-a", sha, user=owner)[0] == 403
    assert decide("case-a", sha, user="console-smoke-member")[0] == 403
    assert decide("case-a", "0" * 64)[0] == 409
    assert decide("case-a", sha, reason=" ")[0] == 400
    detail_path = base + "/reviews/case-a"
    assert console("GET", detail_path, None, "console-smoke-outsider")[0] == 403
    assert console("GET", detail_path, None, "console-smoke-member")[0] == 403
    detail = console("GET", detail_path, None, reviewer)[1]
    assert detail["evidence"] == evidence("case-a") and len(detail["audit"]) == 1
    assert console("GET", "/billing/accounts/funded/reviews/case-a", None, reviewer)[0] == 404
    # Failed audit write must roll back settlement, projections and outbox too.
    sql("CREATE FUNCTION fail_review_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.sequence=2 THEN RAISE EXCEPTION 'injected audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_review_audit BEFORE INSERT ON xscope.billing_review_audit FOR EACH ROW EXECUTE FUNCTION fail_review_audit()")
    before = snapshot()
    failed = decide("case-a", sha)
    assert failed[0] == 503, failed
    assert state("review-one") == "dispatched" and snapshot() == before
    sql("DROP TRIGGER test_review_audit ON xscope.billing_review_audit; DROP FUNCTION fail_review_audit()")
    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
        results = list(pool.map(lambda _: decide("case-a", sha), range(8)))
    assert all(status == 200 and row["state"] == "settled" for status, row in results), results
    before = snapshot()
    assert decide("case-a", sha)[0] == 200  # lost response replay
    assert decide("case-a", sha, user=other)[0] == 409
    assert decide("case-a", sha, action="reject")[0] == 409
    assert snapshot() == before and state("review-one") == "settled"
    assert sql("SELECT count(*),sum(amount_microunits) FROM xscope.ledger_entries WHERE transaction_id IN (SELECT id FROM xscope.ledger_transactions WHERE idempotency_key='reservation:review-one')") == "2|0"
    detail = console("GET", detail_path, None, reviewer)[1]
    assert [row["kind"] for row in detail["audit"]] == ["submitted", "settled"]
    assert detail["audit"][0]["actor_id"] != detail["audit"][1]["actor_id"]
    # Gateway retry after reviewed recovery must not charge twice.
    internal = "/billing/projects/review-money/reservations/review-one/settle"
    assert api("POST", internal, evidence("case-a")["usage"], replica=1)[0] == 200
    assert api("POST", internal, evidence("different", 201)["usage"], replica=1)[0] == 409
    assert snapshot() == before

    held("review-over")
    over = submit("review-over", "case-over", tokens=701)[1]
    before = snapshot()
    assert decide("case-over", over["evidence_sha256"])[0] == 409
    assert state("review-over") == "dispatched" and snapshot() == before
    assert decide("case-over", over["evidence_sha256"], action="reject")[0] == 200
    assert state("review-over") == "dispatched"  # rejection is not a refund
    # New immutable evidence can be submitted after rejection.
    revised = submit("review-over", "case-revised", tokens=0)[1]
    assert decide("case-revised", revised["evidence_sha256"])[0] == 200
    assert state("review-over") == "settled"
    assert sql("SELECT count(*) FROM xscope.ledger_transactions WHERE idempotency_key='reservation:review-over'") == "0"

    # Conflicting review cases race against each other and ordinary Gateway completion.
    held("review-race")
    first = submit("review-race", "case-race-a", tokens=100)[1]
    second = submit("review-race", "case-race-b", tokens=101)[1]
    with concurrent.futures.ThreadPoolExecutor(max_workers=3) as pool:
        tasks = [pool.submit(decide, "case-race-a", first["evidence_sha256"]),
            pool.submit(decide, "case-race-b", second["evidence_sha256"]),
            pool.submit(api, "POST", "/billing/projects/review-money/reservations/review-race/settle", evidence("gateway-race", 102)["usage"], 1)]
        statuses = sorted(task.result()[0] for task in tasks)
    assert statuses == [200, 409, 409], statuses
    assert sql("SELECT count(*) FROM xscope.usage_events WHERE request_id='req-review-race'") == "1"
    assert sql("SELECT count(*) FROM xscope.ledger_transactions WHERE idempotency_key='reservation:review-race'") == "1"
    listing = console("GET", base + "/reservations/review-race/reviews?limit=1", None, reviewer)[1]
    assert len(listing["data"]) == 1 and listing["next"] == "case-race-a" and "evidence" not in listing["data"][0]
    following = console("GET", base + "/reservations/review-race/reviews?limit=1&after_id=case-race-a", None, reviewer)[1]
    assert following["data"][0]["id"] == "case-race-b" and following["next"] is None
    assert console("GET", endpoint + "?limit=101", None, reviewer)[0] == 400
    # Ordinary SQL cannot rewrite or remove receipts / append-only audit.
    for statement in ["UPDATE xscope.billing_reviews SET evidence='{}' WHERE id='case-a'",
        "DELETE FROM xscope.billing_reviews WHERE id='case-a'", "TRUNCATE xscope.billing_reviews CASCADE",
        "UPDATE xscope.billing_review_audit SET payload='{}' WHERE review_id='case-a'",
        "DELETE FROM xscope.billing_review_audit WHERE review_id='case-a'", "TRUNCATE xscope.billing_review_audit"]:
        try:
            sql(statement)
        except subprocess.CalledProcessError:
            pass
        else:
            raise AssertionError("billing evidence mutation was accepted: " + statement)
    assert console("GET", detail_path, None, reviewer)[1] == detail
    # Volatile delivery loses only local evidence: unresolved holds are still
    # discoverable, and waivers are separate from invented zero-token usage.
    held("waiver-one")
    unresolved_path = "/billing/projects/review-money/reservations/waiver-one/unresolved"
    missing = {"event_id": "evt-missing-fixture", "reason": "usage_missing", "input_tokens": None, "output_tokens": None}
    assert api("POST", unresolved_path, missing, authenticated=False)[0] == 401
    assert api("POST", unresolved_path, {**missing, "input_tokens": 0})[0] == 400
    assert api("POST", unresolved_path, missing)[0] == 200
    before = snapshot()
    assert api("POST", unresolved_path, missing, replica=1)[0] == 200
    assert snapshot() == before
    assert api("POST", unresolved_path, {**missing, "event_id": "different"})[0] == 409

    def waiver(case):
        return {"id": case, "reason": "Pod replacement; provider receipt unrecoverable (synthetic fixture)",
            "incident_reference": "incident-fixture-1", "request_terminated": True, "platform_absorbs_loss": True}

    waiver_path = base + "/reservations/waiver-one/waivers"
    assert console("POST", waiver_path, waiver("case-waive"), "console-smoke-member")[0] == 403
    assert api("POST", waiver_path, waiver("case-waive"), internal=False)[0] == 403
    assert console("POST", waiver_path, {**waiver("case-waive"), "request_terminated": False}, reviewer)[0] == 400
    status, case = console("POST", waiver_path, waiver("case-waive"), reviewer)
    assert status == 200 and case["kind"] == "loss_waiver", case
    assert console("POST", waiver_path, waiver("case-waive"), reviewer)[1] == case
    assert decide("case-waive", case["evidence_sha256"], user=reviewer)[0] == 403
    assert state("waiver-one") == "dispatched"
    # Simulated audit write failure rolls back BOTH projection and waiver.
    sql("CREATE FUNCTION fail_waiver_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.sequence=2 THEN RAISE EXCEPTION 'injected waiver audit failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER test_waiver_audit BEFORE INSERT ON xscope.billing_review_audit FOR EACH ROW EXECUTE FUNCTION fail_waiver_audit()")
    before = snapshot()
    assert decide("case-waive", case["evidence_sha256"], user=other)[0] == 503
    assert snapshot() == before and state("waiver-one") == "dispatched"
    sql("DROP TRIGGER test_waiver_audit ON xscope.billing_review_audit; DROP FUNCTION fail_waiver_audit()")
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        results = list(pool.map(lambda _: decide("case-waive", case["evidence_sha256"], user=other), range(4)))
    assert all(status == 200 and row["state"] == "waived" for status, row in results), results
    assert state("waiver-one") == "waived"
    assert sql("SELECT settled_microunits IS NULL FROM xscope.billing_reservations WHERE id='waiver-one'") == "t"
    assert sql("SELECT count(*) FROM xscope.usage_events WHERE request_id='req-waiver-one'") == "0"
    assert sql("SELECT count(*) FROM xscope.ledger_transactions WHERE idempotency_key='reservation:waiver-one'") == "0"
    assert api("POST", "/billing/projects/review-money/reservations/waiver-one/settle", evidence("late")["usage"])[0] == 409
    assert api("POST", "/billing/projects/review-money/reservations/waiver-one/release", {"reason": "not_dispatched"})[0] == 409
    waiver_detail = console("GET", base + "/reviews/case-waive", None, reviewer)[1]
    assert [a["kind"] for a in waiver_detail["audit"]] == ["submitted", "waived"]
    assert sql("SELECT count(*) FROM xscope.billing_events WHERE aggregate_id='waiver-one' AND kind='reservation.waived'") == "1"

    # A killed admission worker can also orphan a reserved (not dispatched) hold.
    assert api("POST", "/billing/reservations", reservation("waiver-reserved", project="review-money"))[0] == 200
    reserved_path = base + "/reservations/waiver-reserved/waivers"
    case = console("POST", reserved_path, waiver("case-reserved"), reviewer)[1]
    assert decide("case-reserved", case["evidence_sha256"], user=other)[0] == 200
    assert api("POST", "/billing/projects/review-money/reservations/waiver-reserved/dispatch", {})[0] == 409

    # Ordinary settlement and operator waiver serialize on the same account.
    held("waiver-race")
    case = console("POST", base + "/reservations/waiver-race/waivers", waiver("case-waiver-race"), reviewer)[1]
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        tasks = [pool.submit(decide, "case-waiver-race", case["evidence_sha256"], user=other),
            pool.submit(api, "POST", "/billing/projects/review-money/reservations/waiver-race/settle", evidence("race-late")["usage"])]
        assert sorted(task.result()[0] for task in tasks) == [200, 409]
    month = sql("SELECT to_char(now() AT TIME ZONE 'UTC','YYYY-MM-01')")
    check = api("GET", "/billing/projects/review-money/projections/check?api_key_id=key-review-money&month=" + month)[1]
    assert check["consistent_before"], check
    print("PASS volatile unresolved receipts, operator-only two-person waivers, immutable audit, atomic rollback, NULL unknown cost, late settlement fencing, orphan reserved holds and settlement/waiver races", flush=True)
    print("PASS evidence/decision authorization, two-person review, immutable receipts/audit, zero/over-limit outcomes, rollback, concurrent approval and Gateway races, exact replay and bounded history", flush=True)
