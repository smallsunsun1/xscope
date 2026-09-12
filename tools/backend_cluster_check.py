"""Read-only local workflow check; no synthetic identity, payment or cloud mutation."""
import json
import subprocess
import urllib.error
import urllib.parse
import urllib.request

from observability_cluster import eventually, forward, kubectl, local_only, request


def main():
    local_only()
    pods = json.loads(kubectl("get", "pods", "-o", "json"))["items"]
    for component in ("gateway", "control-plane", "cluster-agent"):
        selected = [p for p in pods if p["metadata"].get("labels", {}).get("app.kubernetes.io/name") == component and not p["metadata"].get("deletionTimestamp")]
        assert len(selected) == 1
        status = selected[0]["status"]["containerStatuses"][0]
        expected = subprocess.check_output(["docker", "image", "inspect", "xscope/" + component + ":dev", "--format", "{{.Id}}"], text=True).strip()
        assert status["ready"] and status["imageID"].removeprefix("docker://") == expected
    print("PASS running Gateway/control-plane/Agent match newly loaded release image IDs and are ready")
    with forward("control-plane", 8081) as base:
        for path in ("/billing/capabilities", "/clusters", "/audit", "/billing/accounts/synthetic/event-worker", "/billing/invoices/synthetic/tax-request"):
            try:
                urllib.request.urlopen(base + "/admin/v1" + path, timeout=5)
                raise AssertionError("workflow API accepted anonymous access")
            except urllib.error.HTTPError as error:
                assert error.code == 401
        req = urllib.request.Request(base + "/payments/alipay/notify", data=b"synthetic=invalid", headers={"Content-Type": "application/x-www-form-urlencoded"})
        try:
            urllib.request.urlopen(req, timeout=5)
            raise AssertionError("disabled or invalid payment callback accepted")
        except urllib.error.HTTPError as error:
            assert error.code in (400, 503)
    with forward("prometheus", 9090) as base:
        def metric(query):
            return request(base + "/api/v1/query?" + urllib.parse.urlencode({"query": query}))["data"]["result"]
        def drained():
            values = metric('xscope_event_worker_state{component="control-plane",kind="backlog"}')
            return values and all(float(row["value"][1]) == 0 for row in values)
        eventually(drained)
        rules = [r for g in request(base + "/api/v1/rules")["data"]["groups"] for r in g["rules"]]
        assert any(r["name"] == "XScopeAvailabilityFastBurn" and r.get("health") == "ok" for r in rules)
    print("PASS authenticated workflow boundaries, rejected unconfigured callback, event backlog zero and healthy SLO rules; no payment/identity/state mutations")


if __name__ == "__main__":
    try:
        main()
    except Exception:
        raise SystemExit("Backend verification failed; private outputs suppressed") from None
