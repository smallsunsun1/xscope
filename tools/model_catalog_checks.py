"""Real PostgreSQL catalog, immutable prices and isolation checks (synthetic only)."""
import concurrent.futures


def verify(api, sql, setup, request, console_call):
    model = {"id": "catalog-fixture", "display_name": "Synthetic catalog model", "max_context_tokens": 4096,
        "price_version": "fixture-price-1", "input_per_million_tokens": {"currency": "CNY", "amount": 3000},
        "output_per_million_tokens": {"currency": "CNY", "amount": 7000}}
    path = "/model-catalog/catalog-fixture"
    value = {"expected_revision": 0, "enabled": True, "default_pool": None, "model": model}
    assert console_call("PUT", path, value, "console-smoke-member")[0] == 403
    assert api("PUT", path, value, internal=False)[0] == 200
    assert api("PUT", path, value, replica=1, internal=False)[0] == 409
    assert api("GET", "/quote?model=catalog-fixture&input_tokens=100&output_tokens=100", internal=False)[1]["maximum"]["amount"] == 1
    setup("catalog-project")
    status, key = api("POST", "/api-keys", {"id": "catalog-key", "project_id": "catalog-project", "tenant_id": "test-tenant",
        "name": "Synthetic model key", "scopes": ["completions"], "allowed_models": ["catalog-fixture"]}, internal=False)
    assert status == 201
    reserve = {**request("catalog-hold", project="catalog-project"), "api_key_id": "catalog-key", "model_id": model["id"],
        "price_version": model["price_version"], "input_token_limit": 10, "output_token_limit": 20}
    hold_path = "/billing/projects/catalog-project/reservations/catalog-hold"
    result = api("POST", "/billing/reservations", reserve)
    assert result[0] == 200 and result[1]["reserved_microunits"] == 170000, result
    assert api("POST", hold_path + "/dispatch", {})[0] == 200
    # Existing versions cannot be repurposed; the catalog CAS must roll back too.
    invalid = {**value, "expected_revision": 1, "model": {**model, "max_context_tokens": 8192}}
    assert api("PUT", path, invalid, internal=False)[0] == 409
    assert sql("SELECT revision FROM xscope.model_catalog WHERE id='catalog-fixture'") == "1"
    changed = {**value, "expected_revision": 1, "model": {**model, "price_version": "fixture-price-2",
        "input_per_million_tokens": {"currency": "CNY", "amount": 5000}}}
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        results = list(executor.map(lambda i: api("PUT", path, changed, replica=i, internal=False)[0], range(2)))
    assert sorted(results) == [200, 409]
    assert api("POST", "/billing/reservations", {**reserve, "id": "stale-price", "request_id": "stale-price"})[0] == 400
    assert api("POST", "/billing/reservations", reserve, replica=1)[1]["reserved_microunits"] == 170000
    disabled = {**changed, "expected_revision": 2, "enabled": False}
    assert api("PUT", path, disabled, internal=False)[0] == 200
    assert api("POST", "/billing/reservations", {**reserve, "id": "disabled-hold", "request_id": "disabled-hold", "price_version": "fixture-price-2"})[0] == 400
    settlement = {"input_tokens": 1, "output_tokens": 2, "latency_ms": 10, "endpoint_id": "fixture-entry", "region": "synthetic",
        "status": "succeeded"}
    status, settled = api("POST", hold_path + "/settle", settlement, replica=1)
    assert status == 200 and settled["settled_microunits"] == 17000, settled
    assert api("POST", hold_path + "/settle", settlement)[0] == 200
    assert sql("SELECT count(*) FROM xscope.model_prices WHERE model_id='catalog-fixture'") == "2"
    assert sql("SELECT sum(amount_microunits) FROM xscope.ledger_entries WHERE transaction_id IN (SELECT id FROM xscope.ledger_transactions WHERE idempotency_key='reservation:catalog-hold')") == "0"
    assert api("PUT", path, {**disabled, "expected_revision": 3, "enabled": True}, internal=False)[0] == 200
    endpoint = {"id": "catalog-entry", "model": model["id"], "revision": "fixture-revision", "address": "synthetic-entry.invalid:8085"}
    endpoint_path = "/serving-endpoints/catalog-entry"
    assert console_call("PUT", endpoint_path, {"expected_generation": 0, "endpoint": endpoint}, "console-smoke-member")[0] == 403
    assert api("PUT", endpoint_path, {"expected_generation": 0, "endpoint": endpoint}, internal=False)[1]["generation"] == 1
    assert console_call("GET", "/serving-endpoints", None, "console-smoke-member")[0] == 403
    assert api("GET", "/serving-endpoints", internal=False)[1]["data"][0]["generation"] == 1
    assert api("PUT", endpoint_path, {"expected_generation": 1, "endpoint": {**endpoint, "revision": "other-revision"}}, internal=False)[0] == 409
    snapshot = api("GET", "/gateway/snapshot", replica=1)[1]
    assert any(e["id"] == "catalog-entry" for e in snapshot["catalog"]["endpoints"])
    assert any(m["model"]["id"] == model["id"] and m["model"]["price_version"] == "fixture-price-2" for m in snapshot["catalog"]["models"])
    assert any(p["id"] == "catalog-entry" for p in api("GET", "/route-pools", internal=False)[1]["data"])
    second = {**endpoint, "id": "catalog-canary", "revision": "fixture-canary"}
    assert api("PUT", "/serving-endpoints/catalog-canary", {"expected_generation": 0, "endpoint": second}, internal=False)[0] == 200
    route = "/projects/catalog-project/models/catalog-fixture/route-policy"
    spec = {"stable_pool": "catalog-entry", "canary_pool": "catalog-canary", "canary_percent": 10,
        "headers": [{"name": "x-route-test", "value": "synthetic", "target": "canary"}]}
    assert api("PUT", route, {"expected_revision": 0, "spec": spec}, internal=False)[0] == 200
    assert console_call("POST", route + "/actions", {"action": "pause", "expected_revision": 1}, "console-smoke-member")[0] == 403
    pause = api("POST", route + "/actions", {"action": "pause", "expected_revision": 1}, internal=False)
    assert pause[0] == 200 and pause[1]["spec"]["canary_percent"] == 0 and pause[1]["spec"]["headers"] == [], pause
    assert api("POST", route + "/actions", {"action": "promote", "expected_revision": 1}, internal=False)[0] == 409
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
        results = list(executor.map(lambda i: api("POST", route + "/actions", {"action": "promote", "expected_revision": 2}, replica=i, internal=False)[0], range(2)))
    assert sorted(results) == [200, 409]
    # Rollback advances the revision; it never edits an old immutable record.
    rollback = api("POST", route + "/actions", {"action": "rollback", "expected_revision": 3, "target_revision": 1}, internal=False)
    assert rollback[0] == 200 and rollback[1]["revision"] == 4 and rollback[1]["spec"] == spec, rollback
    assert api("POST", route + "/actions", {"action": "rollback", "expected_revision": 4, "target_revision": 4}, internal=False)[0] == 400
    history = api("GET", route + "/history?limit=2", internal=False)[1]
    assert [r["operation"] for r in history["data"]] == ["rollback", "promote"] and history["next_before"] == 3
    older = api("GET", route + "/history?before=3&limit=2", internal=False)[1]
    assert [r["operation"] for r in older["data"]] == ["pause", "put"] and older["next_before"] is None
    assert api("GET", route + "/history?limit=101", internal=False)[0] == 400
    # An injected history failure cannot leave a new current route behind.
    sql("CREATE FUNCTION xscope.synthetic_fail_release() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'synthetic release failure'; END$$; CREATE TRIGGER synthetic_fail_release BEFORE INSERT ON xscope.route_revisions FOR EACH ROW EXECUTE FUNCTION xscope.synthetic_fail_release();")
    try:
        assert api("POST", route + "/actions", {"action": "pause", "expected_revision": 4}, internal=False)[0] == 503
        assert sql("SELECT revision FROM xscope.route_policies WHERE project_id='catalog-project' AND model='catalog-fixture'") == "4"
    finally:
        sql("DROP TRIGGER synthetic_fail_release ON xscope.route_revisions; DROP FUNCTION xscope.synthetic_fail_release();")
    print("PASS release operations: owner-only pause/promote/rollback, header bypass removed, concurrent CAS, bounded immutable history and atomic rollback on history failure", flush=True)
    print("PASS persistent multi-model catalog: admin-only CAS, immutable prices, stale-price denial, frozen settlement after disable, balanced replay and endpoint identity", flush=True)
