"""Verify deployed internal auth and one real inference ledger/outbox transaction.

Adds one local inference with a temporary financial hold, then verifies settlement.
Does not create consumers, mutate policies, or reset any state.
"""
import base64
import json
import os
import urllib.error
import urllib.parse
import urllib.request
import uuid

from observability_cluster import eventually, forward, kubectl, local_only, request


def main():
    local_only()
    # New financial review routes stay behind the public identity middleware.
    # Do not fabricate a console identity or create/approve live evidence.
    with forward("control-plane", 8081) as base:
        for method, path in [
            ("GET", "/billing/accounts/project-local/reservations/unknown/reviews"),
            ("GET", "/billing/accounts/project-local/reviews/unknown"),
            ("POST", "/billing/accounts/project-local/reviews/unknown/decision"),
        ]:
            req = urllib.request.Request(base + "/admin/v1" + path, method=method,
                data=None if method == "GET" else b"{}", headers={"Content-Type": "application/json"})
            try:
                urllib.request.urlopen(req, timeout=5)
                raise AssertionError("review API accepted an anonymous request")
            except urllib.error.HTTPError as error:
                assert error.code == 401, error.code
    print("PASS deployed review routes require console identity; no live evidence submitted or approved.")
    token = base64.b64decode(kubectl("get", "secret", "xscope-platform-secrets", "-o", "jsonpath={.data.internal-token}")).decode()
    with forward("control-plane", 8084) as base:
        try:
            urllib.request.urlopen(urllib.request.Request(base + "/internal/v1/billing/reservations", data=b"{}", headers={"Content-Type": "application/json"}), timeout=5)
            raise AssertionError("billing protocol accepted missing internal token")
        except urllib.error.HTTPError as error:
            assert error.code == 401
        req = urllib.request.Request(base + "/internal/v1/billing/reservations", data=b"{}", headers={"Content-Type": "application/json", "Authorization": "Bearer " + token})
        try:
            urllib.request.urlopen(req, timeout=5)
            raise AssertionError("invalid reservation accepted")
        except urllib.error.HTTPError as error:
            assert error.code == 422
    request_id = "money-outbox-" + uuid.uuid4().hex
    from observability_cluster import inference_api_key
    key = inference_api_key()
    req = urllib.request.Request("http://localhost:30082/v1/chat/completions", data=json.dumps({"model": "xscope-demo", "stream": False,
        "messages": [{"role": "user", "content": "verify transactional usage outbox"}]}).encode(), headers={"Content-Type": "application/json", "Authorization": "Bearer " + key, "X-Request-Id": request_id})
    with urllib.request.urlopen(req, timeout=15) as response:
        assert response.status == 200 and json.loads(response.read())["usage"]["completion_tokens"] > 0
        billing_id = response.headers.get("x-xscope-billing-request-id")
        assert billing_id and billing_id.startswith("req-") and billing_id != request_id
        assert all(c.isalnum() or c == "-" for c in billing_id)

    def persisted():
        result = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
            "SELECT count(DISTINCT t.id),count(DISTINCT b.sequence),sum(e.amount_microunits) "
            "FROM xscope.usage_events u JOIN xscope.ledger_transactions t ON t.reference_id=u.event_id AND t.reference_type='usage_event' "
            "JOIN xscope.ledger_entries e ON e.transaction_id=t.id JOIN xscope.billing_events b ON b.aggregate_id=t.id AND b.kind='ledger.posted' "
            f"WHERE u.request_id='{billing_id}'")
        return result.strip() == "1|1|0"

    eventually(persisted)
    row = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        "SELECT state,settled_microunits,reserved_microunits FROM xscope.billing_reservations " + f"WHERE id='{billing_id}'").strip().split("|")
    assert row[0] == "settled" and 0 < int(row[1]) <= int(row[2]), row
    kinds = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        f"SELECT kind FROM xscope.billing_events WHERE aggregate_id='{billing_id}' ORDER BY sequence").splitlines()
    assert kinds == ["reservation.created", "reservation.dispatched", "reservation.settled"], kinds
    # Read-only discovery: do not ACK consumers or resolve unknown usage.
    project = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        f"SELECT project_id FROM xscope.billing_reservations WHERE id='{billing_id}'").strip()
    assert project and all(c.isalnum() or c in "-_" for c in project)
    with forward("control-plane", 8084) as base:
        url = base + f"/internal/v1/billing/projects/{project}/pending-reservations"
        try:
            request(url)
            raise AssertionError("pending discovery accepted missing internal token")
        except urllib.error.HTTPError as error:
            assert error.code == 401
        pending = request(url + "?limit=1", headers={"Authorization": "Bearer " + token})
        assert pending["requires_usage_evidence"] and len(pending["data"]) <= 1
        assert all(row["state"] == "dispatched" and row["project_id"] == project for row in pending["data"])
        assert all(row["id"] != billing_id for row in pending["data"])
        identity = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
            f"SELECT api_key_id,to_char(occurred_at AT TIME ZONE 'UTC','YYYY-MM-01') FROM xscope.usage_events WHERE request_id='{billing_id}'").strip().split("|")
        check_url = base + f"/internal/v1/billing/projects/{project}/projections/check?" + urllib.parse.urlencode({"api_key_id": identity[0], "month": identity[1]})
        checked = request(check_url, headers={"Authorization": "Bearer " + token})
        assert checked["consistent_before"] and not checked["rebuilt"], "live projection/source mismatch"
    print("PASS live balance/held/key/month projections match source history after inference, with no repair needed.")
    print("PASS deployed authenticated bounded pending-reservation discovery; no automatic release or consumer mutation.")
    print("PASS deployed internal auth/validation; actual Pingora/EPP inference reserved -> dispatched -> settled; one balanced ledger and transactional events.")
    print("Added one settled development usage record: " + billing_id + "; no remaining hold from this request, no policies or consumers changed.")


if __name__ == "__main__":
    main()
