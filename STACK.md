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
| Config | env vars only, parsed into typed structs with serde defaults |
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
| reqwest | 0.13 | harness | Ollama adapter transport; Telegram adapter's calls to Harness API |
| teloxide | 0.17 | adapter-telegram | long-polling updates stream, sendMessage rendering |
| tracing / tracing-subscriber | 0.1 / 0.3 | all | structured logging; env-filter via `RUST_LOG` |
| thiserror | 2.0 | libs | typed errors in library crates (`contract`, harness internals) |
| anyhow | 1.0 | binaries | error context at binary edges (main, adapters) |
| lru | 0.18 | harness | `client_msg_id` dedup window |
| rand | 0.10 | harness | confirmation tokens (`OsRng`, ≥128-bit) |
| subtle | 2.6 | harness | constant-time bearer-token comparison |
| clap | 4.6 | binaries | CLI arg parsing (adapters, harness flags) |
| async-trait | 0.1 | harness | dyn-compatible async traits (`ModelProvider`, `ToolExecutor`) |
| wiremock | 0.6 | tests | stub HTTP server for contract tests |

Notes:
- rmcp feature flags decided at M1 scaffold: harness ships the client-only set
  (`client`, `transport-streamable-http-client-reqwest`,
  `transport-child-process`); test doubles add the server features via
  dev-dependency unification; mcp-linux ships `server`, `macros`,
  `transport-io`, `transport-streamable-http-server`. Dual transport is a
  config concern only — both converge on the same rmcp client handle.
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
│   └── linux-mcp.Dockerfile
└── crates/
    ├── contract/            # Order, Response, ErrorCode enum, attachment types.
    │                        # The ONLY dependency adapters share with the core.
    │                        # No logic beyond (de)serialization + validation helpers.
    ├── harness/             # Part B binary. Modules:
    │   ├── api/             #   axum router, auth layer, /v1 routes, error mapping
    │   ├── queue/           #   global FIFO (tokio mpsc(1)) — one order in flight
    │   ├── stores/          #   sessions (+TTL sweeper), dedup LRU, pending actions
    │   ├── orchestrator/    #   agent loop, prompt assembly/context budgeting,
    │   │                    #   safety classification, frozen-payload gate
    │   ├── mcp/             #   connection manager, tool registry, namespacing
    │   └── model/           #   provider.rs (trait) + ollama.rs (impl)
    ├── adapter-cli/         # M0 throwaway: stdin → Order → render Response
    ├── adapter-telegram/    # Part A: teloxide polling → Order; renders replies/errors
    └── mcp-linux/           # Contract C backend: read-only Linux diagnostics MCP server
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
   `{tool, args}`, mint token, return `needs_confirmation`), enforces
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
6. Redemption path (Order carries `confirmation_token`): validated in `stores/`,
   stored payload dispatched verbatim, token deleted before dispatch, zero model
   involvement (SPEC §4.3). `audit.rs` appends the outcome (success or failure)
   to a hash-chained JSONL file — the harness's own durable record, independent
   of `stores/`'s in-memory, restart-clears semantics (SPEC §12.1).

## 6. Deployment topology

```
HOST (Linux home server)
├─ ollama.service                      :11434  (GPU/model access — stays on host)
└─ docker compose
   ├─ harness         → binds 127.0.0.1:8080   extra_hosts: "host.docker.internal:host-gateway"
   │                     volume arno-state:/data (tool policy + audit log, M3)
   ├─ telegram-adapter→ outbound-only (getUpdates); single replica (409 rule)
   └─ mcp-linux       → Streamable HTTP on the compose network, localhost-scoped
```

- Images: multi-stage build (rust:slim builder → debian-slim runtime), non-root user, one binary per image.
- Ollama reached as `http://host.docker.internal:11434` (host-gateway mapping above).
- Secrets enter exclusively as compose/env vars (`env_file` excluded from VCS).
- Healthchecks: harness `GET /v1/health` (additive route), adapters process-liveness, mcp-linux MCP ping — surfaced to compose (backlog §12.3).
- Restart policy `unless-stopped`.
- **`arno-state` (M3, AGENTS.md #7 exceptions):** the one stateful volume in
  this topology, holding `TOOL_POLICY_PATH` and `AUDIT_LOG_PATH`. Everything
  else here is legitimately restart-clears; these two must survive a
  container recreate or the harness's own memory of tool safety and its
  record of executed writes vanish with it.

## 7. Config ownership

Every SPEC §5.7 knob belongs to exactly one component; unknown vars fail startup loudly:

| Component | Owns |
|---|---|
| harness | `HARNESS_API_BIND`, `HARNESS_API_CLIENT_TOKENS`, `OLLAMA_URL/MODEL/NUM_CTX/TIMEOUT_S/THINK`, `MCP_TOOL_TIMEOUT_S`, `ORDER_BUDGET_S`, `MAX_TOOL_CALLS`, `DEDUP_WINDOW_MIN`, `SESSION_TTL_H`, `CONFIRM_TTL_MIN`, `ATTACH_MAX_BYTES`, `DESTRUCTIVE_TOOLS`, `MCP_SERVERS`, `TOOL_POLICY_PATH`, `AUDIT_LOG_PATH`, `TOOL_POLICY_RETRY_S` (M3) |
| adapter-telegram | `TELEGRAM_BOT_TOKEN`, `ALLOWED_CHAT_IDS`, `HARNESS_API_URL`, `HARNESS_API_TOKEN`, `TELEGRAM_HTTP_TIMEOUT_S` (default 240s, must exceed `ORDER_BUDGET_S`) |
| mcp-linux | `MCP_LINUX_TRANSPORT` (`http`\|`stdio`), `MCP_LINUX_BIND` |
| deployment layer (compose/.env, not the harness) | `SECURO_MCP_URL`, `SECURO_MCP_AUTH` — composed into the harness's `MCP_SERVERS` entry as a header block (SPEC §5.7); the harness itself owns only `MCP_SERVERS` and never reads a `SECURO_*` var (AGENTS.md #6). No workspace var: the bearer JWT's own `ws_id` claim scopes every call server-side (confirmed live, M2) |

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

## 9. Deliberately deferred

| Item | When | Why |
|---|---|---|
| Concrete linux diagnostic tool list | M1 | needs a real box to design against |
| SSE/WebSocket under `/v1` | when a consumer exists | additive, no rework (SPEC §11.3) |
| Second ModelProvider impl | when needed | seam exists; do not pre-build |
