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
| `crates/mcp-ocr` | First-party MCP backend: read-only OCR (`extract_text`) — Tesseract, CPU-only. |

## Decisions (approving writes)

When the model proposes a write, the harness does not run it. The reply
carries a `decision` (prompt, argument details, `approve`/`reject` options)
and the order ends. Adapters render it natively — Telegram shows inline
buttons, the CLI a numbered menu — and send the choice back as
`decision_answer` in a new order. Only `approve` on a live decision executes
the frozen payload; free text or a plain "yes" never does (SPEC §4.3). A new
adapter only needs to render `decision.options` and echo the chosen `value`
with `decision.id`.

## Answers

Answers are short by default (a few lines of key facts; ask for more
detail). When a tool reports `checks` — e.g. OCR noticing that a faded
"30.46" is probably 80.46 — they are appended to the answer verbatim (OCR
corrects the text itself when the evidence is strong), and
numbers the answer states that appear in no tool result or message are
named in a note (SPEC §11 #20).

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
cp config/plugins/securo.env.example config/plugins/securo.env   # Securo token
docker compose up -d --build
```

This starts `harness`, `mcp-linux`, `mcp-ocr`, and `telegram-adapter` on an
isolated compose network. `harness`'s API is published to `127.0.0.1` only;
the Telegram adapter is outbound-only (long-polling) and publishes no ports.

Send a photo of a bill or document and ask about it: the harness persists the
image to a shared ephemeral scratch volume, tells the model a handle exists,
and the model calls `ocr.extract_text` (Tesseract, read-only, no GPU) to read
it back as text — see `SPEC.md §4.4` for the attachment-ingestion mechanism.

## Plugins

Every MCP backend is a plugin: a manifest in `config/plugins/<id>.toml`
plus, if it needs secrets, `config/plugins/<id>.env` (gitignored). See
`SPEC.md §4.5` for the format. The harness reads the directory at boot and
on `SIGHUP`, between orders.

Adding a plugin:

1. Write `config/plugins/<id>.toml` (and `<id>.env` for declared secrets).
   For `kind = "container"`, run the service yourself; the disclosure report
   below includes a compose snippet for it.
2. Reload: `docker compose kill -s HUP harness` (or restart).
3. Review the disclosure: `docker compose logs harness | grep "plugin disclosure"`
   or `jq . data/arno-plugins-report.json`. Check egress, volumes, secret
   names, and the safe/conditional/destructive counts.
4. A new plugin is **untrusted**: every call, reads included, asks for
   approval. To promote it, edit `data/arno-tool-policy.json`:
   `"plugins": {"<id>": {"level": "trusted", "source": "operator", ...}}`,
   then reload. Any later edit to the manifest, or a tool schema change,
   makes it untrusted again.

`MCP_SERVERS` still works for one release (with a boot warning); the same id
in both places fails at boot. A plugin that can't be reached (or is missing
a secret) is skipped, and the log says `N of M enabled plugins could not be
reached — running without them`.

Photos: when exactly one tool takes attachments (OCR today), the harness
reads the photo with it before the model answers, so a bare caption like
"what's this?" still works (SPEC §11 #25).

### Tool-group router

`ROUTER_ENABLED=true` makes one small, structured call per order to pick a
tool group, so the model sees a few tools instead of the whole catalogue.
Measure before turning it on:

```sh
docker compose exec harness arno-replay --runs 3            # full vs routed
docker compose exec harness arno-replay --mode routed --only ocr-what
```

It prints the first tool picked per case, prompt tokens and latency, plus a
summary per mode. Turn the router on only if routed accuracy is equal or
better with fewer tokens and lower latency. The cases live in
`crates/harness/replay/cases.toml` (`--cases FILE` to use another file).

Router picks in the transcript:

```sh
jq -r 'select(.kind=="router_pick")|"\(.group) \(.latency_ms)ms"' data/arno-transcript.jsonl \
    | sort | uniq -c | sort -rn
# tool usage counts:
jq -r 'select(.kind=="tool_call")|.tool' data/arno-transcript.jsonl | sort | uniq -c | sort -rn
```

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

All configuration is env-only — no CLI flags for secrets — except plugin
manifests and their env files in `PLUGINS_DIR` (see Plugins above). Every
component fails loudly on an unrecognized env var under its own namespace,
and the harness fails loudly on an unknown manifest key or version. See `SPEC.md §5.7` for the full list of knobs and `STACK.md §7`
for which component owns which variable. `.env.example` is a starting point.
