#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repository_root}"
# rules_rust/rules_go/rules_python/rules_js compile applications; rules_pkg and
# rules_oci produce all five images. Docker provisions only the Linux Bazel tool
# environment and loads the resulting image archives.
bash tools/bazel-linux.sh images

case "${1:-}" in
  --reset-business-data) bash tools/reset-local-data.sh ;;
  "") ;;
  *) echo "usage: $0 [--reset-business-data]" >&2; exit 1 ;;
esac

kubectl apply -k "${repository_root}/deploy/k8s/overlays/local"
kubectl -n xscope-system rollout restart deployment/control-plane deployment/cluster-agent deployment/gateway deployment/runtime deployment/operator
kubectl -n xscope-system rollout status statefulset/postgres --timeout=180s
kubectl -n xscope-system rollout status deployment/redis --timeout=180s
kubectl -n xscope-system rollout status deployment/keycloak --timeout=300s
kubectl -n xscope-system rollout status deployment/control-plane --timeout=180s
kubectl -n xscope-system rollout status deployment/cluster-agent --timeout=180s
kubectl -n xscope-system rollout status deployment/gateway --timeout=180s
kubectl -n xscope-system rollout status deployment/runtime --timeout=180s
kubectl -n xscope-system rollout status deployment/operator --timeout=180s
kubectl -n xscope-system rollout status deployment/oauth2-proxy --timeout=180s
