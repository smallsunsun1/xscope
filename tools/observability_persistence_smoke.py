"""Verify stored metrics, trace and a user-created Grafana dashboard survive real Pod replacement.

Local only. Requires --restart to acknowledge brief monitoring downtime. Leaves
one development inference usage record; removes its temporary Grafana dashboard.
Existing dashboards, PVCs and business data are never removed.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import time
import urllib.parse
import uuid

from observability_cluster import eventually, forward, kubectl, local_only, request


def query(base, timestamp):
    values = request(base + "/api/v1/query?" + urllib.parse.urlencode({"query": "up", "time": timestamp}))["data"]["result"]
    return sorted(values, key=lambda value: json.dumps(value["metric"], sort_keys=True))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--restart", action="store_true", required=True)
    parser.add_argument("--migration-backup")
    args = parser.parse_args()
    local_only()
    services = {"prometheus": 9090, "jaeger": 16686, "grafana": 3000}
    claim_uids = {}
    old_pods = {}
    for service in services:
        pvc = json.loads(kubectl("get", "pvc", service + "-data", "-o", "json"))
        assert pvc["status"]["phase"] == "Bound"
        claim_uids[service] = pvc["metadata"]["uid"]
        deployment = json.loads(kubectl("get", "deployment", service, "-o", "json"))
        data = next(v for v in deployment["spec"]["template"]["spec"]["volumes"] if v["name"] == "data")
        assert data["persistentVolumeClaim"]["claimName"] == service + "-data"
        old_pods[service] = kubectl("get", "pods", "-l", "app.kubernetes.io/name=" + service, "-o", "jsonpath={.items[*].metadata.uid}")

    credentials = json.loads(kubectl("get", "secret", "xscope-grafana", "-o", "json"))["data"]
    username = base64.b64decode(credentials["admin-user"]).decode()
    password = base64.b64decode(credentials["admin-password"]).decode()
    headers = {"Authorization": "Basic " + base64.b64encode(f"{username}:{password}".encode()).decode()}
    dashboard_uid = "persistence-" + uuid.uuid4().hex[:16]
    trace_id = uuid.uuid4().hex
    created_dashboard = False
    try:
        with forward("grafana", 3000) as base:
            assert request(base + "/api/health")["database"] == "ok"
            for uid in ["xscope-prometheus", "xscope-jaeger"]:
                assert request(base + f"/api/datasources/uid/{uid}/health", headers=headers)["status"] == "OK"
            assert request(base + "/api/dashboards/uid/xscope-overview", headers=headers)["dashboard"]["panels"]
            response = request(base + "/api/dashboards/db", {"dashboard": {
                "uid": dashboard_uid, "title": "Persistence smoke " + dashboard_uid,
                "schemaVersion": 39, "panels": [], "tags": ["temporary-persistence-smoke"]}, "overwrite": False}, headers)
            assert response["status"] == "success"
            created_dashboard = True
        request("http://127.0.0.1:30082/v1/chat/completions", {
            "model": "xscope-demo", "stream": False, "messages": [{"role": "user", "content": "persistence smoke"}]},
            {"Authorization": "Bearer " + os.environ.get("XSCOPE_TEST_API_KEY", "xscope-local-secret"),
             "traceparent": f"00-{trace_id}-0123456789abcdef-01"})
        with forward("jaeger", 16686) as base:
            eventually(lambda: request(base + "/api/traces/" + trace_id).get("data"))
        with forward("prometheus", 9090) as base:
            timestamp = time.time()
            metrics = query(base, timestamp)
            assert metrics, "No metrics captured before restart"
            if args.migration_backup:
                witness = json.loads((Path(args.migration_backup) / "prometheus-witness.json").read_text())["data"]["result"]
                witness.sort(key=lambda value: json.dumps(value["metric"], sort_keys=True))
                assert witness, "Migration witness was empty"
                historical = query(base, witness[0]["value"][0])
                assert historical == witness, "Old emptyDir metric samples were not retained"
                print("PASS pre-migration historical Prometheus samples retained", flush=True)
        print("Stored trace, historical metric samples and user dashboard; replacing monitoring Pods...", flush=True)
        for service in services:
            kubectl("rollout", "restart", "deployment/" + service)
            kubectl("rollout", "status", "deployment/" + service, "--timeout=150s")
            new_pods = kubectl("get", "pods", "-l", "app.kubernetes.io/name=" + service, "-o", "jsonpath={.items[*].metadata.uid}")
            assert new_pods != old_pods[service], "Pod was not actually replaced"
            pvc = json.loads(kubectl("get", "pvc", service + "-data", "-o", "json"))
            assert pvc["metadata"]["uid"] == claim_uids[service]
        with forward("prometheus", 9090) as base:
            assert query(base, timestamp) == metrics, "Historical metrics lost after Pod replacement"
        print("PASS Prometheus historical metrics survived Pod replacement", flush=True)
        with forward("jaeger", 16686) as base:
            trace = eventually(lambda: request(base + "/api/traces/" + trace_id).get("data"))
            assert trace[0]["traceID"] == trace_id
        print("PASS Jaeger persisted trace survived Pod replacement: " + trace_id, flush=True)
        with forward("grafana", 3000) as base:
            dashboard = request(base + "/api/dashboards/uid/" + dashboard_uid, headers=headers)
            assert dashboard["dashboard"]["uid"] == dashboard_uid
            for uid in ["xscope-prometheus", "xscope-jaeger"]:
                assert request(base + f"/api/datasources/uid/{uid}/health", headers=headers)["status"] == "OK"
        print("PASS Grafana user dashboard, credentials and both data sources survived Pod replacement", flush=True)
    finally:
        if created_dashboard:
            with forward("grafana", 3000) as base:
                request(base + "/api/dashboards/uid/" + dashboard_uid, headers=headers, method="DELETE")
            print("Removed only the temporary smoke dashboard; retained all PVCs and other dashboards.")


if __name__ == "__main__":
    main()
