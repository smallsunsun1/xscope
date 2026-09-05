"""Read-only verification of the provisioned Jaeger trace dashboard via Grafana NodePort."""
import argparse
import base64
import json
import os
import time

from observability_cluster import kubectl, local_only, request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inspect-query", action="store_true")
    args = parser.parse_args()
    local_only()
    secret = json.loads(kubectl("get", "secret", "xscope-grafana", "-o", "json"))["data"]
    user = base64.b64decode(secret["admin-user"]).decode()
    password = os.environ.get("XSCOPE_GRAFANA_PASSWORD") or base64.b64decode(secret["admin-password"]).decode()
    headers = {"Authorization": "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()}
    base = "http://127.0.0.1:30083"
    target = {"refId": "A", "datasource": {"type": "jaeger", "uid": "xscope-jaeger"},
              "queryType": "search", "service": "xscope-gateway", "limit": 100}
    if not args.inspect_query:
        dashboard = request(base + "/api/dashboards/uid/xscope-traces", headers=headers)["dashboard"]
        panel = next(p for p in dashboard["panels"] if p["type"] == "table")
        assert panel["targets"][0]["queryType"] == "search"
        assert panel["targets"][0]["service"] == "$service"
        defaults = {v["name"]: v["current"]["value"] for v in dashboard["templating"]["list"]}
        target = dict(panel["targets"][0])
        for key in ["service", "operation", "minDuration", "tags"]:
            value = target.get(key, "")
            if value.startswith("$"):
                target[key] = defaults[value[1:]]
        overview = request(base + "/api/dashboards/uid/xscope-overview", headers=headers)["dashboard"]
        assert any("xscope-traces" in link.get("url", "") for link in overview["links"])
    end = int(time.time() * 1000)
    payload = {"from": str(end - 3600000), "to": str(end), "queries": [target]}
    result = request(base + "/api/ds/query", payload, headers)["results"]["A"]
    assert not result.get("error"), result.get("error")
    frames = result.get("frames", [])
    assert frames, "No Jaeger search frame returned"
    fields = frames[0]["schema"]["fields"]
    values = frames[0]["data"]["values"]
    print("Jaeger table fields:", json.dumps(fields, ensure_ascii=False))
    print("Rows:", len(values[0]) if values else 0)
    if not args.inspect_query:
        queries = []
        for metric_panel in overview["panels"]:
            for source in metric_panel.get("targets", []):
                if "expr" not in source:
                    continue
                queries.append({**source, "refId": "P" + str(metric_panel["id"]),
                                "datasource": metric_panel["datasource"],
                                "expr": source["expr"].replace("$__rate_interval", "2m"),
                                "intervalMs": 30000, "maxDataPoints": 120,
                                "range": not source.get("instant", False)})
        metrics = request(base + "/api/ds/query", {"from": str(end - 3600000), "to": str(end), "queries": queries}, headers)
        assert len(metrics["results"]) == len(queries)
        assert all(not value.get("error") for value in metrics["results"].values()), "Overview query regression"
        print("PASS all", len(queries), "overview panel queries")
        names = [f["name"] for f in fields]
        assert values and values[0], "No real traces in the last hour; send an inference request first"
        id_field = next(field for field in fields if field["name"] == "traceID")
        link = id_field["config"]["links"][0]["internal"]
        assert link["datasourceUid"] == "xscope-jaeger" and link["query"]["query"] == "${__value.raw}"
        trace_id = values[names.index("traceID")][0]
        detail = {"from": str(end - 3600000), "to": str(end), "queries": [{
            "refId": "A", "datasource": target["datasource"], "query": trace_id}]}
        trace_result = request(base + "/api/ds/query", detail, headers)["results"]["A"]
        assert not trace_result.get("error") and trace_result.get("frames")
        assert trace_result["frames"][0]["data"]["values"][0], "Empty trace detail"
        # Impossible duration must remove all local development traces.
        filtered = {**payload, "queries": [{**target, "minDuration": "720h"}]}
        filtered_result = request(base + "/api/ds/query", filtered, headers)["results"]["A"]
        assert not filtered_result.get("error")
        assert all(not f["data"]["values"] or not f["data"]["values"][0] for f in filtered_result.get("frames", []))
        print("PASS dashboard provisioned, overview link, real trace search/detail and duration filtering")


if __name__ == "__main__":
    main()
