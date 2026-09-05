export type Health = {
  status: string;
  component?: string;
};

export type Session = {
  id?: string;
  username: string;
  email: string;
  memberships?: TenantMembership[];
};

export type PlatformUser = {
  id: string;
  external_subject: string;
  username: string;
  email: string;
  status: string;
  last_login_at: string;
  created_at: string;
  updated_at: string;
  memberships: TenantMembership[];
};

export type TenantMembership = {
  tenant_id: string;
  role: "owner" | "member";
};

export type Money = {
  currency: string;
  amount: number;
};

export type Model = {
  id: string;
  display_name: string;
  max_context_tokens: number;
  input_per_million_tokens: Money;
  output_per_million_tokens: Money;
  price_version: string;
};

export type Project = {
  id: string;
  tenant_id: string;
  name: string;
};

export type APIKey = {
  id: string;
  tenant_id: string;
  project_id: string;
  name: string;
  scopes: string[];
  allowed_models: string[];
  expires_at?: string;
  rate_limit_rpm: number;
  rate_limit_tpm: number;
  monthly_budget: Money;
  created_at: string;
  revoked_at?: string;
  status: "active" | "expired" | "revoked";
};

export type APIKeyRequest = Pick<APIKey, "id" | "tenant_id" | "project_id" | "name" | "scopes" | "allowed_models" | "rate_limit_rpm" | "rate_limit_tpm" | "monthly_budget"> & {
  expires_at?: string;
};

export type IssuedAPIKey = APIKey & {
  secret: string;
};

export type Quote = {
  model_id: string;
  price_version: string;
  maximum: Money;
};

export type ProjectBilling = {
  project_id: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  cost: Money;
};

export type BillingSummary = {
  period_start: string;
  period_end: string;
  requests: number;
  input_tokens: number;
  output_tokens: number;
  total: Money;
  projects: ProjectBilling[];
};

export type BillingAccount = {
  id: string;
  tenant_id: string;
  project_id: string;
  currency: string;
  balance: Money;
  enforce_balance: boolean;
  status: string;
  created_at: string;
  updated_at: string;
};

export type BillingOrder = {
  id: string;
  tenant_id: string;
  project_id: string;
  kind: string;
  amount: Money;
  status: string;
  description: string;
  created_at: string;
  updated_at: string;
};

export type CreateOrderRequest = Pick<BillingOrder, "id" | "tenant_id" | "project_id" | "amount" | "description">;

export type Payment = {
  id: string;
  order_id: string;
  provider: string;
  provider_reference: string;
  amount: Money;
  status: string;
  paid_at?: string;
  created_at: string;
};

export type Refund = {
  id: string;
  payment_id: string;
  amount: Money;
  reason: string;
  status: string;
  provider_reference?: string;
  created_at: string;
  completed_at?: string;
};

export type LedgerEntry = {
  id: string;
  transaction_id: string;
  billing_account_id: string;
  ledger_account: string;
  amount_microunits: number;
  currency: string;
  created_at: string;
};

export type LedgerTransaction = {
  id: string;
  tenant_id: string;
  kind: string;
  reference_type: string;
  reference_id: string;
  description: string;
  created_at: string;
  entries: LedgerEntry[];
};

export type Invoice = {
  id: string;
  tenant_id: string;
  project_id: string;
  period_start: string;
  period_end: string;
  amount: Money;
  status: string;
  title: string;
  issued_at: string;
};

export type ProviderSettlement = {
  provider_reference: string;
  amount: Money;
};

export type ReconciliationReport = {
  generated_at: string;
  provider: string;
  matched: number;
  platform_only: string[];
  provider_only: string[];
  amount_mismatches: Record<string, Money>;
};

export type ReconcileRequest = {
  tenant_id: string;
  provider: string;
  settlements: ProviderSettlement[];
};

export type ModelDeployment = {
  apiVersion?: string;
  kind?: string;
  metadata: {
    name: string;
    namespace: string;
    creationTimestamp?: string;
  };
  spec: {
    model: {
      id: string;
      revision: string;
      uri: string;
      checksum: string;
    };
    runtime: {
      image: string;
      protocol: "openai" | "triton-grpc" | "custom-http";
      port: number;
      arguments?: string[];
    };
    replicas: number;
    resources: {
      requests?: Record<string, string>;
      limits: Record<string, string>;
    };
    rollout?: {
      strategy: "rolling" | "canary" | "blueGreen";
      canaryWeight?: number;
    };
  };
  status?: {
    observedGeneration?: number;
    readyReplicas?: number;
    endpoint?: string;
    clusterId?: string;
    region?: string;
    conditions?: Array<{
      type: string;
      status: "True" | "False" | "Unknown";
      reason: string;
      message: string;
    }>;
  };
};

export type ListResponse<T> = {
  object: "list";
  data: T[];
};

export type APIErrorBody = {
  error?: {
    code?: string;
    message?: string;
  };
};
