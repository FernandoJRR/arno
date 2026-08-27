# Part A v1 — Telegram interface adapter (STACK.md §6).
# Multi-stage: rust:slim builder → debian-slim runtime, one binary, non-root.
FROM rust:1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p adapter-telegram

FROM debian:bookworm-slim
# Outbound-only (long-polling `getUpdates`) — no port to publish or expose.
# procps supplies pgrep, used by the compose process-liveness healthcheck
# (STACK.md §6 — this adapter has no HTTP surface to probe).
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10003 arno
COPY --from=build /build/target/release/adapter-telegram /usr/local/bin/adapter-telegram
USER arno
ENTRYPOINT ["adapter-telegram"]
