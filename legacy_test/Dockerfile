# syntax=docker/dockerfile:1.7

FROM rust:1.89-slim-bullseye AS base
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev ca-certificates git \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /app

FROM base AS chef
RUN cargo install cargo-chef --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS deps
COPY --from=planner /app/recipe.json recipe.json
# Use stable cache IDs so the builder stage can reuse the same caches
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/app/target,id=cargo-target,sharing=locked \
    cargo chef cook --debug --recipe-path recipe.json

FROM base AS builder
# Do NOT copy from deps; cache mounts are not part of image layers
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=cargo-registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,id=cargo-git,sharing=locked \
    --mount=type=cache,target=/app/target,id=cargo-target,sharing=locked \
    cargo build --example participant \
    && mkdir -p /dist \
    && cp /app/target/debug/examples/participant /dist/participant

FROM debian:bullseye-slim AS runtime-base
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl1.1 iperf3 iproute2 iputils-ping \
  && rm -rf /var/lib/apt/lists/*
WORKDIR /app
ENV RUST_BACKTRACE=1

FROM runtime-base AS participant-runtime
COPY --from=builder /dist/participant /usr/local/bin/participant
CMD ["participant"]
CMD ["creator"]