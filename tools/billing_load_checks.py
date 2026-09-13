"""Repeatable bounded financial protocol load, not a GPU/whole-platform benchmark."""
import concurrent.futures
import json
import os
import time
from datetime import datetime, timezone
import urllib.parse


def verify(api, sql, setup, reserve_request):
    count = int(os.environ.get("XSCOPE_LOAD_REQUESTS", "128"))
    concurrency = int(os.environ.get("XSCOPE_LOAD_CONCURRENCY", "8"))
    if not 32 <= count <= 10000 or not 1 <= concurrency <= 128:
        raise ValueError("bounded load settings required")
    for pattern, accounts in (("distributed", 16), ("hot_account", 1)):
        projects = [f"load-{pattern.replace('_', '-')}-{i}" for i in range(accounts)]
        for project in projects: setup(project)
        def one(index):
            project = projects[index % accounts]
            identity = f"{project}-{index}"
            path = f"/billing/projects/{project}/reservations/{identity}"
            start = time.monotonic()
            assert api("POST", "/billing/reservations", reserve_request(identity, project), replica=index % 2)[0] == 200
            assert api("POST", path + "/dispatch", {}, replica=(index + 1) % 2)[0] == 200
            receipt = {"input_tokens": 1, "output_tokens": 0, "latency_ms": 1, "endpoint_id": "load-fixture", "region": "synthetic", "status": "succeeded"}
            assert api("POST", path + "/settle", receipt, replica=index % 2)[0] == 200
            elapsed = time.monotonic() - start
            if index % 5 == 0:
                assert api("POST", path + "/settle", receipt, replica=(index + 1) % 2)[0] == 200
            return elapsed
        start = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=concurrency) as executor:
            latency = sorted(executor.map(one, range(count)))
        elapsed = time.monotonic() - start
        prefix = "load-" + pattern.replace("_", "-") + "-%"
        assert sql(f"SELECT count(*) FROM xscope.billing_reservations WHERE project_id LIKE '{prefix}' AND state='settled'") == str(count)
        assert sql(f"SELECT count(*) FROM xscope.billing_reservations WHERE project_id LIKE '{prefix}' AND state IN ('reserved','dispatched')") == "0"
        assert sql("SELECT count(*) FROM (SELECT transaction_id FROM xscope.ledger_entries GROUP BY transaction_id HAVING sum(amount_microunits)<>0) AS unbalanced") == "0"
        for project in projects:
            query = urllib.parse.urlencode({"api_key_id": "key-" + project, "month": str(datetime.now(timezone.utc).date().replace(day=1))})
            assert api("GET", f"/billing/projects/{project}/projections/check?{query}")[1]["consistent_before"]
        quantile = lambda fraction: round(latency[min(len(latency)-1, int((len(latency)-1)*fraction))] * 1000, 2)
        print(json.dumps({"scope": "two_control_planes_postgres_protocol", "pattern": pattern, "requests": count,
            "concurrency": concurrency, "accounts": accounts, "completed_per_second": round(count/elapsed, 2),
            "latency_ms": {"p50": quantile(.50), "p95": quantile(.95), "p99": quantile(.99)},
            "settlement_replay_fraction": .2, "ledger_balanced": True, "pending_holds": 0}), flush=True)
