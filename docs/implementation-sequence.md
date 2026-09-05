# Backend implementation sequence

The Kubernetes integration remains Go/controller-runtime. Rust owns the business control plane and Pingora gateway. All application builds, tests and image packaging use Bazel.

Each stage includes regression checks and a local Kubernetes verification before the next stage is considered complete.

- [ ] 0. Bazel-only application compilation, testing and image packaging; remove obsolete Go business code and legacy data migration paths; reset local business data.
- [ ] 1. SSE inference, incremental usage parsing, downstream cancellation propagation and interrupted-stream accounting.
- [ ] 2. InferencePool and llm-d EPP behind an inference-extension-compatible gateway; Pingora addresses the serving entry point, not individual model Pods.
- [ ] 3. RoutePolicy selecting stable/canary pools with validated header rules and explicit revision identity.
- [ ] 4. Operator-owned workloads, HPA/PDB and InferencePool; explicit ownership boundaries for llm-d infrastructure and EPP.
- [ ] 5. Idempotent billing reserve/settle/release protocol, durable WAL checkpoint and event delivery.
- [ ] 6. Authenticated multicluster desired state, heartbeat and versioned ACK/NACK.
- [ ] 7. OpenTelemetry, Prometheus, SLO definitions and append-only audit.
- [ ] 8. Real payment provider and tax invoice integration, then further inference APIs.

Real payment/tax integration requires an identified provider, credentials and jurisdiction-specific invoice configuration. Local manual bookkeeping is not a substitute for those integrations.
