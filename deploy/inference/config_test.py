"""Security/streaming invariants for the standalone inference serving layer."""
import sys
import unittest
from pathlib import Path

import yaml


class ServingConfigTest(unittest.TestCase):
    def test_echo_does_not_scrape_fake_gpu_metrics(self):
        echo = yaml.safe_load(Path(ECHO).read_text())
        self.assertFalse(echo["dataLayer"]["injectDefaults"])
        self.assertEqual(echo["dataLayer"]["sources"], [])
        self.assertEqual(echo["plugins"], [{"type": "active-request-scorer"}])
        vllm = yaml.safe_load(Path(VLLM).read_text())
        self.assertTrue({"queue-scorer", "kv-cache-utilization-scorer", "metrics-data-source"}.issubset(
            {p["type"] for p in vllm["plugins"]}))

    def test_bundle_contains_three_separate_upstream_crds(self):
        crds = list(yaml.safe_load_all(Path(CRDS).read_text()))
        self.assertEqual({c["metadata"]["name"] for c in crds if c}, {
            "inferencepools.inference.networking.k8s.io",
            "inferenceobjectives.llm-d.ai", "inferencemodelrewrites.llm-d.ai",
        })

    def test_streams_fail_closed_and_never_retry_inference(self):
        config = yaml.safe_load(Path(ENVOY).read_text())
        resources = config["static_resources"]
        manager = resources["listeners"][0]["filter_chains"][0]["filters"][0]["typed_config"]
        ext = manager["http_filters"][0]["typed_config"]
        self.assertFalse(ext["failure_mode_allow"])
        for field in ("request_body_mode", "response_body_mode"):
            self.assertEqual(ext["processing_mode"][field], "FULL_DUPLEX_STREAMED")
        routes = manager["route_config"]["virtual_hosts"][0]["routes"]
        self.assertEqual(len(routes), 1)
        self.assertEqual(routes[0]["route"]["timeout"], "0s")
        self.assertNotIn("retry_policy", routes[0]["route"])
        pool = next(cluster for cluster in resources["clusters"] if cluster["name"] == "inference_pool")
        self.assertEqual(pool["type"], "ORIGINAL_DST")
        self.assertEqual(pool["original_dst_lb_config"]["http_header_name"], "x-gateway-destination-endpoint")
        self.assertNotIn("load_assignment", pool)

    def test_namespaced_readonly_rbac_and_bounded_resources(self):
        resources = list(yaml.safe_load_all(Path(RESOURCES).read_text()))
        self.assertFalse(any(r["kind"] in ("ClusterRole", "ClusterRoleBinding") for r in resources))
        role = next(r for r in resources if r["kind"] == "Role")
        for rule in role["rules"]:
            self.assertLessEqual(set(rule["verbs"]), {"get", "list", "watch"})
            self.assertNotIn("secrets", rule["resources"])
        deployment = next(r for r in resources if r["kind"] == "Deployment")
        containers = deployment["spec"]["template"]["spec"]["containers"]
        self.assertEqual(len(containers), 2)
        self.assertLessEqual(sum(int(c["resources"]["requests"]["cpu"].removesuffix("m")) for c in containers), 100)
        for container in containers:
            self.assertIn("readinessProbe", container)
        service = next(r for r in resources if r["kind"] == "Service")
        self.assertEqual(service["spec"].get("type", "ClusterIP"), "ClusterIP")


if __name__ == "__main__":
    ENVOY, RESOURCES, CRDS, ECHO, VLLM = sys.argv[1:6]
    unittest.main(argv=[sys.argv[0], *sys.argv[6:]])
