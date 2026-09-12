"""Exercise recording/alert rules with the deployed Prometheus' real promtool."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import yaml
from observability_cluster import kubectl, local_only


def series(status, outcome, rate, route="/v1/chat/completions"):
    return {"series": f'xscope_http_requests_total{{component="gateway",route="{route}",status="{status}",outcome="{outcome}"}}', "values": f"0+{rate}x480"}


def main():
    local_only()
    rules = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"]) / "deploy/k8s/observability/alerts.yaml"
    deployment = json.loads(kubectl("get", "deployment", "prometheus", "-o", "json"))
    image = next(c["image"] for c in deployment["spec"]["template"]["spec"]["containers"] if c["name"] == "prometheus")
    if not (image.startswith("prom/prometheus:") or image.startswith("prom/prometheus@sha256:") or image.startswith("docker.io/prom/prometheus")):
        raise RuntimeError("Unexpected Prometheus image; test did not run")
    rules_value = yaml.safe_load(rules.read_text())
    alerts = {r["alert"]: r for group in rules_value["groups"] for r in group["rules"] if "alert" in r}
    def alert(name, when, firing):
        source = alerts[name]
        return {"eval_time": when, "alertname": name, "exp_alerts": [{"exp_labels": source["labels"], "exp_annotations": source["annotations"]}] if firing else []}
    def ratio(expected):
        return {"expr": "xscope:slo_availability_error_ratio:5m", "eval_time": "2h", "exp_samples": [{"labels": '{__name__="xscope:slo_availability_error_ratio:5m"}', "value": expected}]}
    tests = [
        {"interval": "1m", "input_series": [
            {"series": 'xscope_usage_queue{kind="oldest_seconds"}', "values": "31+0x60"},
            {"series": 'xscope_usage_queue{kind="ready"}', "values": "0+0x60"},
            {"series": 'xscope_billing_pending_oldest_seconds{state="dispatched"}', "values": "1900+60x60"}],
         "alert_rule_test": [alert("XScopeVolatileUsageBacklog", "10m", True), alert("XScopeUsageAdmissionBlocked", "10m", True), alert("XScopePendingMoneyOld", "10m", True)]},
        {"interval": "1m", "input_series": [
            {"series": 'xscope_usage_queue{kind="oldest_seconds"}', "values": "0+0x60"},
            {"series": 'xscope_usage_queue{kind="ready"}', "values": "1+0x60"},
            {"series": 'xscope_billing_pending_oldest_seconds{state="dispatched"}', "values": "0+0x60"}],
         "alert_rule_test": [alert("XScopeVolatileUsageBacklog", "10m", False), alert("XScopeUsageAdmissionBlocked", "10m", False), alert("XScopePendingMoneyOld", "10m", False)]},
        {"interval": "1m", "input_series": [series("200", "succeeded", 80), series("503", "provider_error", 20), series("401", "rejected", 10000)],
         "promql_expr_test": [ratio(0.2)], "alert_rule_test": [alert("XScopeAvailabilityFastBurn", "2h", True), alert("XScopeAvailabilitySlowBurn", "7h", True)]},
        {"interval": "1m", "input_series": [series("200", "succeeded", 100)], "promql_expr_test": [ratio(0)],
         "alert_rule_test": [alert("XScopeAvailabilityFastBurn", "2h", False), alert("XScopeAvailabilitySlowBurn", "7h", False)]},
        {"interval": "1m", "input_series": [], "promql_expr_test": [{"expr": "xscope:slo_availability_budget_remaining:30d", "eval_time": "2h", "exp_samples": []}],
         "alert_rule_test": [alert("XScopeAvailabilityFastBurn", "2h", False)]},
        {"interval": "1m", "input_series": [series("200", "succeeded", 80), series("200", "provider_error", 20), series("200", "cancelled", 10000)],
         "promql_expr_test": [ratio(0.2)]},
        {"interval": "1m", "input_series": [series("200", "succeeded", 80), series("503", "provider_error", 20, "/v1/completions"), series("503", "cancelled", 10000)],
         "promql_expr_test": [ratio(0.2)]},
    ]
    with tempfile.TemporaryDirectory(prefix="xscope-slo-fixture-") as temporary:
        directory = Path(temporary)
        (directory / "alerts.yaml").write_text(rules.read_text())
        (directory / "tests.yaml").write_text(yaml.safe_dump({"rule_files": ["/rules/alerts.yaml"], "evaluation_interval": "30s", "tests": tests}))
        subprocess.run(["docker", "run", "--rm", "--network=none", "--cpus=0.25", "--memory=192m", "--user", "0:0", "--entrypoint", "/bin/promtool",
                        "-v", str(directory) + ":/rules:ro", image, "test", "rules", "/rules/tests.yaml"], check=True, timeout=100)
    print("PASS SLO: fast/slow burn, healthy traffic, empty data, 4xx/cancellation exclusion and failed HTTP-200 SSE; no deployment changed")


if __name__ == "__main__":
    main()
