import { test as base, expect, type Page } from "@playwright/test";
import type { Model, ModelDeployment, Project, Session, RoutePolicy, APIKey, BillingSummary } from "../src/types";

export const project: Project = { id: "proj-test", tenant_id: "tenant-test", name: "Test project" };
export const model: Model = {
  id: "test-model", display_name: "Test model", max_context_tokens: 32000,
  input_per_million_tokens: { amount: 100, currency: "CNY" },
  output_per_million_tokens: { amount: 200, currency: "CNY" }, price_version: "2026-09-01",
};
export const deployment: ModelDeployment = {
  metadata: { name: "test-deployment", namespace: "xscope-system", creationTimestamp: "2026-09-01T10:00:00Z" },
  spec: {
    model: { id: model.id, revision: "v1", uri: "s3://test/model", checksum: `sha256:${"a".repeat(64)}` },
    runtime: { image: "test/runtime:v1", protocol: "openai", port: 8000 }, replicas: 1,
    resources: { limits: { cpu: "1", memory: "1Gi" } },
  },
  status: { readyReplicas: 1, clusterId: "test-cluster", endpoint: "http://test:8000" },
};
export const policy: RoutePolicy = {
  tenant_id: project.tenant_id, project_id: project.id, model: model.id, revision: 3,
  spec: { stable_pool: "stable", canary_pool: "canary", canary_percent: 10, headers: [] },
};
const key: APIKey = {
  id: "key-test", name: "Test key", tenant_id: project.tenant_id, project_id: project.id,
  scopes: ["chat.completions"], allowed_models: [model.id], rate_limit_rpm: 60, rate_limit_tpm: 60000,
  monthly_budget: { currency: "CNY", amount: 10000 }, created_at: "2026-09-01T10:00:00Z", status: "active",
};
const summary: BillingSummary = {
  period_start: "2026-09-01T00:00:00Z", period_end: "2026-10-01T00:00:00Z",
  requests: 3, input_tokens: 1234, output_tokens: 456, total: { currency: "CNY", amount: 1234 },
  projects: [{ project_id: project.id, requests: 3, input_tokens: 1234, output_tokens: 456, cost: { currency: "CNY", amount: 1234 } }],
};

export type Backend = {
  role: "owner" | "member";
  projects: Project[];
  writes: { path: string; method: string; body: unknown }[];
  errors: string[];
  sessionStatus: number;
  deploymentStatus: number;
  policyConflict: boolean;
  platformAdmin: boolean;
  billingReviewer: boolean;
};

