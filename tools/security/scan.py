"""Redacted Gitleaks scans: working tree AND all reachable Git history.

Never print findings' Match/Secret/context, even on scanner errors. Temporary
snapshots/reports are outside the workspace and removed when this process exits.
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

from python.runfiles import runfiles


def run_scan(binary, config, mode, source, report):
    command = [binary, mode, str(source), "--config", config, "--redact=100", "--no-banner",
               "--ignore-gitleaks-allow", "--report-format=json", "--report-path", str(report)]
    if mode == "git":
        command += ["--log-opts=--all"]
    process = subprocess.run(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180)
    if process.returncode not in (0, 1):
        raise RuntimeError("Gitleaks execution failed; raw output suppressed")
    findings = json.loads(report.read_text()) if report.exists() else []
    for finding in findings:
        # A filename/line/rule suffices to locate a finding without exposing it.
        print(json.dumps({"scope": mode, "file": finding["File"], "line": finding["StartLine"],
                          "rule": finding["RuleID"], "commit": finding.get("Commit", "")[:12]}))
    print(f"{mode}: {len(findings)} finding(s)", flush=True)
    return bool(findings) or process.returncode == 1


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binary")
    parser.add_argument("config")
    parser.add_argument("--working-only", action="store_true")
    args = parser.parse_args()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]).resolve()
    resolver = runfiles.Create()
    binary = resolver.Rlocation(args.binary)
    config = resolver.Rlocation(args.config)
    os.umask(0o077)
    with tempfile.TemporaryDirectory(prefix="xscope-secret-scan-") as temp:
        directory = Path(temp)
        if directory.resolve().is_relative_to(root):
            raise RuntimeError("Secret scan temporary directory must be outside the repository")
        snapshot = directory / "working"
        snapshot.mkdir(mode=0o700)
        names = subprocess.check_output(["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"], cwd=root)
        for raw in set(names.split(b"\0")) - {b""}:
            name = Path(os.fsdecode(raw))
            source = root / name
            if source.is_symlink():
                raise RuntimeError("Repository symlink encountered; review it before publication")
            if not source.exists():
                continue
            if not source.is_file() or not source.resolve().is_relative_to(root):
                raise RuntimeError("Unsafe repository entry encountered")
            target = snapshot / name
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            shutil.copyfile(source, target)
        failed = run_scan(binary, config, "dir", snapshot, directory / "working-report.json")
        if not args.working_only:
            failed |= run_scan(binary, config, "git", root, directory / "history-report.json")
        print("FAIL: publication needs review" if failed else "PASS: no scanner findings; manual review and credential rotation still apply")
        return int(failed)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError):
        print("Secret scan failed; detailed output suppressed to protect credentials", file=sys.stderr)
        sys.exit(2)
