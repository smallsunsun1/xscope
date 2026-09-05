#!/usr/bin/env bash
set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
build_root="${repository_root}/.build"
target_arch="$(kubectl get nodes -o jsonpath='{.items[0].status.nodeInfo.architecture}')"

case "${target_arch}" in
  amd64|arm64) ;;
  *)
    echo "unsupported Kubernetes node architecture: ${target_arch}" >&2
    exit 1
    ;;
esac

mkdir -p "${build_root}"

bazel build //web/console:console
mkdir -p "${build_root}/console"
rsync -a --delete "${repository_root}/bazel-bin/web/console/dist/" "${build_root}/console/"

(
  cd "${repository_root}/go"
  CGO_ENABLED=0 GOOS=linux GOARCH="${target_arch}" go build -trimpath -ldflags="-s -w" -o "${build_root}/control-plane" ./control-plane/cmd/server
  CGO_ENABLED=0 GOOS=linux GOARCH="${target_arch}" go build -trimpath -ldflags="-s -w" -o "${build_root}/operator" ./operator/cmd/operator
)

docker build -f "${repository_root}/deploy/docker/control-plane.Dockerfile" -t xscope/control-plane:dev "${repository_root}"
docker build -f "${repository_root}/deploy/docker/operator.Dockerfile" -t xscope/operator:dev "${repository_root}"
docker build -f "${repository_root}/deploy/docker/gateway.Dockerfile" -t xscope/gateway:dev "${repository_root}"
docker build -f "${repository_root}/deploy/docker/runtime.Dockerfile" -t xscope/runtime:dev "${repository_root}"

kubectl apply -k "${repository_root}/deploy/k8s/overlays/local"
kubectl -n xscope-system rollout restart deployment/control-plane deployment/gateway deployment/runtime deployment/operator
kubectl -n xscope-system rollout status statefulset/postgres --timeout=180s
kubectl -n xscope-system rollout status deployment/keycloak --timeout=300s
kubectl -n xscope-system rollout status deployment/control-plane --timeout=180s
kubectl -n xscope-system rollout status deployment/gateway --timeout=180s
kubectl -n xscope-system rollout status deployment/runtime --timeout=180s
kubectl -n xscope-system rollout status deployment/operator --timeout=180s
kubectl -n xscope-system rollout status deployment/oauth2-proxy --timeout=180s