export const test = base.extend<{ backend: Backend }>({
  backend: [async ({ context, page }, use) => {
    const backend: Backend = { role: "owner", projects: [project], writes: [], errors: [], sessionStatus: 200, deploymentStatus: 200, policyConflict: false, platformAdmin: true, billingReviewer: false };
    const attachErrors = (tab: Page) => {
      tab.on("pageerror", error => backend.errors.push(error.message));
    };
    attachErrors(page);
    context.on("page", attachErrors);
    await context.route("**/api/**", async route => {
      const request = route.request();
      const url = new URL(request.url());
      const pathname = url.pathname.replace(/^\/api/, "");
      const method = request.method();
      const json = (body: unknown, status = 200) => route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) });
      const list = (data: unknown[]) => json({ object: "list", data });
      if (method !== "GET") {
        const body = request.postDataJSON();
        backend.writes.push({ path: pathname, method, body });
        if (pathname === "/admin/v1/projects" && method === "POST") {
          backend.projects.push(body);
          return json(body, 201);
        }
        if (pathname.endsWith("/route-policy") && method === "PUT") {
          return backend.policyConflict
            ? json({ error: { code: "conflict", message: "stale revision" } }, 409)
            : json({ ...policy, spec: body.spec, revision: 4 });
        }
        throw new Error(`Unmocked mutation: ${method} ${pathname}`);
      }
      if (pathname === "/healthz" || /\/components\/.+\/health$/.test(pathname)) return json({ status: "ok" });
      if (pathname === "/admin/v1/capabilities") return json({ platform_admin: backend.platformAdmin, billing_reviewer: backend.billingReviewer, evidence_submission: true, alert_receiver_configured: true });
      if (pathname === "/admin/v1/managed-pools" || pathname === "/admin/v1/clusters" || pathname === "/admin/v1/operations/alerts" || pathname === "/admin/v1/audit" || pathname.endsWith("/reviews")) return list([]);
      if (pathname === "/admin/v1/session") {
        const session: Session = { id: "user-test", username: "test-user", email: "test@example.com", memberships: [{ tenant_id: project.tenant_id, role: backend.role }] };
        return json(backend.sessionStatus === 200 ? session : { error: { code: "unauthorized", message: "expired session" } }, backend.sessionStatus);
      }
      if (pathname === "/admin/v1/projects") return list(backend.projects);
      if (pathname === "/v1/models") return list([model]);
      if (pathname === "/admin/v1/api-keys") return list([key]);
      if (pathname === "/admin/v1/model-deployments") return json(backend.deploymentStatus === 200 ? { data: [deployment] } : { error: { code: "cluster_unavailable", message: "unavailable" } }, backend.deploymentStatus);
      if (pathname === "/admin/v1/route-pools") return list([{ id: "stable", model: model.id, revision: "v1" }, { id: "canary", model: model.id, revision: "v2" }]);
      if (pathname === "/admin/v1/route-policies") return list([policy]);
      if (pathname === "/admin/v1/users") return list([{ id: "user-test", username: "test-user", email: "test@example.com", status: "active", last_login_at: "2026-09-01T10:00:00Z", memberships: [{ tenant_id: project.tenant_id, role: backend.role }] }]);
      if (pathname === "/admin/v1/billing/summary") return json(summary);
      if (pathname === `/admin/v1/billing/accounts/${project.id}`) return json({ id: "account-test", project_id: project.id, currency: "CNY", balance: { currency: "CNY", amount: 10000 }, enforce_balance: true });
      if (pathname.endsWith("/position")) return json({ project_id: project.id, currency: "CNY", balance_microunits: "10000000000", held_microunits: "123456789", available_microunits: "9876543211" });
      if (pathname.endsWith("/pending-reservations")) return json({ data: [{ id: "reservation-test", request_id: "request-test", api_key_id: key.id, state: "dispatched", reserved_microunits: "123456789", created_at: "2026-09-01T10:00:00Z", updated_at: "2026-09-01T10:01:00Z" }], next: null, created_before: url.searchParams.get("created_before"), requires_usage_evidence: true });
      if (["orders", "payments", "refunds", "ledger", "invoices"].some(name => pathname === `/admin/v1/billing/${name}`)) return list([]);
      if (pathname === "/admin/v1/quote") return json({ model_id: model.id, price_version: model.price_version, maximum: { amount: 2, currency: "CNY" } });
      throw new Error(`Unmocked API request: ${method} ${pathname}`);
    });
    // No request is allowed to escape to a real cluster or third-party service.
    await context.route(url => url.hostname !== "127.0.0.1", route => route.abort());
    await use(backend);
    expect(backend.errors, "uncaught browser errors").toEqual([]);
  }, { auto: true }],
});
export { expect };

export async function switchLanguage(page: Page, language: "English" | "简体中文") {
  await page.getByRole("combobox", { name: /^(Language|语言)$/ }).click();
  await page.getByRole("option", { name: language, exact: true }).click();
  await expect(page.locator("html")).toHaveAttribute("lang", language === "English" ? "en-US" : "zh-CN");
  await expect(page.locator(".ant-select-dropdown:visible")).toHaveCount(0);
}

export async function selectProject(page: Page) {
  await page.getByRole("combobox", { name: /^(All projects|全部项目)$/ }).click();
  // Ant Design's virtual Select exposes an off-screen ARIA option; click its visible row.
  await page.locator(".ant-select-dropdown:visible").getByText(project.name, { exact: true }).click();
}
