"""Local-only operations installation and isolated restore drills; no resets.

Live configuration is captured in memory and written to Secrets via stdin.
All command stderr is captured: never disclose credentials or private backups.
"""
import argparse
import base64
import copy
import json
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import sys
import hashlib
import tarfile
import tempfile
import time
import uuid
import yaml
from python.runfiles import runfiles
from observability_cluster import local_only, forward, request, eventually
from safe_scaling_local import kube, get, secret, patch, NS

OWNER = "platform.xscope.io/reliability-local"
LABEL = "app.kubernetes.io/name"
ALERT_IMAGE = "quay.io/prometheus/alertmanager:v0.28.1@sha256:27c475db5fb156cab31d5c18a4251ac7ed567746a2483ff264516437a39b15ba"


def run(command, payload=None, timeout=120, env=None):
    result = subprocess.run(command, input=payload, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=timeout, env=env)
    if result.returncode:
        raise RuntimeError("operations command failed; private output suppressed")
    return result.stdout


def apply(value):
    metadata = value["metadata"]
    existing = get(value["kind"], metadata["name"])
    if existing and existing["metadata"].get("annotations", {}).get(OWNER) != "true":
        raise RuntimeError("refusing to adopt unrelated operations resource")
    metadata.setdefault("annotations", {})[OWNER] = "true"
    kube("apply", "--server-side", "--field-manager=xscope-reliability", "-f", "-", payload=value)


def resource(kind, name, spec):
    return {"apiVersion": {"Deployment": "apps/v1", "CronJob": "batch/v1", "NetworkPolicy": "networking.k8s.io/v1"}.get(kind, "v1"),
            "kind": kind, "metadata": {"name": name, "namespace": NS}, "spec": spec}


def alert_config(url):
    return {"global": {"resolve_timeout": "5m"},
        # A notification contains one label-set; bounded receiver batches cannot
        # be silently truncated during high-cardinality alert bursts.
        "route": {"receiver": "console", "group_by": ["..."], "group_wait": "5s", "group_interval": "10s", "repeat_interval": "4h"},
        "receivers": [{"name": "console", "webhook_configs": [{"url": url, "send_resolved": True, "max_alerts": 0,
            "http_config": {"authorization": {"type": "Bearer", "credentials_file": "/etc/alertmanager/token"}, "follow_redirects": False}}]}]}


def install_alerts():
    old = get("secret", "xscope-alert-delivery")
    token = base64.b64decode(old["data"]["token"]).decode() if old else secrets.token_urlsafe(48)
    config = yaml.safe_dump(alert_config("http://control-plane:8084/internal/v1/operations/alerts"))
    secret("xscope-alert-delivery", {"token": token, "alertmanager.yml": config, "prometheus-url": "http://prometheus:9090"})
    control = get("deployment", "control-plane")
    container = control["spec"]["template"]["spec"]["containers"][0]
    patch("deployment", "control-plane", {"metadata": {"resourceVersion": control["metadata"]["resourceVersion"]}, "spec": {"template": {"spec": {"containers": [{"name": container["name"], "env": [
        {"name": "XSCOPE_ALERT_WEBHOOK_TOKEN", "valueFrom": {"secretKeyRef": {"name": "xscope-alert-delivery", "key": "token"}}},
        {"name": "XSCOPE_OPS_PROMETHEUS_URL", "valueFrom": {"secretKeyRef": {"name": "xscope-alert-delivery", "key": "prometheus-url"}}}]}]}}}})
    apply(resource("PersistentVolumeClaim", "alertmanager-data", {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "1Gi"}}}))
    labels = {LABEL: "alertmanager", "app.kubernetes.io/part-of": "xscope"}
    apply(resource("Deployment", "alertmanager", {"replicas": 1, "strategy": {"type": "Recreate"}, "selector": {"matchLabels": labels}, "template": {"metadata": {"labels": labels}, "spec": {
        "automountServiceAccountToken": False, "securityContext": {"runAsUser": 65534, "runAsGroup": 65534, "fsGroup": 65534},
        "containers": [{"name": "alertmanager", "image": ALERT_IMAGE,
            "args": ["--config.file=/etc/alertmanager/alertmanager.yml", "--storage.path=/alertmanager", "--cluster.listen-address=", "--data.retention=120h"],
            "ports": [{"name": "http", "containerPort": 9093}], "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}, "limits": {"cpu": "100m", "memory": "128Mi"}},
            "readinessProbe": {"httpGet": {"path": "/-/ready", "port": 9093}, "periodSeconds": 5},
            "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}},
            "volumeMounts": [{"name": "config", "mountPath": "/etc/alertmanager", "readOnly": True}, {"name": "data", "mountPath": "/alertmanager"}]}],
        "volumes": [{"name": "config", "secret": {"secretName": "xscope-alert-delivery"}}, {"name": "data", "persistentVolumeClaim": {"claimName": "alertmanager-data"}}]}}}))
    apply(resource("Service", "alertmanager", {"selector": labels, "ports": [{"name": "http", "port": 9093, "targetPort": 9093}]}))
    apply(resource("NetworkPolicy", "alertmanager-ingress", {"podSelector": {"matchLabels": labels}, "policyTypes": ["Ingress"], "ingress": [{"from": [{"podSelector": {"matchLabels": {LABEL: "prometheus"}}}], "ports": [{"port": 9093, "protocol": "TCP"}]}]}))
    apply(resource("NetworkPolicy", "alertmanager-control", {"podSelector": {"matchLabels": {LABEL: "control-plane"}}, "policyTypes": ["Ingress"], "ingress": [{"from": [{"podSelector": {"matchLabels": labels}}], "ports": [{"port": 8084, "protocol": "TCP"}]}]}))
    apply(resource("NetworkPolicy", "control-backup-monitor", {"podSelector": {"matchLabels": {LABEL: "prometheus"}}, "policyTypes": ["Ingress"], "ingress": [{"from": [{"podSelector": {"matchLabels": {LABEL: "control-plane"}}}], "ports": [{"port": 9090, "protocol": "TCP"}]}]}))
    install_state_metrics()
    update_prometheus()
    kube("rollout", "status", "deployment/control-plane", "--timeout=180s")
    kube("rollout", "status", "deployment/alertmanager", "--timeout=180s")
    print("Alertmanager and dedicated authenticated console receiver installed; no external notification target configured", flush=True)


