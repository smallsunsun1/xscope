# Backend implementation sequence

The Kubernetes integration now uses Rust/kube-rs, following the user's revised architecture. All Rust packages live under rust/crates; the business control plane and Kubernetes adapters retain separate processes and RBAC boundaries. All application builds, tests and image packaging use Bazel.

Each stage includes regression checks and a local Kubernetes verification before the next stage is considered complete.

- [x] 0. Bazel-only application compilation, testing and image packaging; remove obsolete Go business code and legacy data migration paths; reset local business data. Subsequent architecture revision migrates the remaining Kubernetes adapters to kube-rs (without another data reset).
- [x] 1. SSE inference, incremental usage parsing, downstream cancellation propagation and explicit interrupted-stream outcomes. Unknown usage retains its TPM reservation; monetary settlement/reconciliation remains stage 5.
- [x] 2. InferencePool and llm-d EPP behind a standalone Envoy serving entry; Pingora addresses that entry, not individual model Pods. Development echo uses in-flight scoring; real GPU cache-aware scoring still requires vLLM metrics.
- [x] 3. RoutePolicy selecting registered stable/canary pools for the existing public model, with validated header rules and explicit revision identity. Arbitrary multi-model routing remains future work.
- [x] 4. Operator-owned workloads, PDB and InferencePool; explicit ownership boundaries for llm-d infrastructure and EPP. Subsequent user decision replaces direct HPA generation with KEDA ScaledObject; see [KEDA implementation boundaries](keda-autoscaling.md). Full GPU/SLO autoscaling, resource-pool admission and automatic public serving registration are not included in this checkpoint.
- [ ] 5. Idempotent billing reserve/settle/release protocol, durable WAL checkpoint and event delivery.
- [ ] 6. Authenticated multicluster desired state, heartbeat and versioned ACK/NACK.
- [ ] 7. OpenTelemetry, Prometheus, SLO definitions and append-only audit.
- [ ] 8. Real payment provider and tax invoice integration, then further inference APIs.

Real payment/tax integration requires an identified provider, credentials and jurisdiction-specific invoice configuration. Local manual bookkeeping is not a substitute for those integrations.

## Earlier stage 2 verification (2026-09-05)

- `bazel test //...`: all 11 host test targets pass, including Go controller tests, Rust formatting/Clippy checks and serving configuration/schema tests.
- Linux ARM64 release: gateway unit tests and real-TCP SSE integration tests pass through `tools/bazel-linux.sh test --config=release`.
- Five OCI images are compiled and packaged by Bazel and deployed to Docker Desktop. No application Dockerfile invokes Cargo/Go/pip builds.
- Local schema/WAL/quota reset completed; Keycloak identities retained and console sign-in verified.
- Cluster SSE smoke: final usage 3 input / 5 output tokens, one balanced ledger transaction. Client cancellation stops runtime generation and produces one `cancelled` usage event.
- Stage 2: official InferencePool schema + llm-d-router v0.9.0 EPP + Envoy v1.38.2 are deployed; upstream images are digest-pinned. Namespaced read-only RBAC and private serving ingress are configured.
- Real cluster smoke verifies SSE, JSON responses, 401/403, spoofed destination stripping, cancellation through Envoy/EPP, selected Pod address and persisted usage. Successful streams produce one balanced ledger transaction.
- Three Runtime replicas were discovered without restarting Pingora: 12 requests reached all three Pod addresses (5/2/5). Runtime was restored to one replica afterwards.
- Serving outage test: direct gateway readiness and inference returned 503; no Runtime request was started. Serving was restored to one replica. NodePort may have no available endpoints while gateway readiness fails.
- The serving Pod requests 50m CPU total. Echo's EPP profile explicitly disables default GPU-metrics scraping; the separate vLLM profile is provided, not GPU-validated.
- At this checkpoint, stage 3 was next. See the subsequent RoutePolicy checkpoint below; a new ModelDeployment is still not automatically connected to a serving pool (stage 4 ownership/reconciliation).

## RoutePolicy checkpoint (2026-09-05)

