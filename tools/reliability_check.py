"""Verify local alert transport and backup telemetry without financial mutations."""
from datetime import datetime, timedelta, timezone
import json
import urllib.error
import urllib.parse
import urllib.request
from observability_cluster import local_only, forward, eventually, request
from reliability_local import run, NS


def read(statement):
    return run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", "-i", "statefulset/postgres", "--", "sh", "-c", 'exec psql -XqAt -U "$POSTGRES_USER" -d "$POSTGRES_DB" -v ON_ERROR_STOP=1'], statement.encode()).decode().strip()


def main():
    local_only()
    with forward("control-plane", 8081) as base:
        for path in ("/operations/alerts", "/operations/backup", "/managed-pools", "/clusters"):
            try: request(base + "/admin/v1" + path)
            except urllib.error.HTTPError as error: assert error.code == 401
            else: raise AssertionError("anonymous operations access allowed")
    with forward("control-plane", 8084) as base:
        try: request(base + "/internal/v1/operations/alerts", {"alerts": []})
        except urllib.error.HTTPError as error: assert error.code == 401
        else: raise AssertionError("anonymous notification allowed")
    print("PASS operations and webhook authorization; no console identity fabricated", flush=True)
    start = datetime.now(timezone.utc).replace(microsecond=0)
    alert = {"labels": {"alertname": "XScopeSyntheticDeliveryCheck", "severity": "info"},
        "annotations": {"summary": "Synthetic local notification drill; no payment or inference failure"},
        "startsAt": start.isoformat(), "endsAt": (start + timedelta(minutes=5)).isoformat()}
    predicate = "name='XScopeSyntheticDeliveryCheck' AND starts_at='" + start.isoformat() + "'::timestamptz"
    with forward("alertmanager", 9093) as base:
        def send(value):
            req = urllib.request.Request(base + "/api/v2/alerts", data=json.dumps([value]).encode(), headers={"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=10) as response: assert response.status == 200
        send(alert)
        eventually(lambda: read("SELECT count(*) FROM xscope.ops_alerts WHERE " + predicate + " AND state='firing';") == "1", timeout=90)
        alert["endsAt"] = datetime.now(timezone.utc).isoformat()
        send(alert)
        eventually(lambda: read("SELECT count(*) FROM xscope.ops_alerts WHERE " + predicate + " AND state='resolved';") == "1", timeout=90)
    print("PASS Alertmanager -> dedicated authenticated webhook -> PostgreSQL inbox: firing and resolved received exactly once", flush=True)
    assert read("SELECT count(*) FROM pg_tables WHERE schemaname='public' AND tablename<>'seaql_migrations' AND has_table_privilege('xscope_backup_reader',quote_ident(schemaname)||'.'||quote_ident(tablename),'SELECT');") == "0"
    with forward("prometheus", 9090) as base:
        def healthy():
            query = urllib.parse.urlencode({"query": 'up{job=~"xscope-state-metrics|xscope-alertmanager"}'})
            values = request(base + "/api/v1/query?" + query)["data"]["result"]
            return len(values) == 2 and all(row["value"][1] == "1" for row in values)
        eventually(healthy, timeout=90)
        values = request(base + "/api/v1/query?" + urllib.parse.urlencode({"query": 'kube_cronjob_created{cronjob="xscope-business-backup"}'}))["data"]["result"]
        assert len(values) == 1
        rules = request(base + "/api/v1/rules")["data"]["groups"]
        names = {r["name"] for g in rules for r in g["rules"] if r.get("health") == "ok"}
        assert {"XScopeBusinessBackupFailed", "XScopeBusinessBackupStale", "XScopeBillingAdmissionOverload", "XScopeAlertDeliveryFailure"} <= names
    print("PASS restricted backup role cannot read identity tables; backup schedule telemetry and new Prometheus rules loaded", flush=True)


if __name__ == "__main__":
    try: main()
    except Exception:
        raise SystemExit("Reliability verification failed; private details suppressed")
