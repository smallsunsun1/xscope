"""Verify deployed internal auth and one real inference ledger/outbox transaction.

Adds one local inference with a temporary financial hold, then verifies settlement.
Does not create consumers, mutate policies, or reset any state.
"""
import base64
import json
import os
import urllib.error
import urllib.request
import uuid

from observability_cluster import eventually, forward, kubectl, local_only


def main():
    local_only()
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
    key = os.environ.get("XSCOPE_TEST_API_KEY", "xscope-local-secret")
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
    print("PASS deployed internal auth/validation; actual Pingora/EPP inference reserved -> dispatched -> settled; one balanced ledger and transactional events.")
    print("Added one settled development usage record: " + billing_id + "; no remaining hold from this request, no policies or consumers changed.")


if __name__ == "__main__":
    main()