def install_state_metrics():
    name = "xscope-state-metrics"
    labels = {LABEL: name}
    apply({"apiVersion": "v1", "kind": "ServiceAccount", "metadata": {"name": name, "namespace": NS}})
    apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "Role", "metadata": {"name": name, "namespace": NS}, "rules": [
        {"apiGroups": ["batch"], "resources": ["jobs", "cronjobs"], "verbs": ["list", "watch"]},
        {"apiGroups": [""], "resources": ["persistentvolumeclaims"], "verbs": ["list", "watch"]}]})
    apply({"apiVersion": "rbac.authorization.k8s.io/v1", "kind": "RoleBinding", "metadata": {"name": name, "namespace": NS},
        "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": "Role", "name": name}, "subjects": [{"kind": "ServiceAccount", "name": name, "namespace": NS}]})
    apply(resource("Deployment", name, {"replicas": 1, "selector": {"matchLabels": labels}, "template": {"metadata": {"labels": labels}, "spec": {
        "serviceAccountName": name, "containers": [{"name": "metrics", "image": "registry.k8s.io/kube-state-metrics/kube-state-metrics:v2.15.0@sha256:db384bf43222b066c378e77027a675d4cd9911107adba46c2922b3a55e10d6fb",
            "args": ["--resources=cronjobs,jobs,persistentvolumeclaims", "--namespaces=" + NS],
            "ports": [{"name": "http", "containerPort": 8080}, {"name": "telemetry", "containerPort": 8081}],
            "readinessProbe": {"httpGet": {"path": "/readyz", "port": 8081}},
            "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}, "limits": {"cpu": "100m", "memory": "128Mi"}},
            "securityContext": {"runAsUser": 65534, "runAsNonRoot": True, "allowPrivilegeEscalation": False, "readOnlyRootFilesystem": True, "capabilities": {"drop": ["ALL"]}}}]}}}))
    apply(resource("Service", name, {"selector": labels, "ports": [{"name": "http", "port": 8080, "targetPort": 8080}]}))
    apply(resource("NetworkPolicy", name, {"podSelector": {"matchLabels": labels}, "policyTypes": ["Ingress"], "ingress": [{"from": [{"podSelector": {"matchLabels": {LABEL: "prometheus"}}}], "ports": [{"protocol": "TCP", "port": 8080}]}]}))
    kube("rollout", "status", "deployment/" + name, "--timeout=180s")


