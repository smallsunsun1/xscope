"""Local-cluster API -> serving/EPP -> runtime -> ledger smoke, run via Bazel.

Creates a few development inference/usage records. It never resets data,
creates API keys, changes deployments, or changes Kubernetes context.
"""
import argparse
import http.client
import json
import os
import subprocess
import time
import uuid


def kubectl(*args):
    return subprocess.check_output(["kubectl", *args], text=True, timeout=15)


def eventually(check, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(0.25)
    raise AssertionError("verification timed out")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=30082)
    args = parser.parse_args()
    if kubectl("config", "current-context").strip() != "docker-desktop":
        raise SystemExit("This smoke is scoped to docker-desktop; current context was not changed.")
    key = os.environ.get("XSCOPE_TEST_API_KEY", "xscope-local-secret")
    run_id = "pool-smoke-" + uuid.uuid4().hex
    namespace = "xscope-system"

    def request(suffix, *, stream=True, secret=key, model="xscope-demo", text="hello inference pool", headers=None):
        request_id = f"{run_id}-{suffix}"
        connection = http.client.HTTPConnection("localhost", args.port, timeout=15)
        connection.request("POST", "/v1/chat/completions", json.dumps({
            "model": model, "messages": [{"role": "user", "content": text}], "stream": stream,
        }), headers={"Authorization": f"Bearer {secret}", "Content-Type": "application/json",
                     "X-Request-Id": request_id, **(headers or {})})
        return request_id, connection, connection.getresponse()

    # kubectl 1.25 does not have --all-pods; select Pods directly for portability.
    def runtime_logs():
        return kubectl("-n", namespace, "logs", "-l", "app.kubernetes.io/name=runtime",
                       "--since=10m", "--tail=-1", "--prefix=false")

    for suffix, overrides, expected in [("bad-key", {"secret": "invalid"}, 401),
                                         ("bad-model", {"model": "forbidden"}, 403)]:
        _, connection, response = request(suffix, **overrides)
        try:
            assert response.status == expected, (suffix, response.status, response.read())
            response.read()
        finally:
            response.close()
            connection.close()
    print("PASS API key and model authorization")

    started = time.monotonic()
    stream_id, connection, response = request("stream", text="word " * 30, headers={
        # Must never be able to route to localhost or a control-plane endpoint.
        "X-Gateway-Destination-Endpoint": "127.0.0.1:1",
        "X-Envoy-Original-Dst-Host": "127.0.0.1:1",
        "X-Envoy-Retry-On": "5xx",
    })
    try:
        assert response.status == 200, (response.status, response.read())
        assert "text/event-stream" in response.getheader("Content-Type", "")
        stream_billing_id = response.getheader("x-xscope-billing-request-id", stream_id)
        first = response.readline()
        first_time = time.monotonic() - started
        data = first + response.read()
        assert time.monotonic() - started - first_time > 0.2, "response was buffered"
        frames = [line[6:] for line in data.splitlines() if line.startswith(b"data: ")]
        assert frames.pop() == b"[DONE]"
        usage = [json.loads(frame)["usage"] for frame in frames if json.loads(frame).get("usage")]
        assert len(usage) == 1, usage
    finally:
        response.close()
        connection.close()
    print("PASS incremental SSE, one final usage, [DONE], spoofed destination ignored")

    _, connection, response = request("json", stream=False)
    try:
        assert response.status == 200, (response.status, response.read())
        assert json.loads(response.read())["object"] == "chat.completion"
    finally:
        response.close()
        connection.close()
    print("PASS non-streaming completion")

    cancel_id, connection, response = request("cancel", text="word " * 200)
    try:
        assert response.status == 200, (response.status, response.read())
        cancel_billing_id = response.getheader("x-xscope-billing-request-id")
        while True:
            line = response.readline()
            assert line, "stream ended before the first content token"
            if line.startswith(b"data: {"):
                choices = json.loads(line[6:])["choices"]
                if choices and choices[0].get("delta", {}).get("content"):
                    break
    finally:
        response.close()
        connection.close()
    eventually(lambda: f"request_id={cancel_id} outcome=cancelled" in runtime_logs())
    print("PASS downstream cancellation reached runtime through Envoy/EPP")

    def serving_evidence():
        output = kubectl("-n", namespace, "logs", "deployment/inference-serving", "-c", "envoy", "--since=10m")
        for line in output.splitlines():
            if line.startswith("{"):
                record = json.loads(line)
                if record.get("request_id") == stream_id and record.get("selected_endpoint"):
                    return record["selected_endpoint"]
        return None
    selected = eventually(serving_evidence)
    pods = json.loads(kubectl("-n", namespace, "get", "pods", "-l",
                              "app.kubernetes.io/name=runtime", "-o", "json"))
    assert selected in {f"{pod['status'].get('podIP')}:8090" for pod in pods["items"]}, selected
    print(f"PASS EPP serving hop selected endpoint {selected}")

    # Identifiers are generated hex + fixed suffixes, never interpolated user input.
    def persisted():
        sql = ("SELECT request_id,status,input_tokens,output_tokens FROM xscope.usage_events "
               f"WHERE request_id IN ('{stream_billing_id}','{cancel_billing_id or cancel_id}') ORDER BY request_id")
        rows = kubectl("-n", namespace, "exec", "statefulset/postgres", "--", "psql",
                       "-U", "xscope", "-d", "keycloak", "-At", "-c", sql)
        if f"{stream_billing_id}|succeeded|" not in rows:
            return None
        if cancel_billing_id:
            state = kubectl("-n", namespace, "exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
                f"SELECT state FROM xscope.billing_reservations WHERE id='{cancel_billing_id}'").strip()
            # Runtime cancellation before final usage is ambiguous, not a zero charge.
            return rows + f"cancel reservation {cancel_billing_id}: {state}\n" if state == "dispatched" else None
        return rows if f"{cancel_id}|cancelled|" in rows else None
    print("PASS persisted usage outcomes:\n" + eventually(persisted).strip())
    def balanced_ledger():
        sql = ("SELECT COUNT(DISTINCT t.id), COUNT(e.id), SUM(e.amount_microunits) "
               "FROM xscope.ledger_transactions t JOIN xscope.ledger_entries e ON e.transaction_id=t.id "
               "JOIN xscope.usage_events u ON u.event_id=t.reference_id "
               f"WHERE u.request_id='{stream_billing_id}'")
        return kubectl("-n", namespace, "exec", "statefulset/postgres", "--", "psql",
                       "-U", "xscope", "-d", "keycloak", "-At", "-c", sql).strip() == "1|2|0"
    eventually(balanced_ledger)
    print("PASS one balanced double-entry transaction for the completed stream")
    print(f"Completed {run_id}; development usage records were added, no data reset.")


if __name__ == "__main__":
    main()
