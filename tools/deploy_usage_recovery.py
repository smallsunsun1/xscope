"""Scoped single-writer local Gateway rollout, retaining and checking WAL history."""
import hashlib
import json
from datetime import datetime, timezone
import os
from pathlib import Path

from observability_cluster import kubectl, local_only, eventually


def main():
    local_only()
    deployment = json.loads(kubectl("get", "deployment", "gateway", "-o", "json"))
    if deployment["spec"].get("replicas") != 1:
        raise SystemExit("This rollout requires the local single-replica Gateway; multireplica WAL layout needs a separate migration.")
    wal_path = "/var/lib/xscope/usage-wal/events.jsonl"
    before = kubectl("exec", "deployment/gateway", "--", "cat", wal_path)
    if before and not before.endswith("\n"):
        raise SystemExit("Existing WAL has an incomplete tail; refusing rollout without repair.")
    for line in before.splitlines():
        json.loads(line)
    backup = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / ".build" / ("usage-backup-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    backup.mkdir(mode=0o700)
    backup_file = backup / "events.jsonl"
    with backup_file.open("x") as output:
        os.chmod(backup_file, 0o600)
        output.write(before)
        output.flush()
        os.fsync(output.fileno())
    print(f"Private pre-rollout WAL backup: {backup_file} ({len(before.splitlines())} records)", flush=True)
    old = [p["metadata"]["name"] for p in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=gateway", "-o", "json"))["items"]]
    patch = {"metadata": {"resourceVersion": deployment["metadata"]["resourceVersion"]}, "spec": {"strategy": {"type": "Recreate", "rollingUpdate": None},
        "template": {"metadata": {"annotations": {"platform.xscope.io/usage-recovery-rollout": datetime.now(timezone.utc).isoformat()}}}}}
    kubectl("patch", "deployment", "gateway", "--type=merge", "-p", json.dumps(patch))
    print(kubectl("rollout", "status", "deployment/gateway", "--timeout=180s"), end="", flush=True)
    for pod in old:
        kubectl("wait", "--for=delete", "pod/" + pod, "--timeout=60s")

    def caught_up():
        # Use one snapshot of the file length; new incoming records may follow it.
        after = kubectl("exec", "deployment/gateway", "--", "cat", wal_path)
        assert after.startswith(before), "pre-existing WAL history changed"
        exists = kubectl("exec", "deployment/gateway", "--", "sh", "-c", 'if [ -f /var/lib/xscope/usage-wal/events.jsonl.checkpoint ]; then cat /var/lib/xscope/usage-wal/events.jsonl.checkpoint; fi')
        if not exists:
            return not after
        checkpoint = json.loads(exists)
        if checkpoint["end"] != len(after.encode()):
            return False
        assert hashlib.sha256(after.encode()[checkpoint["start"]:checkpoint["end"]]).hexdigest() == checkpoint["sha256"]
        return True

    eventually(caught_up, timeout=90)
    print("PASS Gateway ready, existing WAL retained, durable checkpoint caught up; no database/Redis/identity/PVC resets.", flush=True)


if __name__ == "__main__":
    main()