def update_prometheus():
    deployment = get("deployment", "prometheus")
    volume = next(v for v in deployment["spec"]["template"]["spec"]["volumes"] if v["name"] == "config")
    if "secret" in volume:
        current = get("secret", volume["secret"]["secretName"])
        data = {k: base64.b64decode(v).decode() for k, v in current["data"].items()}
    else:
        data = get("configmap", volume["configMap"]["name"])["data"]
    config = yaml.safe_load(data["prometheus.yaml"] if "prometheus.yaml" in data else data["prometheus.yml"])
    key = "prometheus.yaml" if "prometheus.yaml" in data else "prometheus.yml"
    config.setdefault("alerting", {})["alertmanagers"] = [{"static_configs": [{"targets": ["alertmanager:9093"]}]}]
    # Preserve all existing scrape jobs and add only the new Alertmanager target.
    jobs = config.setdefault("scrape_configs", [])
    if not any(j.get("job_name") == "xscope-alertmanager" for j in jobs):
        jobs.append({"job_name": "xscope-alertmanager", "static_configs": [{"targets": ["alertmanager:9093"]}]})
    if not any(j.get("job_name") == "xscope-state-metrics" for j in jobs):
        jobs.append({"job_name": "xscope-state-metrics", "static_configs": [{"targets": ["xscope-state-metrics:8080"]}]})
    data[key] = yaml.safe_dump(config)
    workspace = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    data["alerts.yaml"] = (workspace / "deploy/k8s/observability/alerts.yaml").read_text()
    checked = subprocess.run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", "deployment/prometheus", "--", "promtool", "check", "rules", "/dev/stdin"], input=data["alerts.yaml"], text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if checked.returncode: raise RuntimeError("Prometheus rule validation failed")
    secret("xscope-prometheus-reliability", data)
    volumes = copy.deepcopy(deployment["spec"]["template"]["spec"]["volumes"])
    volumes = [{"name": "config", "secret": {"secretName": "xscope-prometheus-reliability"}} if v["name"] == "config" else v for v in volumes]
    kube("patch", "deployment", "prometheus", "--type=merge", "--patch-file=/dev/stdin", payload={"metadata": {"resourceVersion": deployment["metadata"]["resourceVersion"]}, "spec": {"template": {"metadata": {"annotations": {"xscope.io/reliability-rollout": uuid.uuid4().hex}}, "spec": {"volumes": volumes}}}})
    kube("rollout", "status", "deployment/prometheus", "--timeout=180s")


def backup_job(image):
    return resource("CronJob", "xscope-business-backup", {"schedule": "15 2 * * *", "timeZone": "Asia/Shanghai", "concurrencyPolicy": "Forbid", "startingDeadlineSeconds": 3600, "successfulJobsHistoryLimit": 2, "failedJobsHistoryLimit": 3,
        "jobTemplate": {"spec": {"backoffLimit": 1, "activeDeadlineSeconds": 900, "template": {"metadata": {"labels": {LABEL: "xscope-business-backup"}}, "spec": {
            "automountServiceAccountToken": False, "restartPolicy": "Never", "securityContext": {"runAsUser": 70, "runAsGroup": 70, "fsGroup": 70},
            "containers": [{"name": "backup", "image": image, "command": ["sh", "/scripts/backup.sh"],
                "envFrom": [{"secretRef": {"name": "xscope-business-backup"}}],
                "resources": {"requests": {"cpu": "10m", "memory": "32Mi"}, "limits": {"cpu": "150m", "memory": "192Mi"}},
                "securityContext": {"allowPrivilegeEscalation": False, "capabilities": {"drop": ["ALL"]}},
                "volumeMounts": [{"name": "scripts", "mountPath": "/scripts", "readOnly": True}, {"name": "backups", "mountPath": "/backups"}]}],
            "volumes": [{"name": "scripts", "secret": {"secretName": "xscope-business-backup-script"}}, {"name": "backups", "persistentVolumeClaim": {"claimName": "business-backups"}}]}}}}})


def env_value(container, key):
    item = next(v for v in container["env"] if v["name"] == key)
    if "value" in item: return item["value"]
    ref = item["valueFrom"]["secretKeyRef"]
    return base64.b64decode(get("secret", ref["name"])["data"][ref["key"]]).decode()


