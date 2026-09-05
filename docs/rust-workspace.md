# Rust-only platform workspace

All packages live under `rust/crates/`; `rust/Cargo.toml` and `rust/Cargo.lock`
remain the workspace root and dependency inputs. Applications are compiled,
tested and packaged by Bazel only.

- `gateway`: Pingora admission, SSE proxying, quota and usage.
- `control-plane`: Axum business API, SeaORM identities and billing.
- `cluster-agent`: Axum internal API backed by kube-rs. Same routes, default
  namespace, shared-token authentication and port 8083 as the former Go service.
- `operator`: kube-runtime watches ModelDeployment, owned Deployment/Service;
  `kube-leader-election` maintains the existing Kubernetes Lease. A renewal
  failure cancels the controller before the 15-second lease expires.
- `kubernetes`: typed CRD contract, input validation and idempotent reconciliation.
- `telemetry`: shared tracing, OTLP export and private Prometheus listener.
- `domain`, `entities`, `migration`: business domain and SeaORM persistence.

The business control plane still has no Kubernetes service-account token.
Migrating languages does not merge process or RBAC boundaries. Existing
`platform.xscope.io/v1alpha1` CRDs, object UIDs, names, service endpoints and
business data are retained. The Operator checks ownership and uses
resourceVersion/UID preconditions before changing an existing child resource.
It does not silently adopt unrelated same-name Deployments or Services.

## Build and IDE

```sh
bazel test //...
bazel run @rules_rust//tools/rust_analyzer:gen_rust_project -- //rust/crates/...
bash tools/bazel-linux.sh images
```

The second command generates ignored, machine-specific `rust-project.json`
including Bazel-built proc macros and generated sources. VS Code uses that
file for completion rather than compiling via Cargo on save. Re-run the
“Bazel: refresh Rust IDE metadata” task after dependency or target changes,
then reload rust-analyzer. The workspace manifest remains usable by other IDEs.

## Boundaries still pending

This migration preserves Deployment/Service reconciliation; it does not add
RoutePolicy, revision pools, HPA/PDB or dynamic InferencePool provisioning.
The static echo InferencePool remains installation-owned. A created
ModelDeployment is not automatically routed through the public gateway yet.
See `implementation-sequence.md` for the remaining product work.

Tracing, metrics and local query access are documented in [observability.md](observability.md).
