import sys
import unittest
from pathlib import Path

import yaml


class ObservabilityConfigTest(unittest.TestCase):
    def test_private_bounded_backends_and_readonly_discovery(self):
        resources = list(yaml.safe_load_all(Path(sys.argv[1]).read_text()))
        for resource in resources:
            if resource["kind"] == "Service":
                self.assertEqual(resource["spec"].get("type", "ClusterIP"), "ClusterIP")
            if resource["kind"] == "Deployment":
                for container in resource["spec"]["template"]["spec"]["containers"]:
                    self.assertIn("@sha256:", container["image"])
                    self.assertEqual(container["resources"]["requests"]["cpu"], "20m")
                    self.assertIn("memory", container["resources"]["limits"])
            if resource["kind"] == "Role":
                self.assertEqual(resource["rules"][0]["resources"], ["pods"])
                self.assertEqual(resource["rules"][0]["verbs"], ["get", "list", "watch"])

    def test_scrapes_each_replica_and_bounds_trace_storage(self):
        prometheus = yaml.safe_load(Path(sys.argv[2]).read_text())
        for job in prometheus["scrape_configs"]:
            self.assertEqual(job["kubernetes_sd_configs"][0]["role"], "pod")
            self.assertNotIn("static_configs", job)
        jaeger = yaml.safe_load(Path(sys.argv[3]).read_text())
        self.assertEqual(jaeger["extensions"]["jaeger_storage"]["backends"]["local"]["memory"]["max_traces"], 1000)
        self.assertIn("memory_limiter", jaeger["service"]["pipelines"]["traces"]["processors"])


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
