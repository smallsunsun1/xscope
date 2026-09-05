import type {
  APIErrorBody,
  APIKey,
  Health,
  IssuedAPIKey,
  ListResponse,
  Model,
  ModelDeployment,
  Project,
  Quote,
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
  models: async () => (await request<ListResponse<Model>>("/v1/models")).data,
  projects: async () => (await request<ListResponse<Project>>("/admin/v1/projects")).data,
  createProject: (project: Project) =>
    request<Project>("/admin/v1/projects", {
      method: "POST",
      headers: { "Idempotency-Key": idempotencyKey("console-project") },
      body: JSON.stringify(project),
    }),
  apiKeys: async () => (await request<ListResponse<APIKey>>("/admin/v1/api-keys")).data,
  createAPIKey: (payload: Omit<APIKey, "created_at">) =>
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

export async function componentHealth(path: string): Promise<Health> {
  const response = await fetch(path, { headers: { Accept: "application/json" } });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return (await response.json()) as Health;
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : "发生未知错误";
}
