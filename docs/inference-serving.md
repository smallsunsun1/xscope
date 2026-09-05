# InferencePool serving integration (stage 2)

The cluster path is:

```text
client -> Pingora (key/model/quota, SSE, WAL)
       -> inference-serving Service
          -> Envoy -> llm-d EPP (ext_proc) -> selected Runtime Pod
                         ^
                    InferencePool/demo-pool (Pod discovery)
```

Pingora has one `XSCOPE_SERVING_ENTRY_JSON` with a pool ID, model and serving
address. It does not discover or load-balance model Pods. Its remaining Pingora
transport balancer covers DNS addresses of that single entry, not model replica
selection. Legacy `XSCOPE_UPSTREAMS_JSON` is rejected to avoid silently retaining
the old direct-Runtime deployment. Multi-model/stable/canary policy is stage 3.

## Pinned upstreams and ownership

- InferencePool v1 schema: Gateway API Inference Extension v1.5.0.
- EPP: `ghcr.io/llm-d/llm-d-router-endpoint-picker:v0.9.0`, pinned by digest. The
  older `llm-d-inference-scheduler` name has been replaced by upstream.
- EPP's InferenceObjective/InferenceModelRewrite schemas: llm-d-router v0.9.0,
  API group `llm-d.ai`. Installing these schemas does not implement XScope
  RoutePolicy or canary rollout management.
- Envoy distroless v1.38.2, pinned by digest, standalone same-Pod EPP over TLS
  loopback. This follows upstream's self-signed local connection recipe; it is
  not a cross-node authenticated EPP transport configuration.

Bazel downloads checksum-verified official schemas and builds
`//deploy/inference:crds`; they are not reimplemented in this repo. The local
deployment script installs them before applying custom resources. It does not
replace the existing cluster-wide Istio/Gateway API CRDs.

Deployment manifests currently own the serving Service/Deployment, configuration,
namespaced read-only RBAC and demo InferencePool. EPP reads Pods and the pool;
it cannot create/delete workloads or read Secrets. The Rust kube-runtime
Operator still owns ModelDeployment workloads/Services only. Automatic
per-deployment InferencePool creation, HPA/PDB and resource finalization belong
to stage 4. Do not let the Operator and installation manifests co-own resources.

The serving layer is cluster-local. Future member clusters need their own serving
entry/EPP/pools; this is not the multicluster desired-state/ACK protocol (stage 6).

## Development versus real inference

The echo Runtime is still the default backend. `plugins.yaml` uses llm-d's
`active-request-scorer` to prefer less busy endpoints using EPP's own in-flight
counts. It does not scrape nonexistent GPU metrics or claim real KV-cache gains.

`plugins-vllm.yaml` provides upstream queue, KV utilization and approximate prefix
cache scorers. Switch the EPP `--config-file` to this mounted file only after
selecting real vLLM-compatible Pods with `/metrics` and matching target ports.
This profile is not a tested GPU deployment or a precise KV event index.

One serving Pod contains two containers, with total CPU request **50m** and memory
request **144Mi**. Envoy has one worker and EPP `GOMAXPROCS=1`; this is a low-resource
development preset, not an HA/production sizing claim. Recreate updates interrupt
serving until the replacement is ready. No extra Gateway Controller, tokenizer,
latency predictor, or monitoring stack is installed.

## Security and stream contract

- Client `x-envoy-*`, `x-gateway-*`, `x-inference-*`, `x-ai-eg-*` headers and the
  API key are removed before proxying. Clients cannot supply EPP's destination
  address or override Envoy retries. Caller-defined business headers remain for
  future validated RoutePolicy rules.
- Envoy uses FULL_DUPLEX_STREAMED for request/response processing. Route timeout
  is disabled for SSE, idle streams time out after 300 seconds. No inference
  POST retry policy or direct-Pod fallback is configured. EPP errors fail closed.
  Pingora connection establishment is bounded (3 seconds TCP / 5 seconds total).
  If no serving backend is healthy, gateway readiness fails; Kubernetes may
  remove all public Service endpoints, in which case NodePort clients can see
  connection failure/timeout rather than an HTTP response. The gateway itself
  returns 503 when reached directly in this state.
- NetworkPolicies restrict serving HTTP ingress to Pingora and Runtime ingress
  to serving/control-plane probes. Enforcement requires a NetworkPolicy-capable
  CNI; YAML alone cannot guarantee enforcement on Docker Desktop.
- Envoy logs request ID, selected endpoint, status and cancellation flags, not
  prompts, API keys or response content. Pingora meters the pool identity;
  individual replica evidence is in the serving log.
- Interrupted monetary settlement is still stage 5. A cancelled event with
  unknown counters is not a finalized zero-cost invoice.

## Verification

```bash
# Hermetic host regression, no live-cluster mutations.
bazel test //rust/crates/gateway:gateway_test //tools:streaming_e2e_test //deploy/inference:config_test

# Real docker-desktop API + EPP + cancellation + database smoke.
# Adds development usage records; never resets data or changes replicas.
bazel run //tools:inference_cluster_smoke

kubectl -n xscope-system get inferencepool demo-pool
kubectl -n xscope-system logs deployment/inference-serving -c envoy --tail=20
```

The smoke uses `xscope-local-secret` by default; `XSCOPE_TEST_API_KEY` can override
it with a key allowing xscope-demo. It verifies 401/403, SSE incrementality and
final usage, spoofed destination stripping, JSON completion, cancellation in
Runtime logs, selected endpoint in serving logs and persisted usage outcomes.
It is deliberately restricted to the current `docker-desktop` context.

The public URLs remain console `http://localhost:30081` and inference
`http://localhost:30082/v1/chat/completions`. The internal serving port is not
exposed as a NodePort; do not use it as an unauthenticated public inference API.

Observed upstream diagnostic: v0.9.0 may log `Request latency values are invalid
for TPOT calculation` for the nearly instantaneous non-streaming echo response
(first-token/completion timestamps differ by microseconds). The response and
usage/ledger checks still pass; do not use this development echo's TPOT metric
as a production latency baseline. Real-engine metrics validation remains ahead.

## Upstream references

- [llm-d router v0.9.0](https://github.com/llm-d/llm-d-router/tree/v0.9.0)
- [Official standalone proxy template](https://github.com/kubernetes-sigs/gateway-api-inference-extension/blob/v1.5.0/config/charts/standalone/values.yaml)
- [Official v0.9.0 scheduling profiles](https://github.com/llm-d/llm-d-router/blob/v0.9.0/config/charts/routerlib/templates/_config.yaml)
