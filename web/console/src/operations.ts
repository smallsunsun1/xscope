import { request } from "./api";
import type { RoutePolicy, RoutePolicySpec } from "./types";

export type Capabilities = { platform_admin: boolean; billing_reviewer: boolean; evidence_submission: boolean; alert_receiver_configured: boolean };
export type ManagedPool = { id: string; cluster_id: string; deployment: string; generation: number; state: string };
export type PoolStatus = ManagedPool & {
  observed_at: string | null; observation_code: string; ready_until: string | null; desired_version: number;
  desired_replicas: number; blocked_participants: number; can_finish_drain: boolean;
  scale_operation: { phase: string; target: number; from: number; up: boolean } | null;
  participants: { session_id: string; acknowledged_generation: number; active_requests: number; reported_at: string | null; retired: boolean }[];
};
export type MemberCluster = { id: string; namespace: string; credential_epoch: number; expires_at: string; revoked_at: string | null; desired_version: number; acknowledged_version: number; heartbeat_at: string | null; online: boolean; last_error: string | null };
export type OpsAlert = { id: string; name: string; severity: string; summary: string; state: string; starts_at: string; ends_at: string | null; received_at: string; acknowledged_by: string | null; acknowledged_at: string | null };
export type Audit = { id: string; actor_id: string; action: string; resource_id: string; payload: unknown; created_at: string };
export type WorkerStatus = { consumer: string; state: string; acknowledged: number; target_sequence: number; backlog: number; attempts: number; last_error: string | null; available_at: string; updated_at: string };
export type RouteRevision = { revision: number; spec: RoutePolicySpec; actor: string; operation: string; created_at: string };
export type RouteAcks = { revision: number; known_sessions_only: true; all_known_acknowledged: boolean; sessions: { session_id: string; acknowledged: boolean; retired: boolean; delivered_at: string }[] };
export type Evidence = { id: string; source: string; source_request_id: string; document: string; explanation: string; usage: { input_tokens: number; output_tokens: number; latency_ms: number; endpoint_id: string; region: string; status: string } };
export type Review = { id: string; reservation_id: string; kind: string; submitted_by: string; reviewed_by: string | null; state: string; evidence_sha256: string; created_at: string; updated_at: string; decision: { action: string; reason: string } | null };
export type ReviewDetail = Review & { evidence: unknown; audit: unknown[] };
const esc = encodeURIComponent;
const billing = (project: string) => `/admin/v1/billing/accounts/${esc(project)}`;
const route = (project: string, model: string) => `/admin/v1/projects/${esc(project)}/models/${esc(model)}/route-policy`;
const post = <T>(path: string, payload: unknown) => request<T>(path, { method: "POST", body: JSON.stringify(payload) });
export const ops = {
  backup: () => request<{ telemetry_present: boolean; last_successful_at: number | null; last_scheduled_at: number | null; active_jobs: number | null; suspended: number | null; created_at: number | null }>("/admin/v1/operations/backup"),
  capabilities: () => request<Capabilities>("/admin/v1/capabilities"),
  pools: () => request<{ data: ManagedPool[] }>("/admin/v1/managed-pools"),
  pool: (id: string) => request<PoolStatus>(`/admin/v1/managed-pools/${esc(id)}`),
  transition: (id: string, generation: number, action: "drain" | "finish-drain" | "activate") => post(`/admin/v1/managed-pools/${esc(id)}/${action}`, { expected_generation: generation }),
  clusters: () => request<{ data: MemberCluster[] }>("/admin/v1/clusters"),
  alerts: (state: string, after?: string) => request<{ data: OpsAlert[]; next: string | null }>(`/admin/v1/operations/alerts?${new URLSearchParams({ ...(state ? { state } : {}), ...(after ? { after } : {}) })}`),
  acknowledge: (id: string, reason: string) => post(`/admin/v1/operations/alerts/${esc(id)}/acknowledge`, { reason }),
  audits: (after?: string) => request<{ data: Audit[]; next_after: string | null }>(`/admin/v1/audit?${new URLSearchParams({ limit: "30", ...(after ? { after } : {}) })}`),
  worker: (project: string) => request<WorkerStatus>(`${billing(project)}/event-worker`),
  retryWorker: (project: string, reason: string) => post(`${billing(project)}/event-worker/retry`, { reason }),
  history: (project: string, model: string, before?: number) => request<{ data: RouteRevision[]; next_before: number | null }>(`${route(project, model)}/history?${new URLSearchParams({ limit: "10", ...(before ? { before: String(before) } : {}) })}`),
  acks: (project: string, model: string) => request<RouteAcks>(`${route(project, model)}/acks`),
  release: (project: string, model: string, expected_revision: number, action: "pause" | "promote" | "rollback", target_revision?: number) => post<RoutePolicy>(`${route(project, model)}/actions`, { action, expected_revision, ...(target_revision ? { target_revision } : {}) }),
  reviews: (project: string, reservation: string, after?: string) => request<{ data: Review[]; next: string | null }>(`${billing(project)}/reservations/${esc(reservation)}/reviews?${new URLSearchParams({ limit: "20", ...(after ? { after_id: after } : {}) })}`),
  review: (project: string, id: string) => request<ReviewDetail>(`${billing(project)}/reviews/${esc(id)}`),
  evidence: (project: string, reservation: string, body: Evidence) => post<Review>(`${billing(project)}/reservations/${esc(reservation)}/reviews`, body),
  waiver: (project: string, reservation: string, body: { id: string; reason: string; incident_reference: string; request_terminated: boolean; platform_absorbs_loss: boolean }) => post<Review>(`${billing(project)}/reservations/${esc(reservation)}/waivers`, body),
  decide: (project: string, review: Review, action: string, reason: string) => post<Review>(`${billing(project)}/reviews/${esc(review.id)}/decision`, { evidence_sha256: review.evidence_sha256, action, reason }),
};
