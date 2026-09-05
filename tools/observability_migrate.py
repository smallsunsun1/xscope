"""One-time, non-overwriting local emptyDir -> PVC migration. Run BEFORE applying new Deployments.

Exports queryable in-memory Jaeger traces to a private local JSON archive (not
automatically re-ingested). Briefly pauses Prometheus during its filesystem copy;
WAL recovery restores the copied TSDB. Samples collected after the copy until
the new Deployment starts are not included. Never touches business data.
"""
import argparse
import datetime
import json
import os
from pathlib import Path
import subprocess
import urllib.parse
import uuid

from observability_cluster import KUBE, forward, kubectl, local_only, request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backup-dir", required=True)
    args = parser.parse_args()
    local_only()
    deployment = json.loads(kubectl("get", "deployment", "prometheus", "-o", "json"))
    volumes = deployment["spec"]["template"]["spec"]["volumes"]
    if not any(v["name"] == "data" and "emptyDir" in v for v in volumes):
        raise SystemExit("Prometheus is no longer using emptyDir; refusing to overwrite persistent data.")
    os.umask(0o077)
    backup = Path(args.backup_dir).resolve()
    backup.mkdir(parents=True, exist_ok=False)
    with forward("jaeger", 16686) as base:
        traces = {}
        for service in request(base + "/api/services").get("data", []):
            query = urllib.parse.urlencode({"service": service, "limit": 1000, "lookback": "168h"})
            for trace in request(base + "/api/traces?" + query).get("data", []):
                traces[trace["traceID"]] = trace
        (backup / "jaeger-traces.json").write_text(json.dumps({"data": list(traces.values())}))
        print(f"Archived {len(traces)} queryable Jaeger traces; new spans after export are not included.", flush=True)
    with forward("prometheus", 9090) as base:
        witness = request(base + "/api/v1/query?query=up")
        (backup / "prometheus-witness.json").write_text(json.dumps(witness))
    pods = json.loads(kubectl("get", "pods", "-l", "app.kubernetes.io/name=prometheus", "-o", "json"))["items"]
    if len(pods) != 1:
        raise SystemExit("Expected exactly one old Prometheus Pod")
    pod = pods[0]["metadata"]["name"]
    image = deployment["spec"]["template"]["spec"]["containers"][0]["image"]
    archive = backup / "prometheus.tar"
    # Independent finally ensures Prometheus resumes even if tar/transport fails.
    try:
        kubectl("exec", pod, "--", "sh", "-c", "kill -STOP 1")
        with archive.open("wb") as output:
            subprocess.run(KUBE + ["exec", pod, "--", "tar", "-C", "/prometheus", "-cf", "-",
                                   "--exclude=lock", "--exclude=queries.active", "."], stdout=output, check=True, timeout=30)
    finally:
        kubectl("exec", pod, "--", "sh", "-c", "kill -CONT 1")
    helper = "prometheus-migrate-" + uuid.uuid4().hex[:10]
    spec = {"apiVersion": "v1", "kind": "Pod", "metadata": {"name": helper}, "spec": {
        "restartPolicy": "Never", "activeDeadlineSeconds": 180, "terminationGracePeriodSeconds": 1,
        "automountServiceAccountToken": False,
        "securityContext": {"runAsNonRoot": True, "runAsUser": 65534, "fsGroup": 65534},
        "containers": [{"name": "copy", "image": image, "command": ["sh", "-c", "sleep 150"],
                        "resources": {"requests": {"cpu": "5m", "memory": "16Mi"}, "limits": {"cpu": "100m", "memory": "64Mi"}},
                        "securityContext": {"allowPrivilegeEscalation": False},
                        "volumeMounts": [{"name": "data", "mountPath": "/data"}]}],
        "volumes": [{"name": "data", "persistentVolumeClaim": {"claimName": "prometheus-data"}}]}}
    subprocess.run(KUBE + ["create", "-f", "-"], input=json.dumps(spec), text=True, check=True)
    try:
        kubectl("wait", "--for=condition=Ready", "pod/" + helper, "--timeout=90s")
        contents = kubectl("exec", helper, "--", "ls", "-A", "/data").split()
        if set(contents) - {"lost+found"}:
            raise RuntimeError("PVC already contains data; refusing to overwrite it")
        with archive.open("rb") as source:
            subprocess.run(KUBE + ["exec", "-i", helper, "--", "tar", "-C", "/data", "-xf", "-"], stdin=source, check=True, timeout=30)
        print("Copied old Prometheus TSDB to prometheus-data PVC.", flush=True)
        (backup / "migration.json").write_text(json.dumps({"copied_at": datetime.datetime.now(datetime.timezone.utc).isoformat(), "old_pod": pod}))
    finally:
        kubectl("delete", "pod", helper, "--wait=true", "--timeout=60s")
    print(f"Private migration backup: {backup}")


if __name__ == "__main__":
    main()
