import hashlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest
import yaml
from prepare_usage_cutover import pending_bytes
from python.runfiles import runfiles

DEPLOY_SCRIPT = Path(runfiles.Create().Rlocation(sys.argv.pop(1)))
CONFIG, DATABASE, WORKLOADS, RESET = [Path(runfiles.Create().Rlocation(sys.argv.pop(1))) for _ in range(4)]


class CutoverTest(unittest.TestCase):
    def test_new_install_is_stateless_and_reset_preserves_historical_volumes(self):
        config = yaml.safe_load(CONFIG.read_text())["data"]
        self.assertFalse(any(key.startswith("XSCOPE_WAL_") or key == "XSCOPE_USAGE_WAL" for key in config))
        database = [doc for doc in yaml.safe_load_all(DATABASE.read_text()) if doc]
        self.assertFalse(any(doc["metadata"]["name"] == "gateway-usage-wal" for doc in database))
        workloads = list(yaml.safe_load_all(WORKLOADS.read_text()))
        gateway = next(doc for doc in workloads if doc and doc["metadata"]["name"] == "gateway")
        self.assertEqual(gateway["spec"]["strategy"]["type"], "RollingUpdate")
        pod = gateway["spec"]["template"]["spec"]
        self.assertFalse(pod.get("volumes"))
        container = next(c for c in pod["containers"] if c["name"] == "gateway")
        self.assertFalse(container.get("volumeMounts"))
        env = {item["name"]: item.get("value") for item in container["env"]}
        self.assertEqual(env["XSCOPE_USAGE_MODE"], "memory")
        self.assertEqual(env["XSCOPE_BILLING_RESERVATIONS"], "true")
        grace = int(config["XSCOPE_GATEWAY_GRACE_SECONDS"]) + 10 + int(config["XSCOPE_USAGE_DRAIN_SECONDS"])
        self.assertGreaterEqual(pod["terminationGracePeriodSeconds"], grace)
        self.assertEqual(container["livenessProbe"]["httpGet"]["path"], "/healthz")
        self.assertEqual(container["readinessProbe"]["httpGet"]["path"], "/readyz")
        self.assertNotIn("delete pvc", RESET.read_text())
        self.assertNotIn("gateway-usage-wal", RESET.read_text())

    def test_local_script_flags_and_drain_order(self):
        script = DEPLOY_SCRIPT.read_text()
        flags = script[script.index('case "${1:-}" in'):script.index("# rules_rust/")]
        for arguments, expected in [([], "run\n//tools:prepare_usage_cutover\n"),
            (["--accept-volatile-cutover"], "run\n//tools:prepare_usage_cutover\n--\n--accept-volatile-cutover\n"),
            (["--reset-business-data"], "")]:
            # Executes only option handling with a fake Bazel function, never
            # Docker/kubectl or the deployment itself. Covers macOS Bash nounset.
            result = subprocess.run(["bash", "-euc", 'bazel() { printf "%s\\n" "$@"; }\n' + flags, "fixture", *arguments],
                check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            self.assertEqual(result.stdout, expected)
        rejected = subprocess.run(["bash", "-euc", flags, "fixture", "--legacy-wal"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.assertNotEqual(rejected.returncode, 0)
        self.assertLess(script.index("scale deployment/gateway --replicas=0"), script.index("bazel run //tools:deploy_billing_protocol"))
        self.assertNotIn("rollout restart deployment/gateway\n", script)

    def check(self, data, checkpoint):
        with tempfile.TemporaryDirectory(prefix="xscope-cutover-test-") as temp:
            path = Path(temp) / "fixture.tar"
            with tarfile.open(path, "w") as archive:
                for name, raw in [("./events.jsonl", data), ("./events.jsonl.checkpoint", json.dumps(checkpoint).encode())]:
                    member = tarfile.TarInfo(name)
                    member.size = len(raw)
                    archive.addfile(member, io.BytesIO(raw))
            return pending_bytes(path)

    def test_requires_exact_ack_and_counts_pending_tail(self):
        line = b'{"synthetic":true}\n'
        checkpoint = {"version": 1, "start": 0, "end": len(line), "sha256": hashlib.sha256(line).hexdigest()}
        self.assertEqual(self.check(line, checkpoint), 0)
        self.assertEqual(self.check(line * 2, checkpoint), len(line))
        with self.assertRaises(ValueError):
            self.check(line, {**checkpoint, "sha256": "0" * 64})
        with self.assertRaises(ValueError):
            self.check(line, {**checkpoint, "end": len(line) + 1})


if __name__ == "__main__":
    unittest.main()
