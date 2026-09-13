import unittest
from unittest.mock import patch
import base64
import json
from reliability_local import alert_config, backup_job, append_subject, effective_environment, verify_admin_processes


class ReliabilityConfigTest(unittest.TestCase):
    def test_verification_skips_terminating_pods_and_requires_all_checks(self):
        pods = {"items": [{"metadata": {"name": "synthetic-old", "deletionTimestamp": "synthetic"}},
                          {"metadata": {"name": "synthetic-new"}, "status": {"conditions": [{"type": "Ready", "status": "True"}]}}]}
        passed = {"admin_loaded": True, "reviewers_unchanged": True, "console_auth_enabled": True}
        with patch("reliability_local.kube", return_value=json.dumps(pods)), patch("reliability_local.run", return_value=json.dumps(passed).encode()) as process:
            self.assertTrue(verify_admin_processes("synthetic-subject", ""))
            self.assertIn("synthetic-new", process.call_args.args[0])
            self.assertNotIn("synthetic-old", process.call_args.args[0])
            process.return_value = b'{}'
            self.assertFalse(verify_admin_processes("synthetic-subject", ""))

    def test_admin_append_is_exact_idempotent_and_preserves_existing_members(self):
        self.assertEqual(append_subject("synthetic-b, synthetic-a", "synthetic-c"), "synthetic-a,synthetic-b,synthetic-c")
        self.assertEqual(append_subject("synthetic-a", "synthetic-a"), "synthetic-a")
        for value in ["", "synthetic-a,synthetic-b", "synthetic\nsubject"]:
            with self.assertRaises(RuntimeError): append_subject("", value)

    def test_authorization_env_respects_prefix_and_explicit_override(self):
        source = {"data": {"ADMIN": base64.b64encode(b"synthetic-a").decode()}}
        container = {"envFrom": [{"prefix": "TEST_", "secretRef": {"name": "synthetic-settings"}}],
                     "env": [{"name": "TEST_ADMIN", "value": "synthetic-b"}]}
        with patch("reliability_local.get", return_value=source):
            self.assertEqual(effective_environment(container, ["TEST_ADMIN"]), {"TEST_ADMIN": "synthetic-b"})
            container["env"] = []
            self.assertEqual(effective_environment(container, ["TEST_ADMIN"]), {"TEST_ADMIN": "synthetic-a"})

    def test_webhook_uses_file_token_and_does_not_follow_redirects(self):
        config = alert_config("https://control.example.invalid/internal/v1/operations/alerts")
        webhook = config["receivers"][0]["webhook_configs"][0]
        self.assertTrue(webhook["send_resolved"])
        self.assertEqual(config["route"]["group_by"], ["..."])
        self.assertFalse(webhook["http_config"]["follow_redirects"])
        self.assertIn("credentials_file", webhook["http_config"]["authorization"])
        self.assertNotIn("credentials", webhook["http_config"]["authorization"])

    def test_backup_is_bounded_nonoverlapping_and_has_no_api_identity(self):
        cron = backup_job("postgres:16-alpine")
        self.assertEqual(cron["spec"]["concurrencyPolicy"], "Forbid")
        job = cron["spec"]["jobTemplate"]["spec"]
        self.assertEqual(job["activeDeadlineSeconds"], 900)
        pod = job["template"]["spec"]
        self.assertFalse(pod["automountServiceAccountToken"])
        self.assertEqual(pod["containers"][0]["envFrom"], [{"secretRef": {"name": "xscope-business-backup"}}])
        self.assertEqual(pod["volumes"][1]["persistentVolumeClaim"]["claimName"], "business-backups")
        self.assertNotIn("gateway-usage-wal", str(cron))


if __name__ == "__main__":
    unittest.main()
