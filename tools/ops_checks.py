"""Synthetic alert delivery, duplicate/late ordering and authorization checks."""
import concurrent.futures
import copy


def verify(api, console, sql):
    alert = {"status": "firing", "labels": {"alertname": "XScopeSyntheticCheck", "severity": "warning"},
             "annotations": {"summary": "Synthetic delivery check"}, "fingerprint": "abcdef0123456789",
             "startsAt": "2026-01-01T00:00:00Z", "endsAt": "2026-01-01T00:10:00Z"}
    payload = {"alerts": [alert], "truncatedAlerts": 0}
    path = "/operations/alerts"
    assert api("POST", path, payload, authenticated=False)[0] == 401
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as workers:
        codes = list(workers.map(lambda n: api("POST", path, payload, replica=n % 2)[0], range(8)))
    assert codes == [204] * 8
    assert console("GET", path, user="console-smoke-member")[0] == 403
    status, page = console("GET", path, user="console-smoke-owner")
    assert status == 200 and len(page["data"]) == 1
    row = page["data"][0]
    acknowledge = path + "/" + row["id"] + "/acknowledge"
    assert console("POST", acknowledge, {"reason": "investigating"}, "console-smoke-owner")[0] == 200
    assert console("POST", acknowledge, {"reason": "investigating"}, "console-smoke-owner")[0] == 200
    done = copy.deepcopy(payload)
    done["alerts"][0]["status"] = "resolved"
    assert api("POST", path, done)[0] == 204
    assert api("POST", path, payload)[0] == 204
    final = console("GET", path + "?state=resolved", user="console-smoke-owner")[1]["data"]
    assert len(final) == 1 and final[0]["acknowledged_by"] and final[0]["ends_at"]
    assert sql("SELECT count(*) FROM xscope.operation_audits WHERE action='alert.acknowledge'") == "1"
    assert api("POST", path, {**payload, "truncatedAlerts": 1})[0] == 400
    assert console("GET", path + "?after=invalid", user="console-smoke-owner")[0] == 400
    print("PASS durable alert inbox: concurrent duplicate delivery, monotonic resolution, audited ACK and admin-only access", flush=True)
