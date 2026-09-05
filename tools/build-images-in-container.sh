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
bazel "${bazel_args[@]}" build "${build_args[@]}" //deploy/images:tarballs
mkdir -p /workspace/.build/images
exec_root="$(bazel "${bazel_args[@]}" info execution_root)"
while IFS= read -r artifact; do
  artifact_directory="${artifact%/*}"
  archive_name="${artifact_directory##*/}"
  cp "${exec_root}/${artifact}" "/workspace/.build/images/${archive_name}.tar"
done < <(bazel "${bazel_args[@]}" cquery "${build_args[@]}" //deploy/images:tarballs --output=files)
