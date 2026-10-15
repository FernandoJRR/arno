# Contract C backend — read-only OCR (image → text) MCP server (STACK.md §6).
FROM rust:1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p mcp-ocr

FROM debian:bookworm-slim
ARG MCP_OCR_PORT=9002
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        tesseract-ocr tesseract-ocr-eng tesseract-ocr-spa \
        curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    # Same uid as the harness container (docker/harness.Dockerfile): both
    # mount the shared `attachments` volume (compose.yaml) and the harness
    # writes each file 0600 — a distinct uid here would make it unreadable
    # from this container instead of loosening the mode to group-readable.
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
COPY --from=build /build/target/release/mcp-ocr /usr/local/bin/mcp-ocr
USER arno
# Bind 0.0.0.0 inside the container: the compose network itself is the trust
# boundary; no ports are published to the host (SPEC decision #2 scope).
ENV MCP_OCR_PORT=${MCP_OCR_PORT}
ENV MCP_OCR_BIND=0.0.0.0:${MCP_OCR_PORT}
EXPOSE ${MCP_OCR_PORT}
ENTRYPOINT ["mcp-ocr"]
