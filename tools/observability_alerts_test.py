"""Rules-only deployment preserves live scrape customizations and old maps."""
import unittest
from deploy_observability_alerts import rules_only_config


class ObservabilityAlertsTest(unittest.TestCase):
    def test_preserves_non_rule_entries_and_produces_immutable_retry_identity(self):
        original = {"data": {"prometheus.yaml": "synthetic live customized discovery", "extra.txt": "keep", "alerts.yaml": "old"}}
        configured = rules_only_config(original, "new fixture rules")
        self.assertEqual(configured, rules_only_config(original, "new fixture rules"))
        self.assertEqual(configured["data"], {**original["data"], "alerts.yaml": "new fixture rules"})
        self.assertEqual(original["data"]["alerts.yaml"], "old")
        self.assertTrue(configured["immutable"])
        self.assertNotIn("annotations", configured["metadata"])
        self.assertEqual(configured["metadata"]["namespace"], "xscope-system")
        self.assertNotEqual(configured["metadata"]["name"], rules_only_config(original, "different rules")["metadata"]["name"])


if __name__ == "__main__":
    unittest.main()
