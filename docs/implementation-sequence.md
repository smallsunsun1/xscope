# Backend implementation sequence

The Kubernetes integration now uses Rust/kube-rs, following the user's revised architecture. All Rust packages live under rust/crates; the business control plane and Kubernetes adapters retain separate processes and RBAC boundaries. All application builds, tests and image packaging use Bazel.

Each stage includes regression checks and a local Kubernetes verification before the next stage is considered complete.

- [x] 0. Bazel-only application compilation, testing and image packaging; remove obsolete Go business code and legacy data migration paths; reset local business data. Subsequent architecture revision migrates the remaining Kubernetes adapters to kube-rs (without another data reset).
- [x] 1. SSE inference, incremental usage parsing, downstream cancellation propagation and explicit interrupted-stream outcomes. Unknown usage retains its TPM reservation; monetary settlement/reconciliation remains stage 5.
- [x] 2. InferencePool and llm-d EPP behind a standalone Envoy serving entry; Pingora addresses that entry, not individual model Pods. Development echo uses in-flight scoring; real GPU cache-aware scoring still requires vLLM metrics.
- [ ] 3. RoutePolicy selecting stable/canary pools with validated header rules and explicit revision identity.
- [ ] 4. Operator-owned workloads, HPA/PDB and InferencePool; explicit ownership boundaries for llm-d infrastructure and EPP.
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
- Next is stage 3 (RoutePolicy and stable/canary pools). Stages 3–8 remain incomplete. A new ModelDeployment is not automatically connected to a serving pool yet (stage 4 ownership/reconciliation). See [serving integration and tests](inference-serving.md).

## Subsequent Rust-only migration and telemetry verification (2026-09-05)

- All remaining Go application source was replaced by kube-rs cluster-agent and Operator. Every Rust package now lives under `rust/crates`; Bazel-generated `rust-project.json` supplies IDE dependencies and proc macros.
- `bazel test //...`: all 13 current host targets pass, including the new kube-rs API/controller and telemetry tests; there are no Go test targets now.
- Five Bazel-built application images are deployed to Docker Desktop. The Rust Operator acquired the existing Lease. No business data or Keycloak identities were reset during this migration.
- Live Kubernetes smoke verifies authenticated CRD creation/listing, scale 0 → 1 → 0, readiness/status, stable child UID, deletion and owner-reference garbage collection. Temporary smoke resources were removed.
- Live telemetry smoke verifies one trace across Gateway, Envoy, Runtime and usage ingestion in Control Plane, plus seven healthy per-Pod Prometheus targets. EPP trace continuity remains unverified.
- Tracing/metrics are now implemented ahead of the rest of stage 7 as explicitly requested. Formal SLO, audit and alert notification delivery are still pending. See [observability usage and limitations](observability.md).
