# syntax=docker/dockerfile:1.7

# Keep this tag aligned with rust-toolchain.toml.
FROM rust:1.96.0-bookworm AS builder

WORKDIR /workspace
RUN apt-get update && \
    apt-get install --no-install-recommends --yes cmake && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

RUN --mount=type=cache,id=yg-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=yg-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=yg-target,target=/workspace/target \
    cargo build --locked --release --package yg-cli --bin yg && \
    cp /workspace/target/release/yg /tmp/yg

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install --no-install-recommends --yes ca-certificates git tar && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 yggdrasil && \
    useradd --system --uid 10001 --gid yggdrasil --home-dir /var/lib/yggdrasil yggdrasil && \
    install --directory --owner=yggdrasil --group=yggdrasil \
        /var/lib/yggdrasil/git /var/lib/yggdrasil/shard-cache

COPY --from=builder /tmp/yg /usr/local/bin/yg

ENV YG_LISTEN=0.0.0.0:7311 \
    YG_GIT_CACHE=/var/lib/yggdrasil/git \
    YG_SHARD_CACHE=/var/lib/yggdrasil/shard-cache

EXPOSE 7311
USER 10001:10001

ENTRYPOINT ["/usr/local/bin/yg"]
CMD ["serve", "--role=all"]
