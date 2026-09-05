import json
import sys
import unittest
from pathlib import Path

import yaml


class ObservabilityConfigTest(unittest.TestCase):
    def test_local_grafana_nodeport_only(self):
        overlay = yaml.safe_load(Path(sys.argv[8]).read_text())
        paths = {patch.get("path") for patch in overlay["patches"]}
        self.assertTrue({"grafana-service.yaml", "grafana-ingress.yaml"} <= paths)
        service = yaml.safe_load(Path(sys.argv[9]).read_text())
        self.assertEqual(service["metadata"], {"name": "grafana", "namespace": "xscope-system"})
        self.assertEqual(service["spec"]["type"], "NodePort")
        self.assertEqual(service["spec"]["ports"], [{"name": "http", "port": 3000, "targetPort": "http", "nodePort": 30083}])
        policy = yaml.safe_load(Path(sys.argv[10]).read_text())
        self.assertEqual(policy["spec"]["podSelector"], {"matchLabels": {"app.kubernetes.io/name": "grafana"}})
        self.assertEqual(policy["spec"]["ingress"], [{"ports": [{"port": 3000, "protocol": "TCP"}]}])
        base = list(yaml.safe_load_all(Path(sys.argv[5]).read_text()))
        self.assertEqual(next(r for r in base if r["kind"] == "NetworkPolicy")["spec"]["ingress"], [])

    def test_private_bounded_backends_and_readonly_discovery(self):
        resources = list(yaml.safe_load_all(Path(sys.argv[1]).read_text())) + list(yaml.safe_load_all(Path(sys.argv[5]).read_text()))
        for resource in resources:
            if resource["kind"] == "Service":
                self.assertEqual(resource["spec"].get("type", "ClusterIP"), "ClusterIP")
            if resource["kind"] == "Deployment":
                self.assertEqual(resource["spec"]["replicas"], 1)
                self.assertEqual(resource["spec"]["strategy"]["type"], "Recreate")
                spec = resource["spec"]["template"]["spec"]
                data = next(v for v in spec["volumes"] if v["name"] == "data")
                self.assertEqual(data["persistentVolumeClaim"]["claimName"], resource["metadata"]["name"] + "-data")
                self.assertIn("fsGroup", spec["securityContext"])
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
        badger = jaeger["extensions"]["jaeger_storage"]["backends"]["local"]["badger"]
        self.assertFalse(badger["ephemeral"])
        self.assertTrue(badger["consistency"])
        self.assertEqual(badger["ttl"]["spans"], "72h")
        self.assertEqual(badger["directories"], {"keys": "/var/lib/jaeger/keys", "values": "/var/lib/jaeger/values"})
        self.assertIn("memory_limiter", jaeger["service"]["pipelines"]["traces"]["processors"])

    def test_standalone_claims_and_grafana_provisioning(self):
        claims = list(yaml.safe_load_all(Path(sys.argv[4]).read_text()))
        self.assertEqual({c["metadata"]["name"] for c in claims}, {"prometheus-data", "jaeger-data", "grafana-data"})
        for claim in claims:
            self.assertEqual(claim["kind"], "PersistentVolumeClaim")
            self.assertEqual(claim["spec"]["accessModes"], ["ReadWriteOnce"])
            self.assertNotIn("ownerReferences", claim["metadata"])
        sources = yaml.safe_load(Path(sys.argv[6]).read_text())["datasources"]
        self.assertEqual({d["url"] for d in sources}, {"http://prometheus:9090", "http://jaeger:16686"})
        self.assertTrue(all(not d["editable"] for d in sources))
        dashboard = json.loads(Path(sys.argv[7]).read_text())
        self.assertEqual(dashboard["uid"], "xscope-overview")
        self.assertGreaterEqual(len(dashboard["panels"]), 6)
        data_panels = [p for p in dashboard["panels"] if p["type"] not in {"text", "row"}]
        self.assertTrue(all(p["datasource"]["uid"] == "xscope-prometheus" for p in data_panels))
        self.assertEqual(len([p for p in data_panels if p["type"] == "stat"]), 4)
        grafana = next(r for r in yaml.safe_load_all(Path(sys.argv[5]).read_text()) if r["kind"] == "Deployment")
        env = {e["name"]: e for e in grafana["spec"]["template"]["spec"]["containers"][0]["env"]}
        self.assertEqual(env["GF_AUTH_ANONYMOUS_ENABLED"]["value"], "false")
        self.assertEqual(env["GOMEMLIMIT"]["value"], "512MiB")
        self.assertEqual(grafana["spec"]["template"]["spec"]["containers"][0]["resources"]["limits"]["memory"], "768Mi")
        for key in ["GF_SECURITY_ADMIN_PASSWORD", "GF_SECURITY_SECRET_KEY"]:
            self.assertNotIn("value", env[key])
            self.assertEqual(env[key]["valueFrom"]["secretKeyRef"]["name"], "xscope-grafana")


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
