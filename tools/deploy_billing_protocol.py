"""Exclusive local billing-writer cutover; preserve source data and private backup.

Old non-projecting binaries MUST NOT overlap with projecting writers. A failed
cutover stays fail-closed; never auto-restart an old image over initialized counters.
"""
import argparse
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import subprocess

from observability_cluster import KUBE, kubectl, local_only


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--quiesce-only", action="store_true", help="stop writers and back up; deploy-local resumes via its manifests")
    args = parser.parse_args()
    local_only()
    deployment = json.loads(kubectl("get", "deployment/control-plane", "-o", "json"))
    replicas = deployment["spec"].get("replicas", 1)
    assert replicas > 0, "control-plane is already stopped; inspect the prior cutover before resuming"
    assert any(c["image"] == "xscope/control-plane:dev" for c in deployment["spec"]["template"]["spec"]["containers"]), "unexpected image; scoped local cutover only"
    subprocess.run(["docker", "image", "inspect", "xscope/control-plane:dev"], check=True, stdout=subprocess.DEVNULL)
    backup = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / ".build" / ("billing-backup-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    backup.mkdir(mode=0o700)
    old = [p["metadata"]["name"] for p in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=control-plane", "-o", "json"))["items"]]
    print("Stopping control-plane writers before projection cutover; new inference admission will temporarily fail closed. Gateway WAL is retained.", flush=True)
    kubectl("scale", "deployment/control-plane", "--replicas=0")
    for pod in old:
        kubectl("wait", "--for=delete", "pod/" + pod, "--timeout=90s")
    assert not json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=control-plane", "-o", "json"))["items"], "writers still exist"
    path = backup / "xscope.sql"
    with path.open("xb") as output:
        os.chmod(path, 0o600)
        subprocess.run(KUBE + ["exec", "statefulset/postgres", "--", "pg_dump", "-U", "xscope", "-d", "keycloak", "-n", "xscope", "--no-owner", "--no-acl"], stdout=output, check=True)
        output.flush()
        os.fsync(output.fileno())
    print(f"Private business-schema backup saved: {path}", flush=True)
    kubectl("rollout", "restart", "deployment/control-plane")
    if args.quiesce_only:
        print("Writers stopped and backup complete. Caller must resume the newly built control-plane through its manifests.", flush=True)
        return
    kubectl("scale", "deployment/control-plane", f"--replicas={replicas}")
    print(kubectl("rollout", "status", "deployment/control-plane", "--timeout=180s"), end="", flush=True)
    tables = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        "SELECT table_name FROM information_schema.tables WHERE table_schema='xscope' AND table_name IN ('billing_reservations','billing_events','billing_consumers') ORDER BY table_name")
    assert tables.split() == ["billing_consumers", "billing_events", "billing_reservations"]
    index = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        "SELECT count(*) FROM pg_indexes WHERE schemaname='xscope' AND tablename='billing_reservations' AND indexname='reservation_pending_page_idx'")
    assert index.strip() == "1", "pending-reservation paging index missing"
    projections = kubectl("exec", "statefulset/postgres", "--", "psql", "-U", "xscope", "-d", "keycloak", "-At", "-c",
        "SELECT count(*) FROM information_schema.tables WHERE table_schema='xscope' AND table_name IN ('billing_balances','billing_key_holds','billing_month_spend')")
    assert projections.strip() == "3", "projection migration missing; do not restore old non-projecting writers"
    print("PASS exclusive cutover, additive projection tables and paging index ready. Existing financial history, identities, Redis and PVCs retained.", flush=True)


if __name__ == "__main__":
    main()