- Stage 3 now supports project/tenant-scoped persisted policies, owner-only writes, atomic revision CAS, exact header rules and weighted stable/canary routing for the existing public model. The console provides a flow-management page using the real APIs.
- Keys/routes refresh atomically; invalid snapshots retain last good state. Requests freeze their selected pool and revision; no cross-pool POST retries. Usage persists the actual pool/revision; traces and bounded route-selection metrics expose the decision.
- The local overlay adds an isolated canary Envoy/EPP + InferencePool + echo Runtime, requesting 70m CPU total. Base installations remain single-pool. No existing project is automatically switched to canary.
- Live smoke verified owner checks, invalid pool/header rejection, two concurrent writers (200/409), header canary, 100% canary and revisioned rollback; Envoy selected the appropriate real Pod and PostgreSQL recorded its pool/revision. Test key revoked; the isolated smoke project's financial evidence is retained.
- Final verification: all 14 Bazel host test targets pass; the routing console's built entry/chunk is served by the deployed control plane. Routing smoke verifies canary span attributes and selection counters; existing inference smoke still verifies SSE/cancellation and a balanced ledger transaction. All 10 per-Pod Prometheus targets are UP. Browser interaction/visual QA was not performed in this pass.
- Both local pools still use echo, not real GPU inference. Arbitrary multi-model routing, gateway ACKs and policy history/approvals are not implemented. Next is stage 4: Operator InferencePool/HPA/PDB ownership. Stages 5/6 and the remaining SLO/audit/payment work are still pending. See [routing guide](route-policy.md).

## Operator lifecycle checkpoint (2026-09-05)

Subsequent KEDA checkpoint: direct HPA generation is replaced by typed KEDA ScaledObject reconciliation. Pinned KEDA 2.20.1 is installed locally (75m aggregate CPU request), and only the XScope Operator/cluster-agent and related CRD/RBAC were rolled for this change. All 17 host Bazel tests pass. The live smoke verified legacy HPA migration, foreign-resource refusal, KEDA ownership, real CPU-driven 1 → 2 ready replicas, inference through the managed pool, disabling/returning to manual scale, and garbage collection. No test models/scalers remain. EPP metric generation is present but not GPU-validated; automatic scale-down is disabled pending request drain. ComputePool/PoolGrant, Kueue admission and TTFT/TPOT control remain unimplemented. See [KEDA details](keda-autoscaling.md). The following bullets record the earlier direct-HPA implementation.

- Stage 4 adds optional HPA, PDB and official v1 InferencePool reconciliation with owner UID checks, optimistic write/delete preconditions, drift repair and garbage collection. Same-name external resources are rejected before owned children are written. Installation-owned Envoy/EPP, Service, configuration, certificates and RBAC remain outside ModelDeployment ownership.
- HPA scales the ModelDeployment `/scale` subresource, then Operator scales the Deployment. Status reports actual and ready replicas separately, a scale selector and ResourcesReady / Ready / AutoscalingActive conditions. Manual scaling is rejected while HPA is enabled; full spec updates require resourceVersion CAS.
- Installed pinned metrics-server v0.8.1 on Docker Desktop (30m CPU / 96Mi memory requests). Its self-signed kubelet TLS exception is local-only, not a production setting. CPU HPA is operational; pending-request custom metrics still require an adapter and runtime metric integration.
- All 14 Bazel host test targets passed, including new resource ownership and versioned update cases. Five Linux images were compiled/packaged through Bazel; scoped rollout updated Operator, cluster-agent and control-plane without data resets.
- Live Operator smoke passed twice, including a deliberately foreign PDB, drift repair, real Pingora → Envoy/EPP → managed Pool → Echo Pod SSE, CPU-driven HPA and (second pass) two ready replicas, optional-resource removal, preserved external EPP and owner-reference GC. All temporary fixtures are removed.
- Original Kubernetes CRUD/scale smoke passed. Inference regression first encountered one fail-closed 503 from a stale Redis connection after an earlier Redis restart; the next run reconnected and passed authentication, SSE/JSON, cancellation, Pod selection, usage persistence and balanced ledger settlement. Idempotent recovery from ambiguous quota operations remains stage 5; this run did not add unsafe automatic POST retries.
- Telemetry regression passed: one trace includes Gateway, Envoy, EPP, Runtime and Control Plane; all 10 existing per-Pod Prometheus targets are UP. Final cluster check confirmed all 15 platform Pods healthy, the metrics-server Pod ready and no temporary ModelDeployment/HPA/PDB fixtures remaining.
- New pools still need installation-owned EPP and explicit Gateway serving-catalog registration. This is not automatic customer model onboarding, GPU validation, queue-based scaling or completed multicluster synchronization. Next is stage 5 billing reservations, WAL checkpoints and events. See [Operator guide](operator-lifecycle.md).

