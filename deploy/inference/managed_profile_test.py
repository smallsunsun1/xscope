import unittest
from managed_profile import managed_profile


class ProfileTest(unittest.TestCase):
    def test_missing_marker_is_denied_and_base_profile_is_unchanged(self):
        host = {"routes": [{"match": {"prefix": "/"}, "route": {"cluster": "inference_pool", "timeout": "0s"}}]}
        config = {"static_resources": {"listeners": [{"filter_chains": [{"filters": [{"typed_config": {"route_config": {"virtual_hosts": [host]}}}]}]}]}}
        profile = managed_profile(config)
        manager = profile["static_resources"]["listeners"][0]["filter_chains"][0]["filters"][0]["typed_config"]
        self.assertEqual(manager["http_filters"][0]["name"], "envoy.filters.http.rbac")
        routes = profile["static_resources"]["listeners"][0]["filter_chains"][0]["filters"][0]["typed_config"]["route_config"]["virtual_hosts"][0]["routes"]
        self.assertNotIn("headers", host["routes"][0]["match"])
        self.assertEqual(routes[0]["match"]["headers"], [{"name": "x-xscope-managed-pool", "string_match": {"prefix": "managed-"}}])
        self.assertEqual(routes[0]["route"]["timeout"], "0s")
        self.assertEqual(routes[1]["direct_response"]["status"], 403)


if __name__ == "__main__":
    unittest.main()
