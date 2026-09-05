import type {
  APIErrorBody,
  APIKey,
  APIKeyRequest,
  BillingAccount,
  BillingOrder,
  BillingSummary,
  CreateOrderRequest,
  Health,
  IssuedAPIKey,
  ListResponse,
  Money,
  Model,
  ModelDeployment,
  Invoice,
  LedgerTransaction,
  Payment,
  PlatformUser,
  Project,
  Quote,
  ReconciliationReport,
  ReconcileRequest,
  Refund,
  Session,
} from "./types";

const controlBase = import.meta.env.VITE_CONTROL_API_BASE ?? "/api";

export class APIError extends Error {
  constructor(
    message: string,
    readonly status: number,
    readonly code: string,
  ) {
    super(message);
    this.name = "APIError";
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(`${controlBase}${path}`, {
    ...init,
    headers: {
      Accept: "application/json",
      ...(init?.body ? { "Content-Type": "application/json" } : {}),
      ...init?.headers,
    },
  });
  if (!response.ok) {
    let body: APIErrorBody = {};
    try {
      body = (await response.json()) as APIErrorBody;
    } catch {
      // Preserve the HTTP fallback when an intermediary returns non-JSON.
    }
    throw new APIError(
      body.error?.message ?? `请求失败（HTTP ${response.status}）`,
      response.status,
      body.error?.code ?? "http_error",
    );
  }
  if (response.status === 204) return undefined as T;
  return (await response.json()) as T;
}

function idempotencyKey(prefix: string): string {
  return `${prefix}-${crypto.randomUUID()}`;
}

export const api = {
  health: () => request<Health>("/healthz"),
  session: () => request<Session>("/admin/v1/session"),
  models: async () => (await request<ListResponse<Model>>("/v1/models")).data,
  projects: async () => (await request<ListResponse<Project>>("/admin/v1/projects")).data,
  users: async () => (await request<ListResponse<PlatformUser>>("/admin/v1/users")).data,
  updateMembership: (userID: string, tenantID: string, role: "owner" | "member") =>
    request<PlatformUser>(
      `/admin/v1/users/${encodeURIComponent(userID)}/memberships/${encodeURIComponent(tenantID)}`,
      { method: "PUT", body: JSON.stringify({ role }) },
    ),
  createProject: (project: Project) =>
    request<Project>("/admin/v1/projects", {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-project") },
      body: JSON.stringify(project),
    }),
  apiKeys: async () => (await request<ListResponse<APIKey>>("/admin/v1/api-keys")).data,
  createAPIKey: (payload: APIKeyRequest) =>
    request<IssuedAPIKey>("/admin/v1/api-keys", {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-api-key") },
      body: JSON.stringify(payload),
    }),
  revokeAPIKey: (id: string) =>
    request<void>(`/admin/v1/api-keys/${encodeURIComponent(id)}`, { method: "DELETE" }),
  quote: (model: string, inputTokens: number, outputTokens: number) => {
    const params = new URLSearchParams({
      model,
      input_tokens: String(inputTokens),
      output_tokens: String(outputTokens),
    });
    return request<Quote>(`/admin/v1/quote?${params}`);
  },
  billingSummary: (projectID = "") => {
    const params = new URLSearchParams();
    if (projectID) params.set("project_id", projectID);
    const query = params.size ? `?${params}` : "";
    return request<BillingSummary>(`/admin/v1/billing/summary${query}`);
  },
  billingAccount: (projectID: string) =>
    request<BillingAccount>(`/admin/v1/billing/accounts/${encodeURIComponent(projectID)}`),
  updateBalancePolicy: (projectID: string, enforceBalance: boolean) =>
    request<BillingAccount>(`/admin/v1/billing/accounts/${encodeURIComponent(projectID)}`, {
      method: "PUT",
      body: JSON.stringify({ enforce_balance: enforceBalance }),
    }),
  billingOrders: async () =>
    (await request<ListResponse<BillingOrder>>("/admin/v1/billing/orders")).data,
  createOrder: (payload: CreateOrderRequest) =>
    request<BillingOrder>("/admin/v1/billing/orders", {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-order") },
      body: JSON.stringify(payload),
    }),
  capturePayment: (orderID: string, paymentID: string) =>
    request<Payment>(`/admin/v1/billing/orders/${encodeURIComponent(orderID)}/payments`, {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-payment") },
      body: JSON.stringify({
        id: paymentID,
        provider: "manual",
        provider_reference: `manual-${crypto.randomUUID()}`,
      }),
    }),
  payments: async () =>
    (await request<ListResponse<Payment>>("/admin/v1/billing/payments")).data,
  refunds: async () =>
    (await request<ListResponse<Refund>>("/admin/v1/billing/refunds")).data,
  createRefund: (paymentID: string, amount: Money, reason: string) =>
    request<Refund>("/admin/v1/billing/refunds", {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-refund") },
      body: JSON.stringify({ id: `refund-${crypto.randomUUID()}`, payment_id: paymentID, amount, reason }),
    }),
  ledger: async () =>
    (await request<ListResponse<LedgerTransaction>>("/admin/v1/billing/ledger")).data,
  invoices: async () =>
    (await request<ListResponse<Invoice>>("/admin/v1/billing/invoices")).data,
  createInvoice: (payload: Omit<Invoice, "amount" | "status" | "issued_at">) =>
    request<Invoice>("/admin/v1/billing/invoices", {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-invoice") },
      body: JSON.stringify(payload),
    }),
  reconcile: (payload: ReconcileRequest) =>
    request<ReconciliationReport>("/admin/v1/billing/reconciliation", {
      method: "POST",
      body: JSON.stringify(payload),
    }),
  deployments: async (namespace: string) =>
    (
      await request<ListResponse<ModelDeployment>>(
        `/admin/v1/model-deployments?namespace=${encodeURIComponent(namespace)}`,
      )
    ).data,
  createDeployment: (deployment: ModelDeployment) =>
    request<ModelDeployment>("/admin/v1/model-deployments", {
      method: "POST",
      body: JSON.stringify(deployment),
    }),
  scaleDeployment: (namespace: string, name: string, replicas: number) =>
    request<ModelDeployment>(
      `/admin/v1/model-deployments/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}/scale`,
      { method: "PUT", body: JSON.stringify({ replicas }) },
    ),
  deleteDeployment: (namespace: string, name: string) =>
    request<void>(
      `/admin/v1/model-deployments/${encodeURIComponent(namespace)}/${encodeURIComponent(name)}`,
      { method: "DELETE" },
    ),
};

export const componentHealth = (component: string) =>
  request<Health>(`/admin/v1/components/${encodeURIComponent(component)}/health`);

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "发生未知错误";
}
