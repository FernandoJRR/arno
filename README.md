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

## Chain-store logs (audit, transcript)

The harness's tamper-evident logs live at `./data/` in every mode
(SPEC §11 decision #15):

- `arno-audit.jsonl` — executed frozen writes (§12.1)
- `arno-transcript.jsonl` — full conversation/tool-call narrative (§12.2)
- `arno-tool-policy.json` — persisted tool-safety classification (hand-editable)

Inspect them with plain tools — no `docker run` indirection:

```sh
jq -c . data/arno-transcript.jsonl | tail -5
```

When the active file exceeds its rotate threshold (`AUDIT_LOG_ROTATE_BYTES`,
2 MiB default; transcript 10 MiB) it is renamed to
`arno-<store>-<UTC timestamp>.jsonl` beside the active file **at next boot**,
and the new active file's first entry chains onto the old tail. The oldest
segments beyond `LOG_KEEP_SEGMENTS` (12) are pruned. Verify both chains
runtime:

```sh
curl -s -H "Authorization: Bearer <client-token>" \
    http://127.0.0.1:8080/v1/logs/verify
# {"audit":{"ok":true,...},"transcript":{"ok":true,...},"ok":true}
```

Off-host backup (catches tail-truncation the chain alone cannot):

```sh
docker/backup-logs.sh   # rsyncs ./data to $ARNO_BACKUP_DEST (~/arno-logs-backup)
```

Schedule it from host cron — e.g. `0 3 * * *` for a nightly copy.

## Tool-recognition health

Small local models (qwen3:4b-instruct) sometimes assert they executed an
action without ever calling a tool. The harness defends against this with
three output-side layers (SPEC §11 decision #16): deterministic sampling
(`OLLAMA_SEED`/`OLLAMA_TOP_K`), rescue of tool calls the model emits as
text, and a success-claim guard that withholds false "done!" replies and
forces a real call. None of these require the user to phrase orders any
particular way.

Check the live false-claim rate from the transcript:

```sh
# finals claiming success:
jq -r 'select(.kind=="assistant_final")|.text' data/arno-transcript.jsonl \
    | grep -ci "successfully\|has been created\|has been applied"
# tool calls that actually ran:
jq -r 'select(.kind=="tool_call")|.tool' data/arno-transcript.jsonl | wc -l
```

If false claims persist after the harness layers, try a model swap (ops,
no code): `qwen3:4b` (non-instruct tag — some `-instruct` variants silently
lose native tool mode) or `qwen3:8b` (compose default). Verify capability
first: `ollama show <model>` must list `tools` under Capabilities.
`OLLAMA_SEED=0`/`OLLAMA_TOP_K=0` restore provider-default sampling if you
want more answer variety.

## Configuration

All configuration is env-only — no config files, no CLI flags for secrets.
Every component fails loudly on an unrecognized env var under its own
namespace. See `SPEC.md §5.7` for the full list of knobs and `STACK.md §7`
for which component owns which variable. `.env.example` is a starting point.
