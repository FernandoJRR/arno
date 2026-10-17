# syntax=docker/dockerfile:1
#
# Dev-only build (STACK.md §6): one shared compile pass for the whole
# workspace, cached across `docker compose build` runs via BuildKit mount
# caches, feeding four thin runtime stages below — instead of each service's
# prod Dockerfile independently recompiling the entire dependency graph
# (tokio, axum, rmcp, teloxide, ...) from zero on every change.
#
# Not used in prod — docker/{harness,linux-mcp,ocr-mcp,telegram-adapter}.Dockerfile
# stay the single-binary, no-cache-mount source of truth for release images.
# compose.dev.yaml points each service at the matching stage here instead.

FROM rust:1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# Registry/git caches avoid re-fetching crates.io on every build; the target
# cache carries cargo's own incremental artifacts across builds, so an
# unchanged dependency graph — or a change confined to one crate — only
# recompiles what actually changed instead of the whole workspace.
# `sharing=locked` serializes access since compose may build the four
# services that share this stage concurrently.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/build/target,sharing=locked \
    cargo build --release --workspace \
    && mkdir -p /out \
    && cp target/release/harness target/release/arno-replay target/release/mcp-linux target/release/mcp-ocr target/release/adapter-telegram /out/
# Binaries are copied out of the cache-mounted target dir here: that mount's
# contents do not persist into the image layer once this RUN ends.

# ---- harness runtime (mirrors docker/harness.Dockerfile) ----
FROM debian:bookworm-slim AS harness
ARG HARNESS_API_PORT=8080
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 arno \
    && mkdir -p /data /attachments \
    && chown arno:arno /data /attachments
COPY --from=build /out/harness /usr/local/bin/harness
COPY --from=build /out/arno-replay /usr/local/bin/arno-replay
USER arno
ENV HARNESS_API_BIND=0.0.0.0:${HARNESS_API_PORT}
EXPOSE ${HARNESS_API_PORT}
ENTRYPOINT ["harness"]

# ---- mcp-linux runtime (mirrors docker/linux-mcp.Dockerfile) ----
FROM debian:bookworm-slim AS mcp-linux
ARG MCP_LINUX_PORT=9001
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10002 arno
COPY --from=build /out/mcp-linux /usr/local/bin/mcp-linux
USER arno
ENV MCP_LINUX_PORT=${MCP_LINUX_PORT}
ENV MCP_LINUX_BIND=0.0.0.0:${MCP_LINUX_PORT}
EXPOSE ${MCP_LINUX_PORT}
ENTRYPOINT ["mcp-linux"]

# ---- mcp-ocr runtime (mirrors docker/ocr-mcp.Dockerfile) ----
FROM debian:bookworm-slim AS mcp-ocr
ARG MCP_OCR_PORT=9002
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        tesseract-ocr tesseract-ocr-eng tesseract-ocr-spa \
        curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 arno \
    && mkdir -p /attachments \
    && chown arno:arno /attachments
# tessdata_best eng+spa, pinned and checksummed (SPEC §11 #19): measurably
# better than Debian's fast models on faded bill digits. The fast packages
# stay installed; dropping TESSDATA_PREFIX reverts to them.
ARG TESSDATA_BEST_COMMIT=e12c65a915945e4c28e237a9b52bc4a8f39a0cec
RUN mkdir -p /usr/local/share/tessdata-best \
    && cd /usr/local/share/tessdata-best \
    && for l in eng spa; do \
        curl -fsSL -o "$l.traineddata" \
          "https://raw.githubusercontent.com/tesseract-ocr/tessdata_best/${TESSDATA_BEST_COMMIT}/$l.traineddata"; \
       done \
    && printf '%s  %s\n' \
        8280aed0782fe27257a68ea10fe7ef324ca0f8d85bd2fd145d1c2b560bcb66ba eng.traineddata \
        e2c1ffdad8b30f26c45d4017a9183d3a7f9aa69e59918be4f88b126fac99ab2c spa.traineddata \
       | sha256sum -c -
ENV TESSDATA_PREFIX=/usr/local/share/tessdata-best
COPY --from=build /out/mcp-ocr /usr/local/bin/mcp-ocr
USER arno
ENV MCP_OCR_PORT=${MCP_OCR_PORT}
ENV MCP_OCR_BIND=0.0.0.0:${MCP_OCR_PORT}
EXPOSE ${MCP_OCR_PORT}
ENTRYPOINT ["mcp-ocr"]

# ---- telegram-adapter runtime (mirrors docker/telegram-adapter.Dockerfile) ----
FROM debian:bookworm-slim AS adapter-telegram
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates procps \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10003 arno
COPY --from=build /out/adapter-telegram /usr/local/bin/adapter-telegram
USER arno
ENTRYPOINT ["adapter-telegram"]
