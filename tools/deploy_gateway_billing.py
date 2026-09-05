"""Activate money admission on the local single-writer Gateway, preserving WALs."""
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile

from observability_cluster import KUBE, kubectl, local_only


def inspect_snapshot(archive):
    prior = {}
    with tarfile.open(archive) as snapshot:
        for member in snapshot:
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts or not (member.isfile() or member.isdir()):
                raise ValueError("Unexpected WAL snapshot entry; rollout aborted")
            if not member.isfile() or not member.name.endswith(".jsonl"):
                continue
            digest = hashlib.sha256()
            with snapshot.extractfile(member) as source:
                while line := source.readline((1 << 20) + 1):
                    if len(line) > 1 << 20 or not line.endswith(b"\n"):
                        raise ValueError("Incomplete/oversized WAL record; rollout aborted")
                    json.loads(line)
                    digest.update(line)
            prior[str(relative)] = (member.size, digest.hexdigest())
    if "events.jsonl" not in prior:
        raise ValueError("Usage WAL missing from snapshot; rollout aborted")
    return prior


def main():
    local_only()
    deployment = json.loads(kubectl("get", "deployment", "gateway", "-o", "json"))
    if deployment["spec"].get("replicas") != 1:
        raise SystemExit("Requires the local single-replica Gateway; provision per-replica storage before scaling.")
    backup = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / ".build" / ("gateway-billing-backup-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ"))
    backup.mkdir(mode=0o700)
    archive = backup / "wal-live-snapshot.tar"
    # Retain the whole tree, including manifests/seals and future generations.
    # This is a live forensic copy, NOT an atomic database/WAL restore point.
    # tar reporting concurrent changes aborts before the rollout mutation.
    with archive.open("xb") as output:
        os.chmod(archive, 0o600)
        subprocess.run(KUBE + ["exec", "deployment/gateway", "--", "tar", "-C", "/var/lib/xscope/usage-wal", "-cf", "-", "."],
                       stdout=output, check=True, timeout=180)
        output.flush()
        os.fsync(output.fileno())
    prior = inspect_snapshot(archive)
    print("Private live WAL tree copy (not an atomic restore point): " + str(archive), flush=True)
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
    for relative, (size, digest) in prior.items():
        path = "/var/lib/xscope/usage-wal/" + relative
        after = kubectl("exec", "deployment/gateway", "--", "sh", "-c",
                        'head -c "$2" "$1" | sha256sum', "verify-wal-prefix", path, str(size))
        assert after.split()[0] == digest, "WAL history was changed: " + relative
    assert kubectl("exec", "deployment/gateway", "--", "printenv", "XSCOPE_BILLING_RESERVATIONS").strip() == "true"
    print("PASS financial admission enabled; old writer terminated and prior WAL prefixes retained. No database/identity/Redis/PVC reset.", flush=True)


if __name__ == "__main__":
    main()
