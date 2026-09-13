import { test, expect, switchLanguage, selectProject, project } from "./fixtures";

const pages = [
  ["dashboard", "Every inference, accounted for.", "让每一次推理，都有迹可循。"],
  ["projects", "Projects", "项目"],
  ["keys", "API Keys", "API Keys"],
  ["models", "Models & pricing", "模型与价格"],
  ["deployments", "Model deployments", "模型部署"],
  ["routing", "Traffic routing", "流量路由"],
  ["billing", "Billing & finance", "计费与财务"],
  ["users", "Users & memberships", "用户与成员"],
  ["observability", "From metrics to every request.", "从指标，到每一次请求。"],
  ["operations", "Operations & recovery", "运行与恢复"],
] as const;

test("all lazy pages switch in place and preserve the selected language on reload", async ({ page }, testInfo) => {
  await page.goto("/");
  for (const [route, english, chinese] of pages) {
    await page.goto(`/#/${route}`);
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(english);
    await expect(page.locator("main")).not.toContainText(/[\u3400-\u9fff]/);
    if (route === "dashboard") await page.screenshot({ path: testInfo.outputPath("overview-en.png"), fullPage: true });
    await switchLanguage(page, "简体中文");
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(chinese);
    if (route === "dashboard") await page.screenshot({ path: testInfo.outputPath("overview-zh.png"), fullPage: true });
    await switchLanguage(page, "English");
    await expect(page.getByRole("heading", { level: 1 })).toHaveText(english);
    await expect(page.locator("main")).not.toContainText(/[\u3400-\u9fff]/);
  }
  await switchLanguage(page, "简体中文");
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page).toHaveTitle("XScope · 模型平台控制台");
});

test("browser language is used on first visit and invalid stored preferences are ignored", async ({ browser }) => {
  const context = await browser.newContext({ locale: "zh-CN" });
  // This case only needs the shell; mock every API without contacting a backend.
  await context.route("**/api/**", route => route.fulfill({ json: { data: [] } }));
  const page = await context.newPage();
  await page.addInitScript(() => localStorage.setItem("xscope.locale", "unsupported"));
  await page.goto("http://127.0.0.1:4187/#/projects");
  await expect(page.locator("html")).toHaveAttribute("lang", "zh-CN");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("项目");
  await context.close();
});

test("locale changes sync across tabs without losing a project draft", async ({ context, page, backend }) => {
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "Create project", exact: true }).first().click();
  await page.getByLabel("Project name", { exact: true }).fill("客服生产环境");
  await page.getByLabel("Tenant ID", { exact: true }).fill("");
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.locator(".ant-form-item-explain-error")).toContainText("Tenant ID");
  const other = await context.newPage();
  await other.goto("/#/projects");
  await switchLanguage(other, "简体中文");
  await expect(page.getByLabel("项目名称", { exact: true })).toHaveValue("客服生产环境");
  await expect(page.getByText("Please enter Tenant ID", { exact: true })).not.toBeVisible();
  await expect(page.getByText("请输入租户 ID", { exact: true })).toBeVisible();
  await page.getByLabel("租户 ID", { exact: true }).fill("tenant-local");
  await page.getByRole("button", { name: /^创\s*建$/ }).click();
  await expect(page.getByText("项目已创建", { exact: true })).toBeVisible();
  expect(backend.writes).toHaveLength(1);
  expect(backend.writes[0].body).toMatchObject({ name: "客服生产环境", tenant_id: "tenant-local" });
  await expect(page.getByRole("cell", { name: /客服生产环境/ })).toBeVisible();
});

test("language switching still works when browser storage is unavailable", async ({ page }) => {
  await page.addInitScript(() => {
    Storage.prototype.getItem = () => { throw new DOMException("Storage disabled", "SecurityError"); };
    Storage.prototype.setItem = () => { throw new DOMException("Storage disabled", "SecurityError"); };
  });
  await page.goto("/#/projects");
  await switchLanguage(page, "简体中文");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("项目");
  await switchLanguage(page, "English");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Projects");
});

test("project validation prevents writes and switching translates subsequent validation", async ({ page, backend }) => {
  backend.projects = [];
  await page.goto("/#/projects");
  await page.getByRole("button", { name: "Create project", exact: true }).first().click();
  await page.getByRole("button", { name: "Create", exact: true }).click();
  await expect(page.getByText("Enter a project name", { exact: true })).toBeVisible();
  expect(backend.writes).toHaveLength(0);
  await page.getByRole("button", { name: "Close", exact: true }).click();
  await expect(page.getByRole("dialog")).not.toBeVisible();
  await switchLanguage(page, "简体中文");
  await page.getByRole("button", { name: "创建项目", exact: true }).first().click();
  await page.getByRole("button", { name: /^创\s*建$/ }).click();
  await expect(page.getByText("请输入项目名称", { exact: true })).toBeVisible();
  expect(backend.writes).toHaveLength(0);
});

