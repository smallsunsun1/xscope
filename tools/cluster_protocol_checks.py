"""Versioned cluster control protocol on disposable PostgreSQL / two processes."""
import concurrent.futures


def verify(api, sql):
    def admin(method, path, payload=None, replica=0):
        return api(method, "/clusters" + path, payload, replica=replica, internal=False)

    tokens = {}
    for cluster in ("synthetic-a", "synthetic-b"):
        status, value = admin("POST", "", {"id": cluster, "namespace": "synthetic-member", "credential_days": 30})
        assert status == 201
        tokens[cluster] = value["credential"]
    assert "credential" not in admin("GET", "")[1]["data"][0]

    def member(action, payload=None, token=None, replica=0):
        return api("POST", "/clusters/synthetic-a/" + action, payload, replica=replica, credential=token or tokens["synthetic-a"])

    assert member("poll", token=tokens["synthetic-b"])[0] == 401
    assert api("POST", "/clusters/synthetic-a/poll", {})[0] == 401
    assert member("poll")[1]["delivery"] is None
    state = {"expected_version": 0, "deployments": [{"name": "synthetic-model", "spec": {"replicas": 0}, "delete_uid": None}]}
    assert admin("PUT", "/synthetic-a/desired-state", state)[0] == 200
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        responses = list(pool.map(lambda replica: member("poll", replica=replica), range(2)))
    assert all(status == 200 for status, _ in responses)
    deliveries = [value["delivery"] for _, value in responses if value["delivery"]]
    assert len(deliveries) == 1
    delivery = deliveries[0]
    report = {key: delivery[key] for key in ("version", "sha256", "lease")}
    report.update(outcome="applied", error_code=None)
    assert member("report", {**report, "sha256": "0" * 64})[0] == 409
    sql("UPDATE xscope.member_clusters SET lease_until=clock_timestamp()-interval '1 second' WHERE id='synthetic-a'")
    assert member("report", report)[0] == 409
    new = member("poll", replica=1)[1]["delivery"]
    assert new["lease"] != delivery["lease"]
    assert member("report", report)[0] == 409
    report["lease"] = new["lease"]
    assert member("report", {**report, "outcome": "rejected", "error_code": "invalid_spec"})[0] == 200
    assert sql("SELECT acknowledged_version,last_error FROM xscope.member_clusters WHERE id='synthetic-a'") == "0|invalid_spec"
    # Two administrators cannot overwrite each other's desired state.
    with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
        results = list(pool.map(lambda replica: admin("PUT", "/synthetic-a/desired-state", {**state, "expected_version": 1}, replica), range(2)))
    assert sorted(status for status, _ in results) == [200, 409]
    delivery = member("poll")[1]["delivery"]
    assert delivery["version"] == 2
    report = {key: delivery[key] for key in ("version", "sha256", "lease")}
    report.update(outcome="applied", error_code=None)
    assert member("report", report, replica=1)[0] == 200
    assert member("report", report)[1]["duplicate"] is True
    assert member("poll")[1]["delivery"] is None
    assert sql("SELECT count(*) FROM xscope.operation_audits WHERE resource_id='synthetic-a' AND action='cluster.ack'") == "1"
    assert admin("PUT", "/synthetic-a/desired-state", {"expected_version": 2, "deployments": [{"name": "synthetic-model", "spec": None, "delete_uid": None}]})[0] == 400
    assert admin("POST", "/synthetic-a/revoke")[0] == 200
    assert member("poll")[0] == 401
    status, rotated = admin("POST", "/synthetic-a/credential", {"expected_epoch": 1, "credential_days": 30})
    assert status == 200
    assert member("poll")[0] == 401
    tokens["synthetic-a"] = rotated["credential"]
    assert member("poll")[0] == 200
    assert admin("POST", "/synthetic-a/credential", {"expected_epoch": 1, "credential_days": 30})[0] == 409
    sql("UPDATE xscope.member_clusters SET expires_at=clock_timestamp()-interval '1 second' WHERE id='synthetic-a'")
    assert member("poll")[0] == 401
    print("PASS cluster identities: isolated credentials, single delivery lease, expiry/fencing, desired CAS, ACK/NACK audit, replay, revocation and rotation", flush=True)


def verify_console(console):
    for user, status in ((None, 401), ("console-smoke-member", 403), ("console-smoke-outsider", 403)):
        assert console("GET", "/clusters", user=user)[0] == status
        assert console("POST", "/clusters/synthetic-b/revoke", user=user)[0] == status
    assert console("GET", "/clusters", user="console-smoke-owner")[0] == 200
    print("PASS cluster administration requires platform administrator, not ordinary tenant membership", flush=True)