def install_backup():
    container = get("statefulset", "postgres")["spec"]["template"]["spec"]["containers"][0]
    # Dedicated read-only role: this Pod must not be able to read identity tables.
    old = get("secret", "xscope-business-backup")
    role = "xscope_backup_reader"
    owned = old and base64.b64decode(old["data"].get("PGUSER", "")).decode() == role
    def sql(statement):
        return run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", "-i", "statefulset/postgres", "--", "sh", "-c", 'exec psql -XqAt -U "$POSTGRES_USER" -d "$POSTGRES_DB" -v ON_ERROR_STOP=1'], statement.encode()).decode().strip()
    exists = sql("SELECT count(*) FROM pg_roles WHERE rolname='xscope_backup_reader';") == "1"
    if exists and not owned: raise RuntimeError("refusing to adopt an existing backup database role")
    password = base64.b64decode(old["data"]["PGPASSWORD"]).decode() if owned else secrets.token_urlsafe(48)
    database = env_value(container, "POSTGRES_DB")
    secret("xscope-business-backup", {"PGHOST": "postgres", "PGPORT": "5432", "PGDATABASE": database, "PGUSER": role, "PGPASSWORD": password, "PGCONNECT_TIMEOUT": "5"})
    create = "" if exists else "CREATE ROLE xscope_backup_reader LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS CONNECTION LIMIT 4 PASSWORD '" + password.replace("'", "''") + "';"
    sql("BEGIN; " + create + ' GRANT CONNECT ON DATABASE "' + database.replace('"', '""') + '" TO xscope_backup_reader; ' + r"""
GRANT USAGE ON SCHEMA xscope, public TO xscope_backup_reader;
GRANT SELECT ON ALL TABLES IN SCHEMA xscope TO xscope_backup_reader;
GRANT SELECT ON ALL SEQUENCES IN SCHEMA xscope TO xscope_backup_reader;
GRANT SELECT ON public.seaql_migrations TO xscope_backup_reader;
SELECT format('ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA xscope GRANT SELECT ON TABLES TO xscope_backup_reader', tableowner)
FROM pg_tables WHERE schemaname='xscope' GROUP BY tableowner
\gexec
SELECT format('ALTER DEFAULT PRIVILEGES FOR ROLE %I IN SCHEMA xscope GRANT SELECT ON SEQUENCES TO xscope_backup_reader', tableowner)
FROM pg_tables WHERE schemaname='xscope' GROUP BY tableowner
\gexec
COMMIT;
""")
    source = Path(runfiles.Create().Rlocation("_main/tools/business_backup.sh")).read_text()
    secret("xscope-business-backup-script", {"backup.sh": source})
    apply(resource("PersistentVolumeClaim", "business-backups", {"accessModes": ["ReadWriteOnce"], "resources": {"requests": {"storage": "10Gi"}}}))
    apply(resource("NetworkPolicy", "business-backup-postgres", {"podSelector": {"matchLabels": {LABEL: "postgres"}}, "policyTypes": ["Ingress"], "ingress": [{"from": [{"podSelector": {"matchLabels": {LABEL: "xscope-business-backup"}}}], "ports": [{"port": 5432, "protocol": "TCP"}]}]}))
    apply(backup_job(container["image"]))
    print("Nightly business-only snapshot backup installed; separate PVC, no automatic deletion, not offsite/PITR", flush=True)


def backup_now():
    cron = get("cronjob", "xscope-business-backup")
    if not cron: raise RuntimeError("install business backup before running a drill")
    # Refuse overlap with the scheduled writer on a ReadWriteOnce volume.
    jobs = json.loads(kube("get", "jobs", "-o", "json"))["items"]
    if any(j.get("status", {}).get("active", 0) and j["metadata"]["name"].startswith("xscope-business-backup") for j in jobs):
        raise RuntimeError("business backup already active; retry after completion")
    name = "xscope-business-backup-check-" + uuid.uuid4().hex[:10]
    value = {"apiVersion": "batch/v1", "kind": "Job", "metadata": {"name": name, "namespace": NS, "annotations": {OWNER: "true"}}, "spec": copy.deepcopy(cron["spec"]["jobTemplate"]["spec"])}
    kube("create", "-f", "-", payload=value)
    kube("wait", "--for=condition=complete", "job/" + name, "--timeout=900s", timeout=910)
    return name


