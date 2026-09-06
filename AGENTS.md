# AGENTS.md

Instructions for coding agents working in this repository. Read `SPEC.md` (design)
and `STACK.md` (stack/deployment) before proposing changes to structure or
dependencies — they are the source of truth. This file tells you how to work
*within* that design.

## Project

Home Server Harness ("arno"): three independently replaceable parts — interface
adapters, harness core, MCP tool backends — joined by two stable contracts
(Harness API, MCP). Local-first, read-only by default, Rust end to end.

## Commands

```sh
cargo build --workspace              # build everything
cargo clippy --workspace -- -D warnings   # lint gate — must be clean
cargo fmt --all -- --check           # format gate — run cargo fmt, don't hand-format
cargo test --workspace               # unit + contract tests; no network needed
RUST_LOG=debug cargo run -p adapter-cli   # smoke loop against a local harness
docker compose up -d --build         # full deployment topology
```

Tests never require real Ollama or Telegram: `ModelProvider` and the Harness API
get scripted fakes/wiremock doubles. If you add a test that hits a live service,
you are doing it wrong.

## Workspace map

| Crate | Role |
|---|---|
| `crates/contract` | Wire types: Order, Response, ErrorCode, attachments. Shared vocabulary of Contract 1. |
| `crates/harness` | Part B core: axum API, FIFO queue, stores, orchestrator, MCP client, ModelProvider trait, adaptive tool policy, audit log. Binary. |
| `crates/adapter-cli` | Throwaway M0 interface adapter (stdin ↔ Order/Response). |
| `crates/adapter-telegram` | Part A v1: teloxide long-polling bridge. |
| `crates/mcp-linux` | First MCP backend: read-only Linux diagnostics (rmcp server side). |

Allowed dependency edges: `{harness, adapters} → contract`. Nothing else.
Adapters must not depend on `harness`; `harness` must not depend on adapters.

## Hard rules — violating any of these is a design bug, not a style issue

1. **No app-specific code in the core.** The harness contains zero
   Telegram- or Securo-specific logic. All per-app knowledge lives in adapters or
   configuration (`MCP_SERVERS`, `DESTRUCTIVE_TOOLS`). If you're adding a string
   like `"securo"` or `"telegram"` inside `crates/harness`, stop.
2. **Contract 1 is additive-only.** Never change or remove fields of Order /
   Response / ErrorCode under `/v1`; never repurpose an error code. Breaking
   changes ship as a parallel `/v2` (SPEC §4.1).
3. **The model never executes confirmed actions.** A redemption path dispatches
   the stored frozen payload verbatim over MCP with zero model involvement, and
   the token is deleted *before* dispatch. Destructive calls are never dispatched
   on model output alone and **never auto-retried** (SPEC §4.3, §8). Confirmation
   *interpretation* is model-driven (`crates/harness/src/confirm.rs`, SPEC §4.3
   revised) — the model judges whether a reply constitutes consent to a pending
   action. This does not weaken this rule: the classifier decides only *whether*,
   never *what* — it cannot see or alter the frozen payload, only whether a
   `redeem()` call happens at all. If you're touching confirmation code and find
   yourself passing the model anything it could use to influence *content* rather
   than *consent*, stop — that crosses this rule.
4. **All model access goes through the `ModelProvider` trait.** No direct HTTP to
   Ollama outside `harness/src/model/ollama.rs`.
5. **One order in flight.** The global FIFO queue is what makes session access
   and confirmation redemption race-free by construction. Do not add concurrent
   execution paths around it (SPEC §8).
6. **Config arrives exclusively via env vars**, parsed into typed structs at
   startup; unknown vars fail loudly. Every new knob must also be documented in
   SPEC §5.7 and STACK.md §7.
7. **In-memory stores stay in-memory** (sessions, dedup LRU, pending actions) —
   restart-clears is accepted behavior, not a gap. Persistence proposals need a
   spec revision first. **Three narrowly-scoped exceptions exist (M3, SPEC
   §11, §12.1, §12.2):** the tool-safety policy (`crates/harness/src/policy.rs`),
   the hash-chained audit log (`crates/harness/src/audit.rs`), and the
   hash-chained conversation/tool-call transcript (`crates/harness/src/transcript.rs`).
   All three are the harness's own memory of what backends have declared and
   what it has actually done or said in real financial conversations —
   losing any of them on restart would silently re-open trust assumptions or
   erase the only evidence of what actually happened, rather than fail
   closed. Do not add other persistent stores without going through the same
   process (a spec revision, not a quiet implementation detail).

## Conventions

- Errors: `thiserror` enums in library crates; `anyhow` only at binary edges.
- Logging: `tracing` everywhere; log codes and tool names, not payloads —
  tool results contain finance data (backlog §12.2). When in doubt, log less.
- Comments only where the code can't speak (invariants, contract references,
  safety reasoning). Reference SPEC sections (`// SPEC §4.3`) instead of prose.
- New routes go under `/v1/...` with an entry in the api module's route table
  and a wiremock contract test.
- Timeouts/retries are config-driven knobs, never magic constants
  (`OLLAMA_TIMEOUT_S`, `MCP_TOOL_TIMEOUT_S`, `ORDER_BUDGET_S`).
- Security posture: constant-time token comparison (`subtle`), CSPRNG tokens
  (≥128-bit), bind localhost, no raw shell anywhere, containers run non-root.

## When you finish something non-trivial

Record durable outcomes (a decision made, a gotcha discovered, a lesson learned)
as notes in the Obsidian vault via archiver-rag (`log_note`), tagged `arno`, and
link them to `arno-home-server-harness-spec-v0-4` / `arno-stack-decisions-v0-5`.
If the outcome changed the design, update SPEC.md/STACK.md first — the vault note
summarizes; the repo documents lead.
