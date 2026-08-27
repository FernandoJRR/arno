# Contract C backend — read-only Linux diagnostics MCP server (STACK.md §6).
FROM rust:1-slim-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release -p mcp-linux

FROM debian:bookworm-slim
ARG MCP_LINUX_PORT=9001
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10002 arno
COPY --from=build /build/target/release/mcp-linux /usr/local/bin/mcp-linux
USER arno
# Bind 0.0.0.0 inside the container: the compose network itself is the trust
# boundary; no ports are published to the host (SPEC decision #2 scope).
ENV MCP_LINUX_PORT=${MCP_LINUX_PORT}
ENV MCP_LINUX_BIND=0.0.0.0:${MCP_LINUX_PORT}
EXPOSE ${MCP_LINUX_PORT}
ENTRYPOINT ["mcp-linux"]
