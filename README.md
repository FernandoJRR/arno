# Arno — Home Server Harness

A local-first, read-only-by-default agent harness for a home server: interface
adapters (Telegram, CLI) talk to a core harness over a stable HTTP contract;
the harness talks to tool backends over MCP. Rust end to end, orchestrated
with a small local model (Ollama).

Three independently replaceable parts, joined by two stable contracts:

```
Interface adapters  ──Contract 1 (Harness API)──▶  Harness core  ──Contract 2 (MCP)──▶  Tool backends
  (Telegram, CLI)                                  (queue, orchestrator,                (mcp-linux, …)
                                                     MCP client, ModelProvider)
```

See [`SPEC.md`](SPEC.md) for the full design and [`STACK.md`](STACK.md) for
the stack/deployment details. [`AGENTS.md`](AGENTS.md) documents the hard
rules for anyone (human or agent) changing this codebase.

## Status

M1 is done: MCP registry + tool dispatch, the `mcp-linux` read-only
diagnostics backend, and the Telegram adapter are all implemented and tested.
See `SPEC.md §9` for the milestone list.

## Workspace layout

| Crate | Role |
|---|---|
| `crates/contract` | Wire types shared between the harness and every adapter (Order, Response, ErrorCode). |
| `crates/harness` | Core binary: axum API, FIFO queue, stores, orchestrator, MCP client, ModelProvider. |
| `crates/adapter-cli` | Throwaway stdin/stdout adapter — useful for local testing. |
| `crates/adapter-telegram` | Telegram long-polling interface adapter. |
| `crates/mcp-linux` | First-party MCP backend: read-only Linux diagnostics (disk, services, ports). |

## Quick start

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --all -- --check
```

Run the M0 CLI loop against a local harness:

```sh
RUST_LOG=debug cargo run -p adapter-cli -- --token <client-token>
```

## Running with Docker Compose

```sh
cp .env.example .env   # fill in secrets — see comments in the file
docker compose up -d --build
```

This starts `harness`, `mcp-linux`, and `telegram-adapter` on an isolated
compose network. `harness`'s API is published to `127.0.0.1` only; the
Telegram adapter is outbound-only (long-polling) and publishes no ports.

## Configuration

All configuration is env-only — no config files, no CLI flags for secrets.
Every component fails loudly on an unrecognized env var under its own
namespace. See `SPEC.md §5.7` for the full list of knobs and `STACK.md §7`
for which component owns which variable. `.env.example` is a starting point.
