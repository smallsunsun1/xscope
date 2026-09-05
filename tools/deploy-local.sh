#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repository_root}"
case "${1:-}" in
  --reset-business-data|"") ;;
  *) echo "usage: $0 [--reset-business-data]" >&2; exit 1 ;;
esac
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
bazel run //tools:observability_admin
kubectl apply -k "${repository_root}/deploy/k8s/overlays/local"
if [[ "${1:-}" != "--reset-business-data" ]]; then
  kubectl -n xscope-system rollout restart deployment/control-plane deployment/gateway
fi
kubectl -n xscope-system rollout restart deployment/cluster-agent deployment/runtime deployment/operator
kubectl -n xscope-system rollout status statefulset/postgres --timeout=180s
kubectl -n xscope-system rollout status deployment/redis --timeout=180s
kubectl -n xscope-system rollout status deployment/jaeger --timeout=180s
kubectl -n xscope-system rollout status deployment/prometheus --timeout=180s
kubectl -n xscope-system rollout status deployment/grafana --timeout=180s
kubectl -n xscope-system rollout status deployment/keycloak --timeout=300s
kubectl -n xscope-system rollout status deployment/control-plane --timeout=180s
kubectl -n xscope-system rollout status deployment/cluster-agent --timeout=180s
kubectl -n xscope-system rollout status deployment/inference-serving --timeout=180s
kubectl -n xscope-system rollout status deployment/gateway --timeout=180s
kubectl -n xscope-system rollout status deployment/runtime --timeout=180s
kubectl -n xscope-system rollout status deployment/operator --timeout=180s
kubectl -n xscope-system rollout status deployment/oauth2-proxy --timeout=180s
