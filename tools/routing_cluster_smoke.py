"""Local RoutePolicy API -> Pingora -> Envoy/EPP -> real echo Pod verification.

Uses an admin-only kubectl tunnel to exercise trusted-console identity (not an
OIDC login test). Creates an isolated project, retains its accounting evidence,
and revokes the test key in finally. Never changes other projects' policies.
"""
import concurrent.futures
import http.client
import json
import re
import time
import urllib.error
import urllib.request
import uuid

from observability_cluster import eventually, forward, kubectl, local_only, request


def main():
    local_only()
    run = "route-smoke-" + uuid.uuid4().hex[:12]
    project_id = run
    key_id = run + "-key"
    issued = False
    with forward("control-plane", 8081) as base:
        # Confirm that the bundled management entry/chunk is served, without
        # claiming browser interaction or visual QA.
        with urllib.request.urlopen(base + "/", timeout=10) as response:
            html = response.read().decode()
        script = re.search(r'<script[^>]+src="([^"]+)"', html).group(1)
        with urllib.request.urlopen(base + script, timeout=10) as response:
            javascript = response.read().decode()
        chunk = re.search(r'RoutingPage-[a-zA-Z0-9_-]+\.js', javascript).group(0)
        with urllib.request.urlopen(base + "/assets/" + chunk, timeout=10) as response:
            assert response.status == 200
        print("PASS serving the built routing console chunk")
        def api(method, path, payload=None, *, user="platform-admin"):
            headers = {"Content-Type": "application/json", "Idempotency-Key": uuid.uuid4().hex,
                "X-Auth-Request-User": "route-test-" + user,
                "X-Auth-Request-Preferred-Username": user}
            req = urllib.request.Request(base + "/admin/v1" + path, method=method, headers=headers,
                data=None if payload is None else json.dumps(payload).encode())
            try:
                with urllib.request.urlopen(req, timeout=10) as response:
                    return response.status, json.loads(response.read() or "null")
            except urllib.error.HTTPError as error:
                return error.code, json.loads(error.read())

        status, session = api("GET", "/session")
        assert status == 200, session
        pools = api("GET", "/route-pools")[1]["data"]
        assert {p["id"] for p in pools} >= {"demo-pool", "demo-pool-canary"}
        assert api("POST", "/projects", {"id": project_id, "tenant_id": "tenant-local", "name": "RoutePolicy smoke evidence"})[0] == 201
        policy_path = f"/projects/{project_id}/models/xscope-demo/route-policy"
        spec = {"stable_pool": "demo-pool", "canary_pool": "demo-pool-canary", "canary_percent": 0,
            "headers": [{"name": "x-route-cohort", "value": "qa", "target": "canary"}]}
        try:
            status, key = api("POST", "/api-keys", {"id": key_id, "tenant_id": "tenant-local", "project_id": project_id,
                "name": "Temporary routing smoke", "scopes": ["chat.completions"], "allowed_models": ["xscope-demo"]})
            assert status == 201, key
            issued = True
            assert api("PUT", policy_path, {"expected_revision": 0, "spec": spec}, user="route-member")[0] == 403
            assert api("PUT", policy_path, {"expected_revision": 0, "spec": {**spec, "stable_pool": "http://untrusted"}})[0] == 400
            assert api("PUT", policy_path, {"expected_revision": 0, "spec": {**spec, "headers": [{"name": "authorization", "value": "qa", "target": "canary"}]}})[0] == 400
            assert api("PUT", policy_path, {"expected_revision": 0, "spec": spec})[0] == 200
            assert api("PUT", policy_path, {"expected_revision": 0, "spec": spec})[0] == 409
            print("PASS owner-only route writes, registered pools, protected headers, create conflict")

            counter = 0
            evidence = []

            def infer(revision, pool, cohort=None):
                nonlocal counter
                counter += 1
                request_id = f"{run}-{counter}"
                connection = http.client.HTTPConnection("localhost", 30082, timeout=15)
                headers = {"Authorization": "Bearer " + key["secret"], "Content-Type": "application/json", "X-Request-Id": request_id,
                    "X-Xscope-Pool": "spoofed", "X-Gateway-Destination-Endpoint": "127.0.0.1:1"}
                if cohort:
                    headers["X-Route-Cohort"] = cohort
                connection.request("POST", "/v1/chat/completions", json.dumps({"model": "xscope-demo", "stream": True,
                    "messages": [{"role": "user", "content": "route policy verification"}]}), headers)
                response = connection.getresponse()
                try:
                    body = response.read()
                    if response.status == 401 or response.getheader("x-xscope-route-revision") != str(revision):
                        return False
                    assert response.status == 200, (response.status, body)
                    assert response.getheader("x-xscope-pool") == pool
                    assert b"[DONE]" in body
                    model_revision = response.getheader("x-xscope-model-revision")
                    assert model_revision == ("canary-echo-v1" if pool.endswith("canary") else "development")
                    evidence.append((request_id, pool, model_revision, response.getheader("x-trace-id"), response.getheader("x-xscope-billing-request-id", request_id)))
                    return True
                finally:
                    response.close()
                    connection.close()

            eventually(lambda: infer(1, "demo-pool"), timeout=30)
            assert infer(1, "demo-pool-canary", "qa")
            print("PASS default stable + exact-header canary SSE, spoofed destinations ignored")
            # Both writers race the same revision: exactly one wins in PostgreSQL.
            weighted = {**spec, "canary_percent": 100, "headers": []}
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                results = list(executor.map(lambda _: api("PUT", policy_path, {"expected_revision": 1, "spec": weighted})[0], range(2)))
            assert sorted(results) == [200, 409], results
            eventually(lambda: infer(2, "demo-pool-canary"), timeout=30)
            assert api("PUT", policy_path, {"expected_revision": 2, "spec": {**spec, "headers": []}})[0] == 200
            eventually(lambda: infer(3, "demo-pool"), timeout=30)
            print("PASS atomic concurrent update, 100% canary, versioned rollback to stable")

            def selected_endpoint(request_id, canary):
                serving = "inference-serving-canary" if canary else "inference-serving"
                logs = kubectl("logs", "deployment/" + serving, "-c", "envoy", "--since=10m")
                for line in logs.splitlines():
                    if line.startswith("{"):
                        entry = json.loads(line)
                        if entry.get("request_id") == request_id:
                            return entry.get("selected_endpoint")
                return None

            for request_id, pool, model_revision, trace_id, billing_id in evidence:
                canary = pool.endswith("canary")
                selected = eventually(lambda: selected_endpoint(request_id, canary))
                pods = json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=" + ("runtime-canary" if canary else "runtime"), "-o", "json"))
                assert selected in {p["status"].get("podIP", "") + ":8090" for p in pods["items"]}, selected
                # Generated hex identifiers only. Read-only financial evidence;
                # application writes use the SeaORM APIs above, never raw SQL.
                def persisted():
                    sql = f"SELECT endpoint_id,model_revision,status FROM xscope.usage_events WHERE request_id='{billing_id}'"
                    return kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c", sql).strip() == f"{pool}|{model_revision}|succeeded"
                eventually(persisted)
                print(f"PASS {pool} -> {selected}; usage revision={model_revision}; trace={trace_id}")
            canary_trace = next(item[3] for item in evidence if item[1].endswith("canary"))
            with forward("jaeger", 16686) as jaeger:
                def traced():
                    payload = request(jaeger + "/api/traces/" + canary_trace)
                    if not payload.get("data"):
                        return False
                    trace = payload["data"][0]
                    for span in trace["spans"]:
                        if trace["processes"][span["processID"]]["serviceName"] == "xscope-gateway":
                            tags = {tag["key"]: tag["value"] for tag in span["tags"]}
                            if tags.get("xscope.route.pool") == "demo-pool-canary" and tags.get("xscope.model.revision") == "canary-echo-v1":
                                return True
                    return False
                eventually(traced)
            with forward("prometheus", 9090) as prometheus:
                def counted():
                    payload = request(prometheus + "/api/v1/query?query=xscope_route_selections_total")
                    return any(row["metric"].get("pool") == "demo-pool-canary" and float(row["value"][1]) > 0 for row in payload["data"]["result"])
                eventually(counted)
            print("PASS canary trace attributes + Prometheus pool selection counters")
        finally:
            if issued:
                assert api("DELETE", "/api-keys/" + key_id)[0] == 204
                print(f"Revoked test API key. Kept project {project_id} and its usage/ledger evidence; other projects unchanged.")


if __name__ == "__main__":
    main()
