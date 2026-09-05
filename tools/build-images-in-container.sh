#!/usr/bin/env bash
set -euo pipefail
cd /workspace
bazel_args=(--output_user_root=/bazel-cache --host_jvm_args=-Xmx1536m)
build_args=(--repository_cache=/repository-cache --symlink_prefix=/bazel-cache/links/bazel- --config=release --jobs=4)
bazel "${bazel_args[@]}" build "${build_args[@]}" //deploy/images:tarballs
mkdir -p /workspace/.build/images
exec_root="$(bazel "${bazel_args[@]}" info execution_root)"
while IFS= read -r artifact; do
  artifact_directory="${artifact%/*}"
  archive_name="${artifact_directory##*/}"
  cp "${exec_root}/${artifact}" "/workspace/.build/images/${archive_name}.tar"
done < <(bazel "${bazel_args[@]}" cquery "${build_args[@]}" //deploy/images:tarballs --output=files)
