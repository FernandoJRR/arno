# Arno — Stack & Architecture (v0.5)

Companion to `SPEC.md` v0.5. This file records the concrete technology decisions
that SPEC §10 defers to "stack planning", plus the module layout, the model-provider
seam, deployment topology, and config ownership.

> Decided 2026-08-25. Crate majors verified against crates.io on this date.
> Bump majors deliberately; never let a `cargo update` cross a major boundary silently.

---

## 1. Decisions at a glance

| Area | Decision |
|---|---|
| Language | Rust, stable toolchain, edition 2024 |
| Async runtime | tokio (single runtime per process) |
| MCP | **rmcp** (official SDK, spec `2026-07-28` line, 3.x) — client in harness, server for linux-mcp |
| Harness API | axum, all routes under `/v1` |
| Model runtime | **ModelProvider trait** with Ollama as first adapter impl (`/api/chat`, native API — not `/v1` compat) |
| Interface adapter v1 | teloxide long-polling |
| Config | env vars, parsed into typed structs with serde defaults; plus plugin manifests (TOML) and per-plugin env files in `PLUGINS_DIR` (SPEC §4.5, §11 #21) |
| Serialization | serde / serde_json everywhere; one wire schema owned by `contract` crate |
| Deployment | Docker multi-stage builds + compose, one service per part; **Ollama stays on the host** |
| Testing | cargo test; wiremock for Harness API contract tests; rmcp-based mock MCP server |

## 2. Crate table

Versions are current stable majors at decision time (`major.minor` shown for orientation).

| Crate | Ver | Part | Purpose |
|---|---|---|---|
| tokio | 1.53 | all | async runtime, mpsc queue, timers/TTL sweepers, signal handling |
| serde / serde_json | 1.0 | all | derive serialization; wire types live in `contract` |
| axum | 0.8 | harness | HTTP API server under `/v1`; extractor-based auth |
| rmcp | 3.1 | harness + mcp-linux | MCP client (harness: tool discovery/dispatch over Streamable HTTP); MCP server (mcp-linux diagnostics tools) |
| chrono / chrono-tz | 0.4 / 0.10 | harness | timestamps and decision expiry; the user's local date in the prompt from a compiled-in IANA database, no `tzdata` in the image (SPEC §11 #28) |
| reqwest | 0.13 | harness | Ollama adapter transport; Telegram adapter's calls to Harness API |
| teloxide | 0.17 | adapter-telegram | long-polling updates stream (messages + callback queries), sendMessage rendering, inline keyboards for decisions (SPEC §4.3) |
| futures | 0.3 | adapter-telegram | `StreamExt` to drive the polling stream |
| image | 0.25 (jpeg, png, webp) | mcp-ocr | decode attachments (EXIF orientation applied), resize, grayscale; pure Rust, no system libs (SPEC §11 #19) |
| imageproc | 0.27 | mcp-ocr | rotation for deskew |
| tempfile | 3 | mcp-ocr | private 0600 temp file for the cleaned image, removed on drop |
| text2num | 2.8 | mcp-ocr | amounts written in words (English, Spanish) for misread checks (SPEC §11 #20) |
| tracing / tracing-subscriber | 0.1 / 0.3 | all | structured logging; env-filter via `RUST_LOG` |
| thiserror | 2.0 | libs | typed errors in library crates (`contract`, harness internals) |
| anyhow | 1.0 | binaries | error context at binary edges (main, adapters) |
| lru | 0.18 | harness | `client_msg_id` dedup window |
| rand | 0.10 | harness | confirmation tokens (`OsRng`, ≥128-bit) |
| subtle | 2.6 | harness | constant-time bearer-token comparison |
| clap | 4.6 | binaries | CLI arg parsing (adapters, harness flags) |
| async-trait | 0.1 | harness | dyn-compatible async traits (`ModelProvider`, `ToolExecutor`) |
| toml | 1.1 | harness | plugin manifests (SPEC §4.5); `deny_unknown_fields` gives the fail-loudly behavior of unknown env vars |
| arc-swap | 1.9 | harness | lock-free swappable plugin set: orders load a snapshot, a reload stores a new one between orders (SPEC §11 #24) |
| wiremock | 0.6 | tests | stub HTTP server for contract tests |

Notes:
- rmcp feature flags decided at M1 scaffold: harness ships the client-only set
  (`client`, `transport-streamable-http-client-reqwest`,
  `transport-child-process`); test doubles add the server features via
  dev-dependency unification; mcp-linux ships `server`, `macros`,
  `transport-io`, `transport-streamable-http-server`. Dual transport is a
  config concern only — both converge on the same rmcp client handle.
  `mcp-ocr` mirrors mcp-linux's feature set exactly (SPEC §11 decision #17).
- `mcp-ocr` execs the fixed `tesseract` binary directly (argv, no shell) via
  an `OcrEngine` trait mirroring mcp-linux's `CommandRunner`/`SystemCtl` seam;
  chosen over a co-resident vision model because qwen3:4b-instruct already
  saturates most of a 6 GB VRAM budget. An `ollama-vision` `OcrEngine` impl is
  a documented future variant behind the same trait, not built.
  Each call cleans the image in-process (`preprocess.rs`: upscale, contrast
  stretch, deskew) and runs three Tesseract reads in parallel (TSV output,
  `OMP_THREAD_LIMIT=1`), keeping the most confident (`reading.rs`); words under
  60% confidence come back as `[?word]`, and rule fragments plus
  all-doubtful non-numeric lines are trimmed before returning. `checks.rs`
  adds `checks` for likely misreads (single confusable-digit repairs that
  satisfy sums, differences or an amount in words) and corrects the text in
  place when two independent pieces of evidence agree; the harness relays
  any tool's `checks` verbatim (hiding them from the model) and notes
  ungrounded numbers
  (`orchestrator/grounding.rs`, SPEC §11 #20). The image ships `tessdata_best`
  eng+spa pinned to commit `e12c65a` with sha256 checks (`TESSDATA_PREFIX`);
  Debian's fast models stay installed as the fallback. Tuning on a real bill:
  `cargo run -p mcp-ocr --example ocr_eval -- <image> [expected…]` with
  `TESSDATA_PREFIX` pointing at the models (SPEC §11 #19).
- No ORM/database anywhere: every store is in-memory by design (SPEC §4.1). If persistence arrives later it enters behind a store trait, not as a new global dependency.
- No HTTP framework beyond axum; no tower-http middleware stack in v1 (localhost-only, no TLS — SPEC §7).

## 3. Workspace layout

```
arno/
├── Cargo.toml               # [workspace] members = ["crates/*"]
├── SPEC.md                  # architecture spec — source of truth for design
├── STACK.md                 # this file — source of truth for stack/deployment
├── AGENTS.md                # rules for coding agents
├── compose.yaml             # one service per part (SPEC §5.7)
├── docker/
│   ├── harness.Dockerfile
│   ├── telegram-adapter.Dockerfile
│   ├── linux-mcp.Dockerfile
│   └── ocr-mcp.Dockerfile
└── crates/
    ├── contract/            # Order, Response, ErrorCode enum, attachment types.
    │                        # The ONLY dependency adapters share with the core.
    │                        # No logic beyond (de)serialization + validation helpers.
    ├── harness/             # Part B binary. Modules:
    │   ├── api/             #   axum router, auth layer, /v1 routes, error mapping
    │   ├── queue/           #   global FIFO (tokio mpsc(1)) — one order in flight
    │   ├── stores/          #   sessions (+TTL sweeper), dedup LRU, pending actions,
    │   │                    #   attachments (ephemeral scratch, §4.4)
    │   ├── orchestrator/    #   agent loop, prompt assembly/context budgeting,
    │   │                    #   safety classification, frozen-payload gate
    │   ├── mcp/             #   connection manager, tool registry, namespacing
    │   └── model/           #   provider.rs (trait) + ollama.rs (impl)
    ├── adapter-cli/         # M0 throwaway: stdin → Order → render Response
    ├── adapter-telegram/    # Part A: teloxide polling → Order; renders replies/errors/decision buttons
    ├── mcp-linux/           # Contract C backend: read-only Linux diagnostics MCP server
    └── mcp-ocr/             # Contract C backend: read-only OCR MCP server (Tesseract,
                             # `OcrEngine` trait seam — SPEC §11 decision #17)
```

Dependency rule (enforced by imports, reviewable in CI): `contract ← {harness, adapters}`.
Adapters must not depend on `harness`; `harness` must not depend on any adapter.

## 4. ModelProvider seam

The model runtime sits behind an internal trait so providers are swappable without
touching orchestration — same discipline as parts A and C, applied inside part B.

```rust
#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    /// One generation step of the agent loop.
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionOutput, ModelError>;
}

pub struct CompletionRequest {
    pub messages: Vec<Message>,        // system/user/assistant/tool-call/tool-result
    pub tools: Vec<ToolSchema>,        // merged, already-namespaced tool definitions
    pub temperature: f32,              // low for tool selection (SPEC §5.4)
    pub context_tokens: u32,           // provider-neutral context budget
}

pub enum CompletionOutput {
    Final(String),                     // answer text
    ToolCalls(Vec<ToolCallRequest>),   // orchestrator dispatches or freezes
}
```

Mapping rules:

| Request field | Ollama adapter (native `/api/chat`) | Future OpenAI-compat adapter |
|---|---|---|
| `context_tokens` | `options.num_ctx` | ignored (or maps to provider max) — document per impl |
| `temperature` | `options.temperature` | `temperature` |
| tools | native `tools` array | `tools` array (note: no `tool_choice` upstream) |
| messages | native roles incl. `tool` results | OpenAI roles |

Why native `/api/chat` and not Ollama's OpenAI-compatible `/v1`: the compat endpoint
cannot set `num_ctx` per request (requires a Modelfile) and does not support
`tool_choice`. Both matter here — `OLLAMA_NUM_CTX` is a first-class knob (SPEC §8:
raising it is the #1 small-model fix).

Rule: nothing outside `harness/src/model/` speaks HTTP to a model provider.

## 5. Harness internals — how SPEC §6 maps to modules

1. `api/` authenticates (constant-time token compare), validates, dedups? No —
   dedup lives behind the queue: the handler enqueues and the worker owns session
   state. Handler returns only transport-level errors; order-level errors
   (`duplicate_order`, `order_budget_exceeded`, …) come back through the Response
   path so adapters have exactly one error surface.
2. `queue/` single consumer = the FIFO from SPEC §8; race-free sessions and
   redemption by construction.
3. `orchestrator/` runs the §6 loop: prompt assembly fills context budget in fixed
   priority (tool schemas → system → newest history first; drop oldest whole turns),
   classifies each tool call (read-only → `mcp/` dispatch; destructive → freeze
   `{tool, args}`, mint a decision id, return a `decision`, SPEC §4.3), enforces
   `ORDER_BUDGET_S` with a clean cancel that reports completed RO side effects.
4. Safety classification is **adaptive** (M3, SPEC §4.2): `policy.rs` classifies
   each discovered tool by asking the model to read its name/description/
   schema once and persisting the verdict (`safe` / `destructive` /
   `destructive_when{key,value}`); dispatch evaluates the persisted rule
   synchronously, no model call per order. Unclassified/drifted tools are
   `destructive` (fail-closed) until a real verdict lands. `DESTRUCTIVE_TOOLS`
   lists exact namespaced tool names that are the operator's override, always
   winning — the common case leaves it empty.
5. `mcp/` holds one rmcp client per configured backend, namespaces tools
   `<server>.<tool>`, presents the merged schema to the orchestrator, and tags
   failures with their backend for `backend_unavailable`.
6. Decision path (Order carries `decision_answer`, or a legacy
   `confirmation_token`): validated in `stores/`, id deleted before anything
   happens, only `approve` dispatches the stored payload verbatim, zero model
   involvement (SPEC §4.3). Any other order supersedes pending decisions. `audit.rs` appends the outcome (success or failure)
   to a hash-chained JSONL file — the harness's own durable record, independent
   of `stores/`'s in-memory, restart-clears semantics (SPEC §12.1).

## 6. Deployment topology

```
HOST (Linux home server)
├─ ollama.service                      :11434  (GPU/model access — stays on host)
└─ docker compose
   ├─ harness         → binds 127.0.0.1:8080   extra_hosts: "host.docker.internal:host-gateway"
   │                     bind mount ./data:/data (tool policy + audit log + transcript, M3)
   │                     named volume attachments:/attachments (ephemeral scratch, §4.4)
   ├─ telegram-adapter→ outbound-only (getUpdates); single replica (409 rule)
   ├─ mcp-linux       → Streamable HTTP on the compose network, localhost-scoped
   └─ mcp-ocr         → Streamable HTTP on the compose network, localhost-scoped;
                         named volume attachments:/attachments (same volume as harness)
```

- Images: multi-stage build (rust:slim builder → debian-slim runtime), non-root user, one binary per image.
- Ollama reached as `http://host.docker.internal:11434` (host-gateway mapping above).
- Secrets enter exclusively as compose/env vars (`env_file` excluded from VCS).
- Healthchecks: harness `GET /v1/health` (additive route), adapters process-liveness, mcp-linux MCP ping — surfaced to compose (backlog §12.3).
- Restart policy `unless-stopped`.
- **`./data` (M3, AGENTS.md #7 exceptions):** the one stateful path in this
  topology, bind-mounted (not a named volume, so the files are host-visible
  and inspectable with plain `cat`/`jq`) holding `TOOL_POLICY_PATH`,
  `AUDIT_LOG_PATH`, and `TRANSCRIPT_LOG_PATH` — gitignored, real finance data.
  Everything else here is legitimately restart-clears; these three must
  survive a container recreate or the harness's own memory of tool safety,
  its record of executed writes, and its conversation
  history all vanish with it. The harness's own local-dev defaults now point
  at `./data/` too (SPEC §11 decision #15), so `./data` is the single
  canonical location in every mode. Rotated log segments
  (`arno-audit-<UTC ts>.jsonl`, `arno-transcript-<UTC ts>.jsonl`, SPEC §11
  decision #15) live beside the active files in this same directory and are
  covered by the same bind mount, gitignore, and inspectability guarantees.
- **`attachments` (SPEC §4.4, §11 decision #17):** the opposite posture from
  `./data` above — a **named** volume, not a bind mount, and deliberately
  *not* one of the AGENTS.md #7 exceptions. It holds inbound image
  attachments for the duration of one order only: the harness deletes each
  order's files on every return path and purges the whole volume again at its
  own boot, so `docker compose down -v` losing it is a feature, not a risk.
  Shared between `harness` (`ATTACH_DIR`) and `mcp-ocr`
  (`MCP_OCR_ATTACH_DIR`); both containers run as the same uid (`10001`,
  `docker/harness.Dockerfile` and `docker/ocr-mcp.Dockerfile`) so the
  harness's 0600 files stay readable across the container boundary without
  loosening the file mode.

## 8.1 Backup (ops)

The chain stores in `./data` are the only stateful data; off-host copy is an
ops concern (SPEC §11 decision #15): tail-truncation of the newest segment is
undetectable from the files alone — only a second copy catches it.
`docker/backup-logs.sh` rsyncs `./data/` (owner-only perms) to
`$ARNO_BACKUP_DEST` (default `~/arno-logs-backup`); schedule via host cron.

## 7. Config ownership

Every SPEC §5.7 knob belongs to exactly one component; unknown vars fail startup loudly:

| Component | Owns |
|---|---|
| harness | `HARNESS_API_BIND`, `HARNESS_API_CLIENT_TOKENS`, `OLLAMA_URL/MODEL/TIMEOUT_S/THINK`, `OLLAMA_NUM_CTX` (default 16384 — fits qwen3:4b ~94% on a 6 GB GPU, SPEC §11 #20), `OLLAMA_SEED` (default 42, 0=provider default), `OLLAMA_TOP_K` (default 1 greedy, 0=provider default) — deterministic tool-selection sampler per SPEC §11 decision #16, `MCP_TOOL_TIMEOUT_S`, `ORDER_BUDGET_S`, `MAX_TOOL_CALLS`, `DEDUP_WINDOW_MIN`, `SESSION_TTL_H`, `CONFIRM_TTL_MIN`, `HARNESS_TIMEZONE` (default `UTC`, IANA name — the dates the model sees, SPEC §11 #28), `CONFIRM_MODE` (deprecated, ignored with a boot warning — SPEC §11 #18), `ATTACH_MAX_BYTES`, `ATTACH_DIR` (default `./data/attachments` — ephemeral scratch for inbound image attachments, SPEC §4.4/§11 decision #17), `DESTRUCTIVE_TOOLS`, `MCP_SERVERS` (deprecated, SPEC §11 #21), `PLUGINS_DIR` (default `./config/plugins`), `PLUGINS_REPORT_PATH` (default `./data/arno-plugins-report.json`), `ROUTER_ENABLED` (default `false`, SPEC §11 #23), `TOOL_POLICY_PATH`, `AUDIT_LOG_PATH`, `TRANSCRIPT_LOG_PATH`, `TOOL_POLICY_RETRY_S` (M3), `AUDIT_LOG_ROTATE_BYTES` (default 2 MiB, 0=off), `TRANSCRIPT_LOG_ROTATE_BYTES` (default 10 MiB, 0=off), `LOG_KEEP_SEGMENTS` (default 12), `LOG_VERIFY_ON_BOOT` (default `false`) — rotation/verification knobs per SPEC §11 decision #15, `OLLAMA_NUM_PREDICT` (default 400, 0=provider default), `NUMBER_GUARD` (default `true`) — answer length and number hygiene per SPEC §11 #20 |
| adapter-telegram | `TELEGRAM_BOT_TOKEN`, `ALLOWED_CHAT_IDS`, `HARNESS_API_URL`, `HARNESS_API_TOKEN`, `TELEGRAM_HTTP_TIMEOUT_S` (default 240s, must exceed `ORDER_BUDGET_S`) |
| mcp-linux | `MCP_LINUX_TRANSPORT` (`http`\|`stdio`), `MCP_LINUX_BIND` |
| mcp-ocr | `MCP_OCR_TRANSPORT` (`http`\|`stdio`), `MCP_OCR_BIND`, `MCP_OCR_ATTACH_DIR` (its own mount of the `attachments` volume shared with the harness, SPEC §4.4), `MCP_OCR_LANG` (default `eng+spa`), `MCP_OCR_ENGINE` (only `tesseract` today; `ollama-vision` documented, not built — SPEC §11 decision #17), `MCP_OCR_PREPROCESS` (`on`\|`off`, default `on` — SPEC §11 #19) |
| plugins (`config/plugins/`, not env) | `<id>.toml` manifests and `<id>.env` secrets, e.g. `securo.toml` (URL, groups) + `securo.env` (`SECURO_MCP_AUTH`). Read by the harness at load; secret values never enter its process env (SPEC §4.5). No workspace var: the bearer JWT's own `ws_id` claim scopes every call server-side (confirmed live, M2) |

## 8. Testing strategy

- **Unit**: stores (TTL/expiry/single-use semantics), context-budget trimmer (never splits a tool pair), classifier, token minting.
- **Contract (wiremock)**: Harness API golden tests — auth, dedup window, all eight error codes, confirmation lifecycle (unknown/expired/used).
- **Mock MCP server**: rmcp-based test double exercising discovery, namespacing, timeout → `backend_unavailable`; doubles as fixture for destructive-gate tests.
- **Smoke**: adapter-cli against a running harness + mock MCP — the M0 loop, kept green in CI.
- Model-dependent tests never hit real Ollama: `ModelProvider` gets a scripted fake.
- **Tool policy (M3, `policy.rs`)**: rule evaluation (including `destructive_when`
  match/non-match), full precedence (env override > operator-pinned file entry >
  model verdict > pending-fail-closed), fingerprint-drift re-pending, a corrupt
  policy file degrading to all-pending rather than panicking, and a classifier
  fed a scripted `ModelProvider` (clean JSON, code-fenced JSON, malformed output,
  a down provider, and a down-then-recovered provider proving the retry path).
- **Audit log (M3, `audit.rs`)**: genesis entry, hash chaining across entries,
  resuming `seq`/hash on reopen, a tampered or truncated line rejected on
  reopen, and `ok`/`err` outcome tagging.
- **Freeze-branch coverage (M3, `api_contract.rs`)**: a wiremock-driven model
  tool call carrying the classified trigger argument freezes (never dispatched,
  `needs_confirmation` returned with the args rendered in `text`); the same
  tool without it dispatches immediately. Every earlier destructive-gate test
  exercised only the redemption *replay* half by seeding the pending store
  directly — these are the first to exercise the classify-then-freeze-or-
  dispatch decision itself.
- **Transcript log (`transcript.rs`)**: same coverage shape as the audit log
  (genesis entry, chaining, resume-on-reopen, tampered/truncated line
  rejected, `0600` perms) plus one `api_contract.rs` integration test driving
  a real tool-call round trip end to end and asserting the resulting file
  contains `user_message` → `tool_call` → `tool_result` → `assistant_final`
  in order with a valid hash chain — the concrete fix for "there's no record
  of what tools got called."
- **Decisions (SPEC §4.3, §11 #18)**: `contract` round-trips `decision` /
  `decision_answer` (exactly-one-of `value`/`text`) and the four
  `decision_*` codes; `pending.rs` covers `dcs_` ids carrying their options
  and `discard_session` (supersede, client/session isolation); the
  orchestrator builds `details` from args + schema titles without model text.
  `api_contract.rs`: a frozen proposal returns `decision` plus legacy fields
  with equal ids; `approve` dispatches exactly once and a replay is
  `decision_used`; `reject` never dispatches; unknown / expired / invalid
  value give distinct codes and an invalid value still burns the id; a
  free-text answer runs the model and never dispatches; a plain "yes"
  supersedes the decision and never executes. Adapter tests cover Telegram
  short callback keys (≤64 bytes, chat-bound, single tap per decision,
  bounded map) and the CLI number/free-text mapping.
- **Attachment ingestion (SPEC §4.4/§11 decision #17, `stores/attachments.rs`)**:
  allowed-mime + size filtering, a written file has `0600` perms and a
  charset-safe handle, disallowed mime/oversize/`ref`-form attachments are
  skipped, `cleanup`/`purge_all` remove files and tolerate an already-missing
  target. `api_contract.rs` adds one end-to-end test: an order carrying an
  inline image attachment produces the manifest system message, a scripted
  `ModelProvider` reads the handle back out of it and calls `ocr.extract_text`,
  the response reflects the tool result, the scratch file is gone afterward,
  and the manifest never appears in the persisted session history.
- **`mcp-ocr` (SPEC §11 decision #17)**: `extract_text` argument/result shape
  exercised against a fake `OcrEngine` (no real Tesseract binary in CI); the
  handle-traversal guard rejects `../x` and empty handles before the engine is
  ever called; a stdio round trip against the crate's own binary lists the
  fixed one-tool surface and confirms the same traversal guard end to end. Preprocessing is tested on synthetic images only
  (gradient lighting, known 3° tilt, upscale cap, pixel-count cap, garbage
  input); TSV parsing, doubt marks and variant selection use a fake engine;
  the cleaned temp file is checked gone after every call.

## 9. Deliberately deferred

| Item | When | Why |
|---|---|---|
| Concrete linux diagnostic tool list | M1 | needs a real box to design against |
| SSE/WebSocket under `/v1` | when a consumer exists | additive, no rework (SPEC §11.3) |
| Second ModelProvider impl | when needed | seam exists; do not pre-build |
| `ollama-vision` `OcrEngine` impl | when GPU budget allows a co-resident vision model | trait seam exists (`mcp-ocr`); building it now would compete with qwen3 for the same 6 GB VRAM (SPEC §11 decision #17) |
| PDF bill OCR | when needed | needs `pdftoppm` rasterization ahead of `extract_text`; images-only for v1 |
| `ref`-form attachment upload (`POST /v1/files`) | when needed | schema field already reserved (SPEC §4.1); no server-side TTL temp storage built yet |
