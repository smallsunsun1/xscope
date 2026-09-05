"""Additive local billing schema/control-plane rollout, with a private business backup."""
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess

from observability_cluster import KUBE, kubectl, local_only


def main():
    local_only()
    backup = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / ".build" / ("billing-backup-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    backup.mkdir(mode=0o700)
    path = backup / "xscope.sql"
    with path.open("xb") as output:
        os.chmod(path, 0o600)
        subprocess.run(KUBE + ["exec", "statefulset/postgres", "--", "pg_dump", "-U", "xscope", "-d", "keycloak", "-n", "xscope", "--no-owner", "--no-acl"], stdout=output, check=True)
        output.flush()
        os.fsync(output.fileno())
    print(f"Private business-schema backup saved: {path}", flush=True)
    old = [p["metadata"]["name"] for p in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=control-plane", "-o", "json"))["items"]]
    kubectl("rollout", "restart", "deployment/control-plane")
    print(kubectl("rollout", "status", "deployment/control-plane", "--timeout=180s"), end="", flush=True)
    for pod in old:
        kubectl("wait", "--for=delete", "pod/" + pod, "--timeout=60s")
    tables = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        "SELECT table_name FROM information_schema.tables WHERE table_schema='xscope' AND table_name IN ('billing_reservations','billing_events','billing_consumers') ORDER BY table_name")
    assert tables.split() == ["billing_consumers", "billing_events", "billing_reservations"]
    print("PASS additive tables ready and all control-plane writers updated. Gateway protocol unchanged; existing data, identities, Redis and PVCs retained.", flush=True)


if __name__ == "__main__":
    main()