test("billing renders exact microunits and pending records in both languages", async ({ page, backend }) => {
  await page.goto("/#/billing");
  await selectProject(page);
  await expect(page.getByText("CNY 1.23456789", { exact: true })).toBeVisible();
  await expect(page.getByText("CNY 98.76543211", { exact: true })).toBeVisible();
  await page.getByRole("tab", { name: "Pending requests", exact: true }).click();
  await expect(page.getByRole("button", { name: "reservation-test" })).toBeVisible();
  await switchLanguage(page, "简体中文");
  await expect(page.getByRole("tab", { name: "待核查请求", exact: true })).toHaveAttribute("aria-selected", "true");
  await page.getByRole("button", { name: "reservation-test" }).click();
  await expect(page.getByText("只读查询", { exact: true })).toBeVisible();
  await expect(page.getByText("request-test", { exact: true })).toBeVisible();
  expect(backend.writes).toHaveLength(0);
});

test("all finance tabs and empty states are translated", async ({ page }) => {
  await page.goto("/#/billing");
  for (const tab of ["Manual payments & refunds", "Double-entry ledger", "Internal statements", "Reconciliation demo"]) {
    await page.getByRole("tab", { name: tab, exact: true }).click();
    await expect(page.getByRole("tabpanel", { name: tab })).not.toContainText(/[\u3400-\u9fff]/);
  }
});

test("route revision conflicts block publication until the editor is reloaded", async ({ page, backend }) => {
  backend.policyConflict = true;
  await page.goto("/#/routing");
  await page.getByRole("button", { name: "Configure policy" }).click();
  await page.getByRole("button", { name: "Publish policy", exact: true }).click();
  await page.getByRole("button", { name: "OK", exact: true }).click();
  await expect(page.getByText(/Someone else changed this policy/)).toBeVisible();
  await expect(page.getByRole("button", { name: /Publish policy$/ })).toBeDisabled();
  expect(backend.writes).toHaveLength(1);
  expect(backend.writes[0]).toMatchObject({ method: "PUT", body: { expected_revision: 3, spec: { stable_pool: "stable", canary_pool: "canary", canary_percent: 10 } } });
});

test("members cannot mutate routing, deployments, or balance policy", async ({ page, backend }) => {
  backend.role = "member";
  await page.goto("/#/routing");
  await expect(page.getByRole("button", { name: "Configure policy" })).toBeDisabled();
  await page.goto("/#/deployments");
  await expect(page.getByRole("button", { name: "Deploy model", exact: true }).first()).toBeDisabled();
  await expect(page.getByRole("button", { name: "Scale replicas", exact: true })).toBeDisabled();
  await page.goto("/#/billing");
  await selectProject(page);
  await expect(page.getByRole("switch", { name: "Prepaid balance enforcement" })).toBeDisabled();
  await page.getByRole("tab", { name: "Pending requests", exact: true }).click();
  await expect(page.getByText("Requires owner access to the project's tenant")).toBeVisible();
  expect(backend.writes).toEqual([]);
});

test("unavailable cluster and expired session show translated recovery states", async ({ page, backend }) => {
  backend.deploymentStatus = 503;
  backend.sessionStatus = 401;
  await page.goto("/#/deployments");
  await expect(page.getByText("Unable to verify your session", { exact: true })).toBeVisible();
  await expect(page.getByText("Kubernetes cluster is not connected", { exact: true })).toBeVisible();
  await switchLanguage(page, "简体中文");
  await expect(page.getByText("无法验证控制台会话", { exact: true })).toBeVisible();
  await expect(page.getByText("Kubernetes 集群尚未连接", { exact: true })).toBeVisible();
});

test("deployments, keys, and estimates have localized dialogs and keep API identifiers", async ({ page }) => {
  await page.goto("/#/deployments");
  await page.getByRole("button", { name: "View test-deployment details" }).click();
  await expect(page.getByText("Resource status snapshot", { exact: true })).toBeVisible();
  await expect(page.getByText("test-cluster", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Close", exact: true }).click();
  await expect(page.getByRole("dialog")).not.toBeVisible();
  await page.goto("/#/keys");
  await page.getByRole("button", { name: "Issue key", exact: true }).first().click();
  await expect(page.getByLabel("Key name", { exact: true })).toBeVisible();
  await expect(page.getByText("Monthly budget (CNY, 0 means unlimited)")).toBeVisible();
  await page.getByRole("button", { name: "Close", exact: true }).click();
  await expect(page.getByRole("dialog")).not.toBeVisible();
  await switchLanguage(page, "简体中文");
  await expect(page.getByText("有效", { exact: true })).toBeVisible();
  await page.goto("/#/models");
  await page.getByLabel("模型", { exact: true }).click();
  await page.locator(".ant-select-dropdown:visible").getByText("Test model", { exact: true }).click();
  await page.getByRole("button", { name: "计算最高费用" }).click();
  await expect(page.getByText("预计最高费用", { exact: true })).toBeVisible();
  await switchLanguage(page, "English");
  await expect(page.getByText("Estimated maximum cost", { exact: true })).toBeVisible();
});
