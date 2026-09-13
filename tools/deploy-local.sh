#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repository_root}"
case "${1:-}" in
  --reset-business-data|"") ;;
  --accept-volatile-cutover) ;;
  *) echo "usage: $0 [--reset-business-data|--accept-volatile-cutover]" >&2; exit 1 ;;
esac
if [[ "${1:-}" != "--reset-business-data" ]]; then
  if [[ "${1:-}" == "--accept-volatile-cutover" ]]; then
    bazel run //tools:prepare_usage_cutover -- --accept-volatile-cutover
  else
    bazel run //tools:prepare_usage_cutover
  fi
fi
# rules_rust/rules_python/rules_js compile applications; rules_pkg and
# rules_oci produce all five images. Docker provisions only the Linux Bazel tool
# environment and loads the resulting image archives.
bash tools/bazel-linux.sh images
bazel build //deploy/inference:crds
crd_bundle="$(bazel info bazel-bin)/deploy/inference/inference-crds.yaml"
kubectl apply --server-side -f "${crd_bundle}"
kubectl wait --for=condition=Established --timeout=60s \
  crd/inferencepools.inference.networking.k8s.io \
  crd/inferenceobjectives.llm-d.ai crd/inferencemodelrewrites.llm-d.ai

if [[ "${1:-}" == "--reset-business-data" ]]; then
  bash tools/reset-local-data.sh
  # Change the template while replicas=0. Applying replicas=1 and immediately
  # restarting again would launch two control planes against an empty schema.
  kubectl -n xscope-system rollout restart deployment/control-plane deployment/gateway
fi

# Preserve existing Grafana login/encryption keys across redeployments.
kubectl create namespace xscope-system --dry-run=client -o yaml | kubectl apply -f -
bazel run //tools:local_secrets
bazel run //tools:observability_admin
# The control plane must remain available while a volatile Gateway drains.
# Full local redeploy already has a financial-writer maintenance gap; stop the
# Gateway FIRST, otherwise a planned upgrade would strand its HTTP outbox.
if [[ "${1:-}" != "--reset-business-data" ]] && kubectl -n xscope-system get deployment/gateway >/dev/null 2>&1; then
  kubectl -n xscope-system scale deployment/gateway --replicas=0
  kubectl -n xscope-system wait --for=delete pod -l app.kubernetes.io/name=gateway --timeout=90s
fi
# Exclude old financial writers before starting projection-aware control planes.
# This is a short fail-closed maintenance gap, NOT a financial data reset.
if [[ "${1:-}" != "--reset-business-data" ]] && kubectl -n xscope-system get deployment/control-plane >/dev/null 2>&1; then
  bazel run //tools:deploy_billing_protocol -- --quiesce-only
fi
kubectl apply -k "${repository_root}/deploy/k8s/overlays/local"
# Gateway was stopped above (or by reset). Applying replicas=1 already starts
# the newly built image; do not immediately replace that fresh process again.
kubectl -n xscope-system rollout restart deployment/cluster-agent deployment/runtime deployment/operator
kubectl -n xscope-system rollout status statefulset/postgres --timeout=180s
kubectl -n xscope-system rollout status deployment/redis --timeout=180s
kubectl -n xscope-system rollout status deployment/jaeger --timeout=180s
kubectl -n xscope-system rollout status deployment/prometheus --timeout=180s
kubectl -n xscope-system rollout status deployment/grafana --timeout=180s
kubectl -n xscope-system rollout status deployment/keycloak --timeout=300s
kubectl -n xscope-system rollout status deployment/control-plane --timeout=180s
kubectl -n xscope-system rollout status deployment/cluster-agent --timeout=180s
if kubectl -n xscope-system get deployment/xscope-member-agent >/dev/null 2>&1; then
  kubectl -n xscope-system rollout restart deployment/xscope-member-agent
  kubectl -n xscope-system rollout status deployment/xscope-member-agent --timeout=180s
fi
kubectl -n xscope-system rollout status deployment/inference-serving --timeout=180s
kubectl -n xscope-system rollout status deployment/gateway --timeout=180s
kubectl -n xscope-system rollout status deployment/runtime --timeout=180s
kubectl -n xscope-system rollout status deployment/operator --timeout=180s
kubectl -n xscope-system rollout status deployment/oauth2-proxy --timeout=180s
