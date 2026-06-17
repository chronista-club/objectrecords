# syntax=docker/dockerfile:1.7
# Multi-stage build for `objectrecords-api` (Phase 4.D).
#
# Layout:
#   - `builder`: Rust 1.95 toolchain. Compiles the workspace's `api`
#     binary statically against musl-equivalent glibc (debian).
#   - `runtime`: debian:bookworm-slim with TLS root certs + a non-root
#     `app` user. Holds only the compiled binary and the libraries it
#     needs at runtime.
#
# Phase 4.D Sub-Q5/Q6/Q7 alignment:
#   - multi-stage (Sub-Q5)
#   - cross-compile-friendly via `--platform linux/amd64` on
#     `docker buildx build` (Sub-Q6 — Mac darwin/arm64 host can target
#     the target host's linux/amd64)
#   - distribution path is `docker save | scp | docker load` for
#     Phase 4.D first deploy (Sub-Q7); ghcr.io migration is a follow-up.

ARG RUST_VERSION=1.95
ARG DEBIAN_VERSION=bookworm

# =============================================================================
# builder stage
# =============================================================================

FROM rust:${RUST_VERSION}-slim-${DEBIAN_VERSION} AS builder

# System packages required to compile the workspace's transitive deps.
# `pkg-config` covers anything that probes for system libraries (some
# build scripts touch it even when they fall through to vendored deps).
RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Copy the whole workspace (the .dockerignore drops target/ etc.).
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

# Release build, cargo workspace selects the api crate's binary.
# `--bin objectrecords-api` matches the `[[bin]]` declaration in
# `crates/objectrecords-api/Cargo.toml`.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release -p objectrecords-api --bin objectrecords-api \
    && cp /build/target/release/objectrecords-api /usr/local/bin/objectrecords-api

# =============================================================================
# runtime stage
# =============================================================================

FROM debian:${DEBIAN_VERSION}-slim AS runtime

# Runtime dependencies:
#   - ca-certificates: HTTPS calls (e.g., creo-id JWKS in Phase 4.1)
#   - libssl3 + libgcc-s1: pulled by the surrealdb client when rustls is
#     enabled but glibc loader still references libssl symbols via
#     transitive deps. Slim image already has libgcc-s1; libssl3 is a
#     belt-and-braces include.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home --uid 1000 --shell /bin/bash app

# Bring in the compiled binary.
COPY --from=builder /usr/local/bin/objectrecords-api /usr/local/bin/objectrecords-api

USER app
WORKDIR /home/app

# Server config defaults — a reverse proxy override at deploy time.
ENV OBJECTRECORDS_API_BIND=0.0.0.0:8000 \
    RUST_LOG=info,objectrecords_api=debug

EXPOSE 8000

ENTRYPOINT ["/usr/local/bin/objectrecords-api"]
