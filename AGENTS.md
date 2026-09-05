# XScope engineering constraints

- All platform services are Rust. Use kube-rs for Kubernetes CRD types, cluster-agent and Operator; retain their process/RBAC boundaries. Keep every Rust package under rust/crates with the workspace manifest at rust/Cargo.toml. Use SeaORM for business database access.
- Build, test, generate and package applications through Bazel targets only. Do not run `cargo build/test`, `go build/test`, `uv run`, `pip install` or `npm run` as an alternative build path. Language manifests remain dependency/IDE inputs; Bazel repository rules may resolve them.
- Implement backend stages in this order: SSE/cancellation; InferencePool and llm-d EPP; RoutePolicy and revision pools; Operator autoscaling/PDB/pools and ownership; billing reservations/WAL checkpoints/events; multicluster desired state/heartbeat/ACK; telemetry/SLO/audit; payment/tax/inference API expansion.
- Do not expand UI ahead of the backend contracts. Verify behavior before advertising it as implemented.
- Preserve unrelated workspace changes and unrelated Kubernetes resources. Local development data resets are limited to XScope business data and its WAL/quota/event state, not Keycloak identities or other applications.
