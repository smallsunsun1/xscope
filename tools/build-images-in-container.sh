#!/usr/bin/env bash
set -euo pipefail
cd /workspace
if [[ -d /host-cargo-registry ]]; then
  mkdir -p /bazel-cache/cargo-home/registry
  cp -a /host-cargo-registry/index /host-cargo-registry/cache /bazel-cache/cargo-home/registry/
fi
bazel_args=(--output_user_root=/bazel-cache --host_jvm_args=-Xmx1536m)
build_args=(--repository_cache=/repository-cache --repo_contents_cache=/bazel-cache/repo-contents
  --symlink_prefix=/bazel-cache/links/bazel- --config=release --jobs=4 --show_progress_rate_limit=10)
if [[ $# == 0 ]]; then set -- control-plane gateway cluster-agent operator runtime; fi
targets=()
for name in "$@"; do
  case "${name}" in control-plane|gateway|cluster-agent|operator|runtime) ;; *) echo "unknown image: ${name}" >&2; exit 1;; esac
  targets+=("//deploy/images:${name}_tarball")
done
bazel "${bazel_args[@]}" build "${build_args[@]}" "${targets[@]}"
mkdir -p /workspace/.build/images
# The selected filegroups expose these OCI tarballs. Copy only after the build
# succeeds; a failed second cquery inside process substitution used to be hidden
# from set -e and could leave an old image archive to be loaded accidentally.
for name in "$@"; do
  # Bazel outputs may be read-only. Never overwrite a prior read-only inode
  # through Docker Desktop's bind mount, or expose a partially copied archive.
  image_archive_tmp="$(mktemp "/workspace/.build/images/.${name}_load.XXXXXX")"
  cp "/bazel-cache/links/bazel-bin/deploy/images/${name}_load/tarball.tar" "${image_archive_tmp}"
  chmod 0644 "${image_archive_tmp}"
  mv -f "${image_archive_tmp}" "/workspace/.build/images/${name}_load.tar"
done
