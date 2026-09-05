export type Health = {
  status: string;
  component?: string;
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
  created_at: string;
};

export type IssuedAPIKey = APIKey & {
  secret: string;
};

export type Quote = {
  model_id: string;
  price_version: string;
  maximum: Money;
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
