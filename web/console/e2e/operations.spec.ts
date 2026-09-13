import { test, expect, selectProject } from "./fixtures";

test("pool safety displays unknown requests and refuses optimistic finish or activation", async ({ page }) => {
  await page.route("**/api/admin/v1/managed-pools", route => route.fulfill({ json: { data: [{ id: "managed-test", cluster_id: "test-cluster", deployment: "test-model", generation: 7, state: "draining" }] } }));
  await page.route("**/api/admin/v1/managed-pools/managed-test", route => route.fulfill({ json: { id: "managed-test", cluster_id: "test-cluster", deployment: "test-model", generation: 7, state: "draining", observation_code: "ok", desired_version: 3, desired_replicas: 2, ready_until: null, scale_operation: { phase: "draining", from: 2, target: 1 }, blocked_participants: 1, can_finish_drain: false, participants: [{ session_id: "test-session", acknowledged_generation: 7, active_requests: -1, retired: true, reported_at: null }] } }));
  await page.goto("/#/operations");
  await page.getByRole("button", { name: "Inspect drain status" }).click();
  await expect(page.getByText("Unknown; scale-down remains blocked", { exact: true })).toBeVisible();
  await expect(page.getByRole("button", { name: "Finish drain", exact: true })).toBeDisabled();
  await expect(page.getByRole("button", { name: "Reopen pool", exact: true })).toBeDisabled();
  await expect(page.getByText("Automatic operation in progress; manual actions are disabled")).toBeVisible();
});

test("tenant owner without platform capability cannot fetch cross-cluster operations", async ({ page, backend }) => {
  backend.platformAdmin = false;
  let fetched = 0;
  page.on("request", req => { if (/managed-pools|\/clusters|operations\/alerts/.test(req.url())) fetched++; });
  await page.goto("/#/operations");
  await expect(page.getByText("Platform administrator access required", { exact: true })).toBeVisible();
  expect(fetched).toBe(0);
});

test("release actions bind current revision and stale responses require reloading", async ({ page }) => {
  await page.route("**/api/admin/v1/**/route-policy/history?*", route => route.fulfill({ json: { data: [{ revision: 2, operation: "update", actor: "test-owner", spec: { stable_pool: "stable", canary_pool: null, canary_percent: 0, headers: [] }, created_at: "2026-01-01T00:00:00Z" }], next_before: null } }));
  await page.route("**/api/admin/v1/**/route-policy/acks", route => route.fulfill({ json: { revision: 3, all_known_acknowledged: false, known_sessions_only: true, sessions: [] } }));
  let payload: unknown;
  await page.route("**/api/admin/v1/**/route-policy/actions", route => { payload = route.request().postDataJSON(); return route.fulfill({ status: 409, json: { error: { code: "conflict" } } }); });
  await page.goto("/#/routing");
  await page.getByRole("button", { name: "Release history" }).click();
  await page.getByRole("button", { name: "Pause canary", exact: true }).click();
  await page.getByRole("button", { name: "OK", exact: true }).click();
  expect(payload).toEqual({ expected_revision: 3, action: "pause" });
  await expect(page.getByText(/Someone else changed this policy/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Promote canary", exact: true })).toBeDisabled();
});

test("evidence submission requires receipt and usage; owners cannot self-approve or request waiver", async ({ page, backend }) => {
  await page.goto("/#/billing"); await selectProject(page);
  await page.getByRole("tab", { name: "Pending requests", exact: true }).click();
  await page.getByRole("button", { name: "reservation-test" }).click();
  await expect(page.getByRole("button", { name: "Request loss waiver" })).toBeDisabled();
  await page.getByRole("button", { name: "Submit usage evidence", exact: true }).click();
  await page.getByRole("button", { name: "Submit usage evidence", exact: true }).last().click();
  await expect(page.locator(".ant-form-item-explain-error").first()).toBeVisible();
  expect(backend.writes).toHaveLength(0);
});

test("alert acknowledgement is explicit, audited by API, and does not resolve notification", async ({ page }) => {
  await page.route("**/api/admin/v1/operations/alerts?*", route => route.fulfill({ json: { data: [{ id: "test-alert", name: "XScopeSyntheticTest", severity: "warning", summary: "Synthetic inbox notice", state: "firing", starts_at: "2026-01-01T00:00:00Z", received_at: "2026-01-01T00:00:10Z", acknowledged_at: null }], next: null } }));
  let body: unknown;
  await page.route("**/api/admin/v1/operations/alerts/test-alert/acknowledge", route => { body = route.request().postDataJSON(); return route.fulfill({ json: { acknowledged: true, resolution_not_implied: true } }); });
  await page.goto("/#/operations"); await page.getByRole("tab", { name: "Alert inbox" }).click();
  await page.getByRole("button", { name: "View notification" }).click();
  await expect(page.getByRole("button", { name: "Acknowledge", exact: true })).toBeDisabled();
  await page.getByRole("textbox", { name: "Investigation note" }).fill("Investigating synthetic fixture");
  await page.getByRole("button", { name: "Acknowledge", exact: true }).click();
  await expect(page.getByRole("dialog")).not.toBeVisible();
  expect(body).toEqual({ reason: "Investigating synthetic fixture" });
});

test("independent reviewer approves a submitted case using the exact displayed digest", async ({ page, backend }) => {
  backend.billingReviewer = true;
  const row = { id: "case-test", reservation_id: "reservation-test", kind: "usage_evidence", state: "submitted", submitted_by: "another-owner", evidence_sha256: "a".repeat(64), created_at: "2026-01-01T00:00:00Z" };
  await page.route("**/api/admin/v1/billing/accounts/**/reservations/**/reviews?*", route => route.fulfill({ json: { data: [row], next: null } }));
  await page.route("**/api/admin/v1/billing/accounts/**/reviews/case-test", route => route.fulfill({ json: { ...row, evidence: { document: "Synthetic receipt", usage: { input_tokens: 2, output_tokens: 1 } }, audit: [] } }));
  let body: unknown;
  await page.route("**/api/admin/v1/billing/accounts/**/reviews/case-test/decision", route => { body = route.request().postDataJSON(); return route.fulfill({ json: { ...row, state: "approved" } }); });
  await page.goto("/#/billing"); await selectProject(page);
  await page.getByRole("tab", { name: "Pending requests", exact: true }).click();
  await page.getByRole("button", { name: "reservation-test" }).click();
  await page.getByRole("button", { name: "Inspect evidence" }).click();
  await page.getByRole("textbox", { name: "Investigation note" }).fill("Verified synthetic usage receipt");
  await expect(page.getByRole("button", { name: "Approve evidence", exact: true })).toBeEnabled();
  await page.getByRole("button", { name: "Approve evidence", exact: true }).click();
  await page.getByRole("button", { name: "OK", exact: true }).click();
  await expect(page.getByText("Review decision saved", { exact: true })).toBeVisible();
  expect(body).toEqual({ evidence_sha256: "a".repeat(64), action: "approve", reason: "Verified synthetic usage receipt" });
});
