# Only the build environment is provisioned here. Applications are compiled and
# packaged by Bazel in tools/bazel-linux.sh, including Python and OCI layers.
FROM mirror.gcr.io/library/rust:1.94-bookworm
ARG TARGETARCH
ARG BAZEL_VERSION=9.2.0
RUN apt-get update && apt-get install --yes --no-install-recommends python3 unzip zip \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; case "$TARGETARCH" in arm64) arch=arm64;; amd64) arch=x86_64;; *) exit 1;; esac; \
    url="https://releases.bazel.build/${BAZEL_VERSION}/release/bazel-${BAZEL_VERSION}-linux-${arch}"; \
    curl -fsSL --retry 3 "$url" -o /usr/local/bin/bazel; \
    curl -fsSL --retry 3 "$url.sha256" -o /tmp/bazel.sha256; \
    cd /usr/local/bin; expected="$(cut -d ' ' -f 1 /tmp/bazel.sha256)"; \
    printf '%s  bazel\n' "$expected" | sha256sum -c -; chmod 755 bazel
WORKDIR /workspace
ENV USER=root
ENTRYPOINT ["bazel", "--output_user_root=/bazel-cache", "--host_jvm_args=-Xmx1536m"]