## Billing recovery foundation (2026-09-05, stage 5 remains open)

- Added single-writer, bounded-memory WAL replay with durable contiguous checkpoints; 4xx no longer skip usage. Corruption/torn tails preserve evidence and fail closed. Existing JSONL history is retained; no compaction or monetary reservation guarantee is implied.
- Redis reserve/settle now carry server-generated operation IDs across bounded reconnect retries, reject conflicting settlement payloads and do not recreate expired buckets. Model POST is not retried.
- All 15 Bazel host test targets passed, including 1,108-record recovery through rejection/lost ACK/process kill. Disposable Redis fault injection passed with two real Gateway processes: reserve/settle replies lost after execution, duplicate/conflicting settlement, shared exact counters and stale-connection first-request recovery.
- Bazel-built Linux Gateway was rolled out using Recreate against its existing PVC. All 62 prior WAL records were privately backed up and retained; checkpoint caught up. Live inference then passed SSE/JSON, cancellation, usage persistence and balanced ledger; telemetry passed a five-service trace and all 10 Prometheus targets UP. Regression added four development usage events; no database, Redis, identity or PVC reset occurred.
- Monetary reserve/settle/release, atomic budget enforcement and transaction-linked outbox/event consumers are still pending, as are WAL compaction and per-replica persistent storage. See [billing recovery boundaries and commands](billing-recovery.md).

## Money protocol and transactional outbox (2026-09-05, stage 5 remains open)

- Added SeaORM reservation, event and consumer entities plus additive migration 000005. Internal authenticated reserve/dispatch/release/settle APIs freeze price terms, bind request identity, serialize account balance and key-budget checks, and retain ambiguous dispatched holds.
- Settlement commits usage, balanced entries, final reservation state and outbox together. Existing ledger writers also emit outbox events under account locks; refunds/debits on balance-enforced accounts cannot consume held funds. The legacy usage endpoint rejects reservation-bound request identities.
- Account-local event sequences are allocated under the account transaction lock, with bounded polling, redelivery and durable CAS ACK. No global commit-order watermark or external message broker is assumed.
- All 15 host Bazel test targets pass. Real PostgreSQL/two-control-plane smoke passed concurrent balance and budget admission, idempotent/conflicting settlement, refund protection, partial cancellation charging, deliberately failed outbox rollback, and process restart with retained holds/ACKs. Temporary fixtures removed; no live financial holds were created by that test.
- Bazel-built Linux control-plane deployed after a private xscope schema backup; additive tables verified and old writers terminated. Live internal-auth checks passed; real inference created matching ledger/outbox evidence. SSE/JSON/cancellation regression and a five-service trace passed; all 10 Prometheus targets UP. Existing identities, Redis, WAL and PVCs were retained.
- Gateway does not yet call the monetary protocol. The next step is request identity/pre-reserve/dispatch/WAL settlement integration and uncertain-request reconciliation, not UI expansion or a claim of end-to-end monetary admission. See [money protocol](billing-protocol.md).

## Gateway monetary admission checkpoint (2026-09-05, stage 5 remains open)

