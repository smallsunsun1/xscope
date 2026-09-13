"""Bazel-generated Envoy profile; installation ownership remains separate."""
import copy
import json
import sys
from pathlib import Path
import yaml


def managed_profile(source):
    config = copy.deepcopy(source)
    manager = config["static_resources"]["listeners"][0]["filter_chains"][0]["filters"][0]["typed_config"]
    # Reject before ext_proc can invoke EPP. The marker is set only after the
    # Gateway traffic gate; NetworkPolicy must restrict this port to Gateways.
    manager.setdefault("http_filters", []).insert(0, {"name": "envoy.filters.http.rbac", "typed_config": {
        "@type": "type.googleapis.com/envoy.extensions.filters.http.rbac.v3.RBAC", "rules": {"action": "ALLOW", "policies": {
            "managed_gateway": {"permissions": [{"header": {"name": "x-xscope-managed-pool", "string_match": {"prefix": "managed-"}}}],
                "principals": [{"any": True}]}}}}})
    manager["http_filters"].insert(1, {"name": "envoy.filters.http.ext_authz", "typed_config": {
        "@type": "type.googleapis.com/envoy.extensions.filters.http.ext_authz.v3.ExtAuthz",
        "failure_mode_allow": False, "clear_route_cache": False,
        "allowed_headers": {"patterns": [{"prefix": "x-xscope-"}]},
        "http_service": {"server_uri": {"uri": "http://control-plane:8084", "cluster": "traffic_authority", "timeout": "3s"},
            "path_prefix": "/internal/v1/traffic/authorize"}}})
    manager["http_filters"].insert(2, {"name": "envoy.filters.http.header_mutation", "typed_config": {
        "@type": "type.googleapis.com/envoy.extensions.filters.http.header_mutation.v3.HeaderMutation",
        "mutations": {"request_mutations": [{"remove": "x-xscope-traffic-token"}, {"remove": "x-xscope-traffic-session"}]}}})
    config["static_resources"].setdefault("clusters", []).append({"name": "traffic_authority", "type": "STRICT_DNS", "connect_timeout": "2s",
        "load_assignment": {"cluster_name": "traffic_authority", "endpoints": [{"lb_endpoints": [{"endpoint": {"address": {
            "socket_address": {"address": "control-plane", "port_value": 8084}}}}]}]}})
    for host in manager["route_config"]["virtual_hosts"]:
        for route in host["routes"]:
            if "route" not in route:
                continue
            route["match"].setdefault("headers", []).append({"name": "x-xscope-managed-pool", "string_match": {"prefix": "managed-"}})
        host["routes"].append({"match": {"prefix": "/"}, "direct_response": {"status": 403, "body": {"inline_string": "managed gateway traffic required"}}})
    return config


if __name__ == "__main__":
    # Only static, synthetic/default build inputs; never put deployment Secrets
    # or private endpoint configuration into this action's inputs or arguments.
    print(json.dumps(managed_profile(yaml.safe_load(Path(sys.argv[1]).read_text()))))
