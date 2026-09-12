"""Guard the one-time WAL -> volatile deployment; never delete old evidence."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile

from observability_cluster import KUBE, local_only


def inspect_snapshot(archive):
    prior = {}
    with tarfile.open(archive) as snapshot:
        for member in snapshot:
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts or not (member.isfile() or member.isdir()):
                raise ValueError("Unexpected WAL snapshot entry; rollout aborted")
            if not member.isfile() or not member.name.endswith((".jsonl", ".seal", ".reclaimed")):
                continue
            digest = hashlib.sha256()
            with snapshot.extractfile(member) as source:
                if not member.name.endswith(".jsonl"):
                    data = source.read(256 * 1024 + 1)
                    if len(data) > 256 * 1024:
                        raise ValueError("Oversized immutable WAL metadata; rollout aborted")
                    json.loads(data)
                    prior[str(relative)] = (member.size, hashlib.sha256(data).hexdigest())
                    continue
                while line := source.readline((1 << 20) + 1):
                    if len(line) > 1 << 20 or not line.endswith(b"\n"):
                        raise ValueError("Incomplete/oversized WAL record; rollout aborted")
                    json.loads(line)
                    digest.update(line)
            prior[str(relative)] = (member.size, digest.hexdigest())
    if "events.jsonl" not in prior:
        raise ValueError("Usage WAL missing from snapshot; rollout aborted")
    return prior


def pending_bytes(path):
    total = 0
    with tarfile.open(path) as archive:
        members = {str(Path(m.name)): m for m in archive if m.isfile()}
        for name, member in members.items():
            if not name.endswith(".jsonl") or member.size == 0:
                continue
            checkpoint = members.get(name + ".checkpoint")
            if checkpoint is None:
                total += member.size
                continue
            if checkpoint.size > 8192:
                raise ValueError("Oversized checkpoint")
            row = json.load(archive.extractfile(checkpoint))
            start, end = row["start"], row["end"]
            if row["version"] != 1 or not (0 <= start < end <= member.size) or end - start > 1 << 20:
                raise ValueError("Invalid checkpoint range")
            with archive.extractfile(member) as source:
                source.seek(start)
                record = source.read(end - start)
                if not record.endswith(b"\n") or hashlib.sha256(record).hexdigest() != row["sha256"]:
                    raise ValueError("Checkpoint evidence mismatch")
            total += member.size - end
    return total


def capture(*args):
    result = subprocess.run(KUBE + list(args), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
    if result.returncode:
        raise RuntimeError("Usage cutover inspection failed; private output suppressed")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--accept-volatile-cutover", action="store_true")
    args = parser.parse_args()
    local_only()
    raw = capture("get", "deployment", "gateway", "--ignore-not-found", "-o", "json")
    if not raw.strip():
        print("New install: volatile HTTP usage mode")
        return
    deployment = json.loads(raw)
    gateway = next(c for c in deployment["spec"]["template"]["spec"]["containers"] if c["name"] == "gateway")
    mode = next((v.get("value") for v in gateway.get("env", []) if v["name"] == "XSCOPE_USAGE_MODE"), None)
    if mode == "memory":
        print("Gateway already uses volatile HTTP reporting; no WAL migration")
        return
    if not args.accept_volatile_cutover:
        raise RuntimeError("Existing Gateway uses WAL. Stop inference traffic, then use --accept-volatile-cutover; keep the existing image until the snapshot is acknowledged. No cluster changes made.")
    if deployment["spec"].get("replicas") != 1:
        raise RuntimeError("Legacy cutover requires the existing single-writer Gateway")
    os.umask(0o077)
    backup = Path(tempfile.mkdtemp(prefix="xscope-private-usage-cutover-"))
    if backup.resolve().is_relative_to(Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]).resolve()) or any((p / ".git").exists() for p in (backup, *backup.parents)):
        raise RuntimeError("Private backup must be outside all repositories")
    path = backup / "legacy-wal.tar"
    with path.open("xb") as output:
        result = subprocess.run(KUBE + ["exec", "deployment/gateway", "--", "tar", "-C", "/var/lib/xscope/usage-wal", "-cf", "-", "."],
            stdout=output, stderr=subprocess.PIPE, timeout=60)
        output.flush()
        os.fsync(output.fileno())
    print("Private legacy WAL snapshot retained outside Git: " + str(path), flush=True)
    if result.returncode:
        raise RuntimeError("Snapshot failed/changed during read; no cutover authorized")
    inspect_snapshot(path)
    pending = pending_bytes(path)
    if pending:
        raise RuntimeError("Legacy snapshot contains unacknowledged usage/admission records. Keep the existing image until delivery is resolved. PVC and private backup are retained.")
    print("Snapshot checkpoints verified. Existing PVC MUST remain retained; this is a live forensic copy, not an atomic backup. Do not resume inference until deployment completes.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, RuntimeError, subprocess.SubprocessError) as error:
        # Only our bounded messages, never tar/HTTP/kubectl/cloud response bodies.
        if isinstance(error, RuntimeError):
            raise SystemExit(str(error))
        raise SystemExit("Usage cutover validation failed; private output suppressed, evidence retained")