- Gateway now persists reservation intent before financial admission and withholds prompt bytes until PostgreSQL records dispatch. Server-generated financial request IDs are separate from untrusted client correlation IDs. Shared Rust domain DTOs keep Gateway and SeaORM control-plane contracts aligned.
- Separate single-writer admission WAL and v1/v2 usage WAL avoid completion-behind-intent deadlocks. Recovery releases reserved orphan intents but never dispatches/replays inference. Lost settlement ACKs replay the same immutable completion; unknown or over-limit usage retains its database hold and WAL evidence, without becoming a zero charge.
- Conservative context-sized input bounds plus a normalized provider output limit protect admission; reject ambiguous max fields and multi-result n != 1. This is deliberately more conservative than tokenizer-estimated money admission. No compaction, automatic unknown-usage reconciliation or shared-file multireplica deployment is claimed.
- All 17 host Bazel test targets passed (including independently added KEDA configuration tests). New fault-peer test covers reserve/dispatch/settle lost replies, process kills after remote commits, duplicate client IDs, unknown/over-limit usage, rejected admission and no provider replay. Real PostgreSQL/two-control-plane/two-Gateway smoke passed: eight concurrent requests yield one Runtime invocation and seven 402 responses; successful repeated client IDs settle separately and balance exactly.
- Only the Bazel-built Linux Gateway image was loaded and deployed for this change. Existing WAL/checkpoints were privately backed up at `.build/gateway-billing-backup-20260905T131751863042Z`; Recreate rollout retained old history and enabled financial admission. Keycloak, PostgreSQL, Redis and PVCs were not reset.
- Live checks passed reserve -> dispatch -> settled events with one balanced ledger, SSE/JSON, EPP Pod selection, cancellation propagation and an intentionally retained unknown-usage hold (`req-01a071b8-fe92-74c0-9dcd-d9482c3e7db2`). Trace `bea6f85ef3ba480f9014eae0df417205` spans Gateway/Envoy/EPP/Runtime/control-plane; all 10 Prometheus targets are UP.
- Scoped image builds now accept `./tools/bazel-linux.sh images gateway`. Copy selected tarballs only after Bazel build success; do not silently continue with stale archives when a separate cquery fails. An isolated Bazel output base was used for operational scripts while an unrelated release build held the default server.
- Next: evidence-backed reconciliation for ambiguous holds, segmented/archived WAL with disk-watermark admission and per-replica storage, then production event consumers and multicluster contracts. See [money protocol](billing-protocol.md).

## Subsequent Rust-only migration and telemetry verification (2026-09-05)

- All remaining Go application source was replaced by kube-rs cluster-agent and Operator. Every Rust package now lives under `rust/crates`; Bazel-generated `rust-project.json` supplies IDE dependencies and proc macros.
- `bazel test //...`: all 13 current host targets pass, including the new kube-rs API/controller and telemetry tests; there are no Go test targets now.
- Five Bazel-built application images are deployed to Docker Desktop. The Rust Operator acquired the existing Lease. No business data or Keycloak identities were reset during this migration.
- Live Kubernetes smoke verifies authenticated CRD creation/listing, scale 0 → 1 → 0, readiness/status, stable child UID, deletion and owner-reference garbage collection. Temporary smoke resources were removed.
- Live telemetry smoke verifies one trace across Gateway, Envoy, Runtime and usage ingestion in Control Plane, plus seven healthy per-Pod Prometheus targets. EPP trace continuity remains unverified.
- Tracing/metrics are now implemented ahead of the rest of stage 7 as explicitly requested. Formal SLO, audit and alert notification delivery are still pending. See [observability usage and limitations](observability.md).

## Persistent observability verification (2026-09-05)

- Prometheus TSDB/WAL migrated from emptyDir to a 2 GiB PVC; pre-migration historical samples remain queryable. Jaeger switched from memory to synchronous Badger storage on a 2 GiB PVC (72-hour TTL).
- The 471 queryable traces from the old Jaeger memory backend were exported to `.build/observability-backup-20260905-pvc/jaeger-traces.json`; they are a private local backup, not automatically re-ingested into Badger.
- Grafana 13.2.1 is digest-pinned, uses a 1 GiB PVC, and provisions both data sources and the XScope overview dashboard. Anonymous access is disabled; random local credentials and encryption key are retained in a dedicated Secret. Grafana Keycloak SSO is not yet configured.
- `bazel run //tools:observability_persistence_smoke -- --restart` replaces all three monitoring Pods and verifies the same historical metric samples, stored trace and user-created dashboard survive. It also verifies retained credentials/data-source health and removes only its temporary test dashboard.
- The post-migration telemetry smoke now verifies EPP alongside Gateway, Envoy, Runtime and Control Plane in the same trace; all seven scrape targets are healthy.
