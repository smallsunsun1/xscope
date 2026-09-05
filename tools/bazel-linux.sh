#!/usr/bin/env bash
set -euo pipefail
repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repository_root}"
target_arch="${XSCOPE_BUILD_ARCH:-$(kubectl get nodes -o jsonpath='{.items[0].status.nodeInfo.architecture}')}"
case "${target_arch}" in amd64|arm64) ;; *) echo "unsupported architecture: ${target_arch}" >&2; exit 1;; esac
docker build --platform "linux/${target_arch}" --build-arg "TARGETARCH=${target_arch}" \
  -f deploy/docker/bazel-toolchain.Dockerfile -t "xscope/bazel-toolchain:${target_arch}" .
repository_cache="$(bazel info repository_cache)"
# Keep Linux outputs and caches separate from the host's Bazel output tree.
docker_args=(run --rm --platform "linux/${target_arch}"
  --mount "type=bind,src=${repository_root},dst=/workspace"
  --mount "type=volume,src=xscope-bazel-${target_arch},dst=/bazel-cache"
  --mount "type=bind,src=${repository_cache},dst=/repository-cache")
if [[ "${1:-}" == "images" ]]; then
  docker "${docker_args[@]}" --entrypoint bash "xscope/bazel-toolchain:${target_arch}" tools/build-images-in-container.sh
  for name in control-plane gateway cluster-agent operator runtime; do
    docker load --input "${repository_root}/.build/images/${name}_load.tar"
  done
  exit 0
fi
docker "${docker_args[@]}" \
  "xscope/bazel-toolchain:${target_arch}" "$@" \
  --repository_cache=/repository-cache --symlink_prefix=/bazel-cache/links/bazel-
