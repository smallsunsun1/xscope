FROM mirror.gcr.io/library/rust:1.94-bookworm AS builder
WORKDIR /workspace
RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
COPY rust/Cargo.toml rust/Cargo.lock ./
COPY rust/gateway gateway
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/workspace/target \
    cargo build --release --locked -p xscope-gateway \
    && cp /workspace/target/release/xscope-gateway /tmp/xscope-gateway

FROM mirror.gcr.io/library/debian:bookworm-slim
RUN groupadd --system --gid 65532 nonroot && useradd --system --uid 65532 --gid nonroot --home /nonexistent nonroot
COPY --from=builder /tmp/xscope-gateway /usr/local/bin/xscope-gateway
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/xscope-gateway"]
