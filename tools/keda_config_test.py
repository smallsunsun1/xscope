import sys
import unittest
import yaml
from install_local_keda import DIGESTS, OWNER, render


class KedaConfigTest(unittest.TestCase):
    def test_pinned_images_and_low_resource_requests_preserve_security(self):
        with open(sys.argv[1]) as source:
            upstream = [r for r in yaml.safe_load_all(source) if r]
        resources = render(upstream)
        deployments = [r for r in resources if r["kind"] == "Deployment"]
        self.assertEqual(len(deployments), 3)
        self.assertTrue(all(r["metadata"]["annotations"][OWNER] == "xscope-local" for r in resources))
        for resource in deployments:
            self.assertEqual(resource["spec"]["replicas"], 1)
            for container in resource["spec"]["template"]["spec"]["containers"]:
                self.assertIn(container["image"].split("@sha256:")[1], DIGESTS.values())
                self.assertEqual(container["resources"]["requests"]["cpu"], "25m")
                self.assertNotIn("--disable-webhooks", container.get("args", []))
        for kind in ("CustomResourceDefinition", "ValidatingWebhookConfiguration", "APIService"):
            before = [r for r in upstream if r["kind"] == kind]
            after = [r for r in resources if r["kind"] == kind]
            self.assertTrue(before)
            for a, b in zip(before, after):
                a["metadata"].setdefault("annotations", {})[OWNER] = "xscope-local"
                self.assertEqual(a, b)


if __name__ == "__main__":
    unittest.main(argv=[sys.argv[0]])
