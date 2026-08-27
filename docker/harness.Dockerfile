# Part B core — Harness API server (STACK.md §6).
# Multi-stage: rust:slim builder → debian-slim runtime, one binary, non-root.
FROM rust:1-slim-bookworm AS build
WORKDIR /build
# Manifests first for dependency-layer caching, then sources.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p harness

FROM debian:bookworm-slim
ARG HARNESS_API_PORT=8080
# curl exists solely for the compose healthcheck (STACK.md §6).
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 arno
COPY --from=build /build/target/release/harness /usr/local/bin/harness
USER arno
# HARNESS_API_PORT stays a build-time ARG only — the harness process itself
# rejects any unrecognized HARNESS_*-prefixed runtime env var (SPEC §5.7).
ENV HARNESS_API_BIND=0.0.0.0:${HARNESS_API_PORT}
EXPOSE ${HARNESS_API_PORT}
ENTRYPOINT ["harness"]
