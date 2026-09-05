"""Activate money admission on the local single-writer Gateway, preserving WALs."""
from datetime import datetime, timezone
import json
import os
from pathlib import Path

from observability_cluster import kubectl, local_only


def main():
    local_only()
    deployment = json.loads(kubectl("get", "deployment", "gateway", "-o", "json"))
    if deployment["spec"].get("replicas") != 1:
        raise SystemExit("Requires the local single-replica Gateway; provision per-replica storage before scaling.")
    backup = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / ".build" / ("gateway-billing-backup-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    backup.mkdir(mode=0o700)
    prior = {}
    for name in ("events.jsonl", "events.jsonl.checkpoint", "events.jsonl.reservations.jsonl", "events.jsonl.reservations.jsonl.checkpoint"):
        path = "/var/lib/xscope/usage-wal/" + name
        contents = kubectl("exec", "deployment/gateway", "--", "sh", "-c", 'if [ -f "$1" ]; then cat "$1"; fi', "read-wal", path)
        if not contents:
            continue
        if name.endswith(".jsonl"):
            if not contents.endswith("\n"):
                raise SystemExit("Incomplete WAL tail; preserve and repair before rollout: " + name)
            for line in contents.splitlines():
                json.loads(line)
            prior[path] = contents
        target = backup / name
        with target.open("x") as output:
            os.chmod(target, 0o600)
            output.write(contents)
            output.flush()
            os.fsync(output.fileno())
    print("Private pre-rollout WAL/checkpoint snapshots: " + str(backup), flush=True)
    old = [p["metadata"]["name"] for p in json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=gateway", "-o", "json"))["items"]]
    patch = {"metadata": {"resourceVersion": deployment["metadata"]["resourceVersion"]}, "spec": {
        "strategy": {"type": "Recreate", "rollingUpdate": None}, "template": {
            "metadata": {"annotations": {"platform.xscope.io/billing-admission-rollout": datetime.now(timezone.utc).isoformat()}},
            "spec": {"containers": [{"name": "gateway", "env": [
                {"name": "XSCOPE_BILLING_RESERVATIONS", "value": "true"},
                {"name": "XSCOPE_MODEL_CONTEXT_TOKENS", "value": "32768"},
            ]}]}}}}
    kubectl("patch", "deployment", "gateway", "--type=strategic", "-p", json.dumps(patch))
    print(kubectl("rollout", "status", "deployment/gateway", "--timeout=180s"), end="", flush=True)
    for name in old:
        kubectl("wait", "--for=delete", "pod/" + name, "--timeout=60s")
    for path, before in prior.items():
        after = kubectl("exec", "deployment/gateway", "--", "cat", path)
        assert after.startswith(before), "WAL history was changed: " + path
    assert kubectl("exec", "deployment/gateway", "--", "printenv", "XSCOPE_BILLING_RESERVATIONS").strip() == "true"
    print("PASS financial admission enabled; old writer terminated and prior WAL prefixes retained. No database/identity/Redis/PVC reset.", flush=True)


if __name__ == "__main__":
    main()
