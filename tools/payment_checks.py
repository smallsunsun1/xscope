"""Signed synthetic Alipay evidence -> real control plane -> real PostgreSQL.

No calls to Alipay, no merchant credentials, and no real money movement.
"""
import concurrent.futures
import hashlib
import json
import os
import subprocess
import urllib.error
import urllib.request


def verify(api, sql, setup, root, fixture_binary, configs, launch, port):
    private = root / "payment-fixture"
    private.mkdir(mode=0o700)
    subprocess.run([fixture_binary, str(private)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    setup("alipay-check")
    for order, environment in (("synthetic-order", "production"), ("synthetic-rollback", "production"), ("synthetic-sandbox", "sandbox")):
        assert api("POST", "/billing/orders", {"id": order, "project_id": "alipay-check", "tenant_id": "test-tenant", "amount": {"currency": "CNY", "amount": 1001}}, internal=False)[0] == 201
        profile = hashlib.sha256(f"{environment}:synthetic-app:synthetic-seller".encode()).hexdigest()
        sql(f"INSERT INTO xscope.payment_checkouts (order_id,profile,environment,state,created_at,updated_at) VALUES ('{order}','{profile}','{environment}','pending',now(),now())")
    replicas = []
    for environment in ("production", "production", "sandbox"):
        public, internal = port(), port()
        env = dict(configs[0][2], XSCOPE_CONTROL_ADDRESS=f"127.0.0.1:{public}", XSCOPE_CONTROL_INTERNAL_ADDRESS=f"127.0.0.1:{internal}",
            XSCOPE_METRICS_ADDRESS=f"127.0.0.1:{port()}", XSCOPE_ALIPAY_CONFIG_FILE=str(private / f"{environment}.json"))
        configs.append((public, internal, env))
        launch(len(configs) - 1)
        replicas.append(len(configs) - 1)

    def callback(name, replica=0, mutate=False):
        data = (private / f"{name}.form").read_bytes()
        if mutate:
            data = data.replace(b"10.01", b"99.99")
        req = urllib.request.Request(f"http://127.0.0.1:{configs[replicas[replica]][0]}/payments/alipay/notify", method="POST", data=data,
            headers={"Content-Type": "application/x-www-form-urlencoded"})
        try:
            with urllib.request.urlopen(req, timeout=10) as response:
                return response.status, response.read()
        except urllib.error.HTTPError as error:
            return error.code, error.read()

    assert callback("paid", mutate=True)[0] == 400
    assert callback("wrong-amount")[0] == 409
    assert callback("missing")[0] == 404
    assert sql("SELECT count(*) FROM xscope.provider_receipts") == "0"
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as executor:
        results = list(executor.map(lambda n: callback("paid", n % 2), range(6)))
    assert results == [(200, b"success")] * 6
    assert sql("SELECT count(*) FROM xscope.payments WHERE order_id='synthetic-order'") == "1"
    assert sql("SELECT count(*) FROM xscope.provider_receipts WHERE order_id='synthetic-order'") == "1"
    assert sql("SELECT balance_microunits FROM xscope.billing_balances WHERE billing_account_id='account-alipay-check'") == "1001000000"
    assert sql("SELECT count(*),sum(e.amount_microunits) FROM xscope.ledger_entries e JOIN xscope.ledger_transactions t ON t.id=e.transaction_id WHERE t.reference_id IN (SELECT id FROM xscope.payments WHERE order_id='synthetic-order')") == "2|0"
    assert callback("pending")[0] == 200
    assert sql("SELECT state FROM xscope.payment_checkouts WHERE order_id='synthetic-order'") == "paid"
    sql("CREATE FUNCTION public.reject_synthetic_credit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.order_id='synthetic-rollback' THEN RAISE EXCEPTION 'synthetic rollback'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_synthetic_credit BEFORE INSERT ON xscope.payments FOR EACH ROW EXECUTE FUNCTION public.reject_synthetic_credit()")
    assert callback("rollback")[0] == 503
    assert sql("SELECT count(*) FROM xscope.provider_receipts WHERE order_id='synthetic-rollback'") == "0"
    assert sql("SELECT status FROM xscope.billing_orders WHERE id='synthetic-rollback'") == "pending"
    sql("DROP TRIGGER reject_synthetic_credit ON xscope.payments; DROP FUNCTION public.reject_synthetic_credit()")
    assert callback("rollback")[0] == 200
    before = sql("SELECT balance_microunits FROM xscope.billing_balances WHERE billing_account_id='account-alipay-check'")
    assert callback("sandbox", 2)[0] == 200
    assert sql("SELECT balance_microunits FROM xscope.billing_balances WHERE billing_account_id='account-alipay-check'") == before
    assert sql("SELECT count(*) FROM xscope.payments WHERE order_id='synthetic-sandbox'") == "0"
    assert api("POST", "/billing/orders/synthetic-sandbox/payments", {"id": "synthetic-manual", "provider": "manual-test", "provider_reference": "synthetic"}, replica=replicas[2], internal=False)[0] == 403
    payment = sql("SELECT id FROM xscope.payments WHERE order_id='synthetic-order'")
    assert api("POST", "/billing/refunds", {"id": "synthetic-forbidden", "payment_id": payment, "amount": {"currency": "CNY", "amount": 1}, "reason": "synthetic"}, internal=False)[0] == 400
    refund = {"id": "synthetic-refund", "payment_id": payment, "amount": {"currency": "CNY", "amount": 600}, "reason": "synthetic return"}
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        results = list(executor.map(lambda n: api("POST", "/billing/alipay/refunds", refund, replica=replicas[n % 2], internal=False), range(4)))
    assert all(status == 200 for status, _ in results), [status for status, _ in results]
    assert sql("SELECT status FROM xscope.refunds WHERE id='synthetic-refund'") == "pending"
    assert sql("SELECT balance_microunits FROM xscope.billing_balances WHERE billing_account_id='account-alipay-check'") == str(int(before) - 600000000)
    assert sql("SELECT count(*) FROM xscope.ledger_transactions WHERE idempotency_key='refund-hold:synthetic-refund'") == "1"
    assert api("POST", "/billing/alipay/refunds", {**refund, "amount": {"currency": "CNY", "amount": 601}}, replica=replicas[0], internal=False)[0] == 409
    assert api("POST", "/billing/alipay/refunds", {**refund, "id": "synthetic-too-much"}, replica=replicas[0], internal=False)[0] == 409
    evidence_path = "/billing/alipay/refunds/synthetic-refund/evidence"
    evidence = (private / "refund.json").read_bytes()
    assert api("POST", evidence_path, evidence.replace(b"6.00", b"9.00"), replica=replicas[0], internal=False)[0] == 400
    sql("CREATE FUNCTION public.reject_synthetic_refund() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.kind='refund_settled' THEN RAISE EXCEPTION 'synthetic rollback'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_synthetic_refund BEFORE INSERT ON xscope.ledger_transactions FOR EACH ROW EXECUTE FUNCTION public.reject_synthetic_refund()")
    assert api("POST", evidence_path, evidence, replica=replicas[0], internal=False)[0] == 503
    assert sql("SELECT status FROM xscope.refunds WHERE id='synthetic-refund'") == "pending"
    assert sql("SELECT count(*) FROM xscope.provider_receipts WHERE state='REFUND_SUCCESS'") == "0"
    sql("DROP TRIGGER reject_synthetic_refund ON xscope.ledger_transactions; DROP FUNCTION public.reject_synthetic_refund()")
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
        results = list(executor.map(lambda n: api("POST", evidence_path, evidence, replica=replicas[n % 2], internal=False), range(4)))
    assert all(status == 200 and row["state"] == "succeeded" for status, row in results)
    assert sql("SELECT count(*) FROM xscope.ledger_transactions WHERE idempotency_key='refund-settle:synthetic-refund'") == "1"
    assert sql("SELECT count(*),sum(amount_microunits) FROM xscope.ledger_entries WHERE transaction_id IN (SELECT id FROM xscope.ledger_transactions WHERE idempotency_key='refund-settle:synthetic-refund')") == "2|0"
    assert sql("SELECT balance_microunits FROM xscope.billing_balances WHERE billing_account_id='account-alipay-check'") == str(int(before) - 600000000)
    print("PASS Alipay synthetic RSA2 callbacks: two replicas credit once, amount/signature binding, balanced ledger, atomic rollback, late-state protection, sandbox cannot credit, manual capture/refund bypass denied; no real Alipay requests", flush=True)
    print("PASS refunds: debit once to pending clearing; unknown stays deducted; signed bound success clears once without a second customer debit, transactional fault rollback, concurrent replay and cumulative bounds; no external refund sent", flush=True)
    sql("INSERT INTO xscope.invoices (id,tenant_id,project_id,period_start,period_end,amount,currency,status,title,issued_at) VALUES ('synthetic-statement','test-tenant','alipay-check',now()-interval '1 day',now(),1,'CNY','statement_ready','Synthetic',now())")
    target = "/billing/invoices/synthetic-statement/tax-request"
    payload = {"title": "Synthetic request", "taxpayer_id": "SYNTHETIC0000000000"}
    assert api("POST", target, payload, internal=False)[1]["state"] == "pending_provider"
    assert api("POST", target, payload, replica=1, internal=False)[0] == 200
    assert api("POST", target, {**payload, "title": "Other"}, internal=False)[0] == 409
    assert api("GET", target, internal=False)[1]["tax_invoice_issued"] is False
    assert api("GET", "/billing/capabilities", internal=False)[1]["tax"]["issuance_enabled"] is False
    print("PASS China tax request: durable pending-provider intake, immutable idempotent identity, explicitly no tax issuance or fabricated invoice number", flush=True)