def restore_drill():
    backup_now()
    cron = get("cronjob", "xscope-business-backup")
    image = cron["spec"]["jobTemplate"]["spec"]["template"]["spec"]["containers"][0]["image"]
    name = "xscope-backup-reader-" + uuid.uuid4().hex[:10]
    pod = resource("Pod", name, {"automountServiceAccountToken": False, "restartPolicy": "Never", "securityContext": {"runAsUser": 70, "runAsGroup": 70}, "containers": [{"name": "reader", "image": image, "command": ["sleep", "600"], "volumeMounts": [{"name": "backups", "mountPath": "/backups", "readOnly": True}], "resources": {"requests": {"cpu": "5m", "memory": "16Mi"}, "limits": {"cpu": "100m", "memory": "96Mi"}}}], "volumes": [{"name": "backups", "persistentVolumeClaim": {"claimName": "business-backups", "readOnly": True}}]})
    pod["metadata"]["annotations"] = {OWNER: "true"}
    kube("create", "-f", "-", payload=pod)
    container = None
    started = time.monotonic()
    try:
        kube("wait", "--for=condition=Ready", "pod/" + name, "--timeout=90s")
        listing = kube("exec", name, "--", "sh", "-c", "for b in /backups/backup-*; do if [ -f \"$b/COMPLETE\" ]; then printf '%s ' \"$(cat \"$b/completed-at\")\"; basename \"$b\"; fi; done")
        latest = sorted(line.split() for line in listing.splitlines())[-1][1]
        if not latest.startswith("backup-") or not latest.replace("-", "").isalnum(): raise RuntimeError("invalid backup bundle identity")
        kube("exec", name, "--", "sh", "-c", 'cd "/backups/$1" && sha256sum -c SHA256SUMS >/dev/null', "sh", latest)
        # Dump contents contain business/private data. Never enter repo or logs.
        with tempfile.TemporaryDirectory(prefix="xscope-restore-", dir="/tmp") as directory:
            os.chmod(directory, 0o700)
            private = Path(directory)
            # Stream a potentially large dump to a private file, not a giant
            # subprocess.PIPE/bytes allocation in the operator's host memory.
            archive = private / "bundle.tar"
            with os.fdopen(os.open(archive, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as output:
                copied = subprocess.run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", name, "--", "tar", "-C", "/backups/" + latest, "-cf", "-", "business.dump", "migrations.dump", "counts.tsv"], stdout=output, stderr=subprocess.PIPE, timeout=600)
                if copied.returncode: raise RuntimeError("private backup transfer failed")
            seen = set()
            with tarfile.open(archive, mode="r|") as tar:
                for member in tar:
                    if member.name not in {"business.dump", "migrations.dump", "counts.tsv"} or not member.isfile() or member.name in seen:
                        raise RuntimeError("unexpected backup archive member")
                    seen.add(member.name)
                    item = tar.extractfile(member)
                    if not item: raise RuntimeError("missing verified archive file")
                    with os.fdopen(os.open(private / member.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), "wb") as output:
                        shutil.copyfileobj(item, output, length=1024 * 1024)
            if len(seen) != 3: raise RuntimeError("incomplete backup transfer")
            # Isolated container: no host ports, no network, no source credentials.
            container = run(["docker", "run", "-d", "--rm", "--network=none", "--cpus=0.5", "--memory=384m", "--mount", f"type=bind,src={directory},dst=/backup,readonly", "-e", "POSTGRES_PASSWORD", image], env={**os.environ, "POSTGRES_PASSWORD": secrets.token_urlsafe(32)}).decode().strip()
            def ready():
                return subprocess.run(["docker", "exec", container, "pg_isready", "-U", "postgres"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0
            eventually(ready)
            time.sleep(2)
            for filename in ("business.dump", "migrations.dump"):
                run(["docker", "exec", container, "pg_restore", "-U", "postgres", "-d", "postgres", "--no-owner", "--no-acl", "--exit-on-error", "--single-transaction", "/backup/" + filename])
            def sql(statement):
                return run(["docker", "exec", "-i", container, "psql", "-XqAt", "-U", "postgres", "-v", "ON_ERROR_STOP=1"], statement.encode()).decode().strip()
            for line in (private / "counts.tsv").read_text().splitlines():
                table, count = line.split("|")
                if not table.replace("_", "").isalnum() or not count.isdigit(): raise RuntimeError("invalid backup verification manifest")
                if sql('SELECT count(*) FROM xscope."' + table + '";') != count: raise RuntimeError("restored row count differs from snapshot")
            assert sql("SELECT count(*) FROM (SELECT transaction_id FROM xscope.ledger_entries GROUP BY transaction_id HAVING sum(amount_microunits) <> 0) AS invalid;") == "0"
            assert sql("SELECT count(*) FROM pg_tables WHERE schemaname='public' AND tablename<>'seaql_migrations';") == "0"
            # Test append-only trigger behavior without removing any real row.
            sql("BEGIN; INSERT INTO xscope.operation_audits(id,actor_id,action,resource_id,payload,created_at) VALUES('restore-test','test','test','test','{}',now()); ROLLBACK;")
            result = subprocess.run(["docker", "exec", "-i", container, "psql", "-XqAt", "-U", "postgres", "-v", "ON_ERROR_STOP=1"], input=b"DELETE FROM xscope.operation_audits;", stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            assert result.returncode != 0
            print(f"PASS isolated restore: all snapshot table counts match, ledger balanced, audit protection retained, no identity tables; drill elapsed {time.monotonic()-started:.1f}s", flush=True)
            run(["docker", "stop", container]); container = None
    finally:
        if container: run(["docker", "stop", container])
        current = get("pod", name)
        if current and current["metadata"].get("annotations", {}).get(OWNER) == "true": kube("delete", "pod", name, "--wait=false")


def upgrade():
    # Build images via Bazel BEFORE this operation. A backup must succeed before
    # a deliberate Gateway drain or database migration maintenance window.
    previous = get("secret", "xscope-reliability-upgrade")
    if previous and base64.b64decode(previous["data"].get("phase", "")).decode() == "in_progress":
        raise RuntimeError("previous upgrade is incomplete; use resume-upgrade without replacing saved replica counts")
    backup_now()
    gateway = get("deployment", "gateway")
    control = get("deployment", "control-plane")
    counts = {"gateway": gateway["spec"].get("replicas", 1), "control-plane": control["spec"].get("replicas", 1)}
    secret("xscope-reliability-upgrade", {"replicas.json": json.dumps(counts), "phase": "in_progress"})
    kube("scale", "deployment/gateway", "--replicas=0")
    kube("wait", "--for=delete", "pod", "-l", LABEL + "=gateway", "--timeout=90s")
    kube("scale", "deployment/control-plane", "--replicas=0")
    kube("wait", "--for=delete", "pod", "-l", LABEL + "=control-plane", "--timeout=90s")
    for name in ["control-plane", "gateway"]:
        kube("rollout", "restart", "deployment/" + name)
        kube("scale", "deployment/" + name, "--replicas=" + str(counts[name]))
        kube("rollout", "status", "deployment/" + name, "--timeout=180s")
    secret("xscope-reliability-upgrade", {"phase": "complete"})
    print("Control plane, console and Gateway updated after backup and drain; accounts and other workloads preserved", flush=True)


def resume_upgrade():
    saved = get("secret", "xscope-reliability-upgrade")
    if not saved or base64.b64decode(saved["data"].get("phase", "")).decode() != "in_progress":
        raise RuntimeError("no incomplete, backed-up upgrade to resume")
    counts = json.loads(base64.b64decode(saved["data"]["replicas.json"]))
    for name in ["control-plane", "gateway"]:
        if not isinstance(counts[name], int) or not 0 <= counts[name] <= 100:
            raise RuntimeError("invalid saved replica count")
        kube("scale", "deployment/" + name, "--replicas=" + str(counts[name]))
        kube("rollout", "status", "deployment/" + name, "--timeout=180s")
    secret("xscope-reliability-upgrade", {"phase": "complete"})
    print("Restored saved replica counts for the previously backed-up upgrade", flush=True)


def cleanup():
    jobs = json.loads(kube("get", "jobs", "-o", "json"))["items"]
    removed = 0
    for job in jobs:
        metadata = job["metadata"]
        if not (metadata["name"].startswith("xscope-business-backup-check-") and metadata.get("annotations", {}).get(OWNER) == "true" and job.get("status", {}).get("succeeded", 0) == 1): continue
        kube("delete", "--raw", f"/apis/batch/v1/namespaces/{NS}/jobs/{metadata['name']}", "-f", "-", payload={"apiVersion": "v1", "kind": "DeleteOptions", "propagationPolicy": "Background", "preconditions": {"uid": metadata["uid"], "resourceVersion": metadata["resourceVersion"]}})
        removed += 1
    print(f"Removed {removed} completed temporary backup Jobs and their Pods; all backup bundles, failed jobs and PVCs preserved", flush=True)


def effective_environment(container, names):
    """Resolve only requested settings; return no secrets to command output."""
    result, sources = {}, {}
    def source(kind, name):
        key = (kind, name)
        if key not in sources: sources[key] = get(kind, name)
        return sources[key]
    for name in names:
        value = ""
        for ref in container.get("envFrom", []):
            prefix = ref.get("prefix", "")
            if not name.startswith(prefix): continue
            key = name[len(prefix):]
            if "secretRef" in ref:
                obj = source("secret", ref["secretRef"]["name"])
                if obj and key in obj.get("data", {}): value = base64.b64decode(obj["data"][key]).decode()
            elif "configMapRef" in ref:
                obj = source("configmap", ref["configMapRef"]["name"])
                if obj and key in obj.get("data", {}): value = obj["data"][key]
        for item in container.get("env", []):
            if item["name"] != name: continue
            if "value" in item: value = item["value"]; continue
            ref = item.get("valueFrom", {})
            if "secretKeyRef" in ref:
                selected = ref["secretKeyRef"]
                value = base64.b64decode(source("secret", selected["name"])["data"][selected["key"]]).decode()
            elif "configMapKeyRef" in ref:
                selected = ref["configMapKeyRef"]
                value = source("configmap", selected["name"])["data"][selected["key"]]
            else: raise RuntimeError("unsupported explicit authorization setting")
        result[name] = value
    return result


def auth_status():
    container = get("deployment", "control-plane")["spec"]["template"]["spec"]["containers"][0]
    values = effective_environment(container, ["XSCOPE_PLATFORM_ADMIN_SUBJECTS", "XSCOPE_BILLING_REVIEWER_SUBJECTS"])
    result = {name: len({item.strip() for item in value.split(",") if item.strip()}) for name, value in values.items()}
    print(json.dumps({"platform_admin_subject_count": result["XSCOPE_PLATFORM_ADMIN_SUBJECTS"], "billing_reviewer_subject_count": result["XSCOPE_BILLING_REVIEWER_SUBJECTS"]}), flush=True)


def append_subject(current, subject):
    if not subject or len(subject) > 512 or any(c.isspace() or c == "," for c in subject):
        raise RuntimeError("invalid resolved authorization subject")
    return ",".join(sorted({item.strip() for item in current.split(",") if item.strip()} | {subject}))


def verify_admin_processes(subject, reviewers):
    # Do not exec via deployment/: during a rollout it can select an old,
    # terminating Pod. Check every current ready Pod explicitly instead.
    pods = json.loads(kube("get", "pods", "-l", LABEL + "=control-plane", "-o", "json"))["items"]
    pods = [p for p in pods if not p["metadata"].get("deletionTimestamp")]
    if not pods or any(not any(c["type"] == "Ready" and c["status"] == "True" for c in p.get("status", {}).get("conditions", [])) for p in pods):
        return False
    probe = '''import os, sys, json, hashlib
expected = json.load(sys.stdin)
print(json.dumps({
    "admin_loaded": expected["subject"] in {x.strip() for x in os.environ.get("XSCOPE_PLATFORM_ADMIN_SUBJECTS", "").split(",")},
    "reviewers_unchanged": hashlib.sha256(os.environ.get("XSCOPE_BILLING_REVIEWER_SUBJECTS", "").encode()).hexdigest() == expected["reviewers_hash"],
    "console_auth_enabled": os.environ.get("XSCOPE_CONSOLE_AUTH") == "trusted-headers"
}))
'''
    payload = json.dumps({"subject": subject, "reviewers_hash": hashlib.sha256(reviewers.encode()).hexdigest()}).encode()
    expected = {"admin_loaded": True, "reviewers_unchanged": True, "console_auth_enabled": True}
    return all(json.loads(run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", "-i", p["metadata"]["name"], "-c", "control-plane", "--", "python3", "-c", probe], payload)) == expected for p in pods)


def grant_platform_admin():
    """Explicit operator action; stdin username must resolve to one live identity."""
    raw = sys.stdin.buffer.read(4097)
    if len(raw) > 4096: raise RuntimeError("authorization request too large")
    request_body = json.loads(raw)
    if set(request_body) != {"username"} or not isinstance(request_body["username"], str):
        raise RuntimeError("a single username is required")
    username = request_body["username"]
    if not username or len(username) > 128 or any(ord(c) < 32 for c in username):
        raise RuntimeError("invalid username")
    literal = "'" + username.replace("'", "''") + "'"
    # Administrative identity lookup only, no business/Keycloak database writes.
    statement = """SELECT COALESCE(json_agg(p.external_subject), '[]')
FROM xscope.platform_users p JOIN public.user_entity u ON u.id=p.external_subject
JOIN public.realm r ON r.id=u.realm_id
WHERE p.status='active' AND u.enabled AND r.name='xscope'
AND p.username=""" + literal + " AND u.username=" + literal + ";"
    subjects = json.loads(run(["kubectl", "--context", "docker-desktop", "-n", NS, "exec", "-i", "statefulset/postgres", "--", "sh", "-c", 'exec psql -XqAt -U "$POSTGRES_USER" -d "$POSTGRES_DB" -v ON_ERROR_STOP=1'], statement.encode()))
    if len(subjects) != 1: raise RuntimeError("username did not resolve to exactly one active, matching platform/Keycloak identity")
    subject = subjects[0]
    deployment = get("deployment", "control-plane")
    container = next(c for c in deployment["spec"]["template"]["spec"]["containers"] if c["name"] == "control-plane")
    # Avoid an authorization change accidentally deploying a different mutable image.
    image_id = run(["docker", "image", "inspect", "--format={{.Id}}", container["image"]]).decode().strip()
    pods = json.loads(kube("get", "pods", "-l", LABEL + "=control-plane", "-o", "json"))["items"]
    statuses = [s for p in pods if not p["metadata"].get("deletionTimestamp") for s in p.get("status", {}).get("containerStatuses", []) if s["name"] == container["name"]]
    if not statuses or any(not s.get("ready") or not s.get("imageID", "").endswith(image_id) for s in statuses):
        raise RuntimeError("control-plane image changed or workload not ready; no grant applied")
    names = ["XSCOPE_PLATFORM_ADMIN_SUBJECTS", "XSCOPE_BILLING_REVIEWER_SUBJECTS", "XSCOPE_CONSOLE_AUTH"]
    before = effective_environment(container, names)
    if before["XSCOPE_CONSOLE_AUTH"] != "trusted-headers": raise RuntimeError("authenticated console must be enabled")
    updated = append_subject(before[names[0]], subject)
    if subject in {v.strip() for v in before[names[0]].split(",")} and verify_admin_processes(subject, before[names[1]]):
        print("PASS requested account already authorized in all current ready control-plane Pods; no configuration changed", flush=True)
        return
    config = get("secret", "xscope-backend-runtime")
    if not config: raise RuntimeError("existing backend configuration Secret required")
    print("Validated one active identity against the existing Keycloak realm; private identifier suppressed", flush=True)
    # Merge only this field, preserving credentials, labels and annotations.
    kube("patch", "secret", "xscope-backend-runtime", "--type=merge", "--patch-file=/dev/stdin", payload={"metadata": {"resourceVersion": config["metadata"]["resourceVersion"]}, "data": {names[0]: base64.b64encode(updated.encode()).decode()}})
    env = [{"name": names[0], "value": None, "valueFrom": {"secretKeyRef": {"name": "xscope-backend-runtime", "key": names[0]}}}]
    patch("deployment", "control-plane", {"metadata": {"resourceVersion": deployment["metadata"]["resourceVersion"]}, "spec": {"template": {"metadata": {"annotations": {"xscope.io/authorization-rollout": uuid.uuid4().hex}}, "spec": {"containers": [{"name": container["name"], "env": env}]}}}})
    kube("rollout", "status", "deployment/control-plane", "--timeout=180s")
    after = effective_environment(get("deployment", "control-plane")["spec"]["template"]["spec"]["containers"][0], names)
    if after[names[0]] != updated or after[names[1]] != before[names[1]] or after[names[2]] != before[names[2]]:
        raise RuntimeError("authorization configuration changed concurrently; inspect before retrying")
    # No fabricated trusted OIDC headers and no environment/identity dumps.
    if not verify_admin_processes(subject, before[names[1]]):
        raise RuntimeError("loaded authorization state not yet verified")
    print("PASS requested account is in the loaded platform administrator allowlist; financial reviewers unchanged; control plane ready", flush=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=["alerts", "backup", "restore-drill", "upgrade", "resume-upgrade", "cleanup", "auth-status", "grant-platform-admin"])
    args = parser.parse_args(); local_only()
    {"alerts": install_alerts, "backup": install_backup, "restore-drill": restore_drill, "upgrade": upgrade, "resume-upgrade": resume_upgrade, "cleanup": cleanup, "auth-status": auth_status, "grant-platform-admin": grant_platform_admin}[args.action]()


if __name__ == "__main__":
    try: main()
    except Exception:
        raise SystemExit("Reliability operation failed; private details suppressed. Existing data was not reset.")
