# Home Server Harness — Spec v0.9 (draft)

Supersedes v0.8. Restructured around a single principle: **three independently
replaceable parts, joined by two stable contracts.** Any part can be swapped
without touching the other two. Concrete stack and deployment topology live in
the companion file `STACK.md`; agent-facing rules in `AGENTS.md`.

> Status: draft for review. Assumptions needing confirmation are marked
> **[CONFIRM]**. Open decisions are in §11.
>
> **Changelog v0.8 → v0.9** — Confirmation moved from each adapter into the
> harness core and became model-interpreted (§4.3, revised). Prompted by two
> real, adapter-level bugs found live: `adapter-cli` never implemented
> redemption at all (every order hardcoded `confirmation_token: None`, so a
> destructive action proposed through it could never execute, no matter how
> many times the model claimed success — seen directly in a real transcript);
> `adapter-telegram` only redeemed on the exact literal `[CONFIRM]` and
> discarded the pending token on any other reply. Both were symptoms of one
> mistake: confirmation was implemented per-adapter, when the harness already
> holds both the pending-action store and the model. New module
> `crates/harness/src/confirm.rs` mirrors `policy.rs`'s classifier shape
> exactly (free function, `Option` not `Result`, strict single-line JSON
> contract, every failure fails closed) to judge confirm/reject/unrelated — a
> `confirm` verdict dispatches through the same unchanged `redeem()`, so
> AGENTS.md #3 (model never executes a confirmed action, bypassed at
> execution) is fully intact: the model only ever judges *whether consent was
> given*, never *what* would execute. `CONFIRM_MODE=token_only` restores the
> strict pre-revision exact-token-only behavior without a redeploy.
>
> **Changelog v0.7 → v0.8** — Persistent conversation/tool-call transcript
> added (`crates/harness/src/transcript.rs`, `TRANSCRIPT_LOG_PATH`), the third
> AGENTS.md #7 exception. Prompted by a real gap found live: debugging a
> transaction-history bug showed there was no record anywhere of what tools
> got called or what the model said — `docker logs` carries only boot lines,
> and `SessionStore` is in-memory and wiped on restart. Deliberately a
> separate file from the audit log (`audit.rs`): that log's narrow,
> auditor-facing scope is "what write actually executed"; this is the
> broader, noisier conversational narrative — every user message, assistant
> answer, tool call request/result, and confirmation freeze/redeem, each a
> typed, greppable event. Same hash-chaining/fail-loud-on-corruption posture
> as the audit log. This resolves backlog #2 ("log hygiene") for this
> specific artifact: it holds tool args/results verbatim by design, same as
> the audit log; `tracing::` output still may never carry payloads.
>
> **Changelog v0.6 → v0.7** — M3 landed: confirmed writes enabled via
> **adaptive tool classification** rather than the exact-name `DESTRUCTIVE_TOOLS`
> list originally sketched for it. Live discovery against Securo showed why
> that couldn't work as the primary mechanism: its 9 `propose_*` write tools
> are dual-mode *by argument* (`apply=true`), not by name, and enumerating them
> in config would couple deployment config to one backend's evolving tool
> list. Instead, the model classifies each newly-discovered/changed tool at
> boot from its name/description/schema alone — the one thing every MCP tool
> guarantees — into a persisted, plain-Rust-evaluated rule; unclassified or
> ambiguous tools fail closed to destructive (§4.2). `DESTRUCTIVE_TOOLS`
> remains as the operator's highest-precedence override. New: a persistent,
> hash-chained audit log of executed frozen actions (§12.1) and the tool
> policy file itself are the harness's first two exceptions to "in-memory
> stores stay in-memory" (AGENTS.md #7) — this revision is that exception's
> spec justification. No open items remain blocking M3.
>
> **Changelog v0.5 → v0.6** — M2 landed: Securo wired as the second MCP
> backend (read-only). Resolves the sole open item from v0.5 — Securo
> authenticates with a bearer token, carried through a new generic
> header-bearing `MCP_SERVERS` HTTP form (`name:[Header=Value;...]scheme://…`,
> §5.7) rather than any Securo-specific harness code (AGENTS.md #6). No open
> items remain blocking M2.
>
> **Changelog v0.4 → v0.5** — crate selection pulled forward from M0 planning
> and resolved in `STACK.md`: rmcp (MCP), axum (Harness API), teloxide (Telegram),
> tokio/serde/tracing baseline, Docker multi-stage + compose topology with Ollama
> on the host. New: model runtime sits behind an internal **ModelProvider trait**
> (Ollama = first adapter impl over native `/api/chat`); linux-mcp backend decided
> as Rust on rmcp's server side; destructive-tool classification defined as
> config-driven (`DESTRUCTIVE_TOOLS`). M0 scaffolded: two additive contract
> additions — `invalid_order` error code (malformed body / attachment violations)
> and `MAX_TOOL_CALLS` knob (the §8 tool-call cap, previously unnamed).
> Sole remaining open item: Securo MCP auth (blocks M2 only).
>
> **Changelog v0.3 → v0.4** — §11 resolved: Rust stack, localhost-only API
> (evidence: outbound-only Telegram long-polling exposes nothing inbound),
> HTTP request/response transport, linux diagnostics as first backend,
> Docker/compose deployment. Sole remaining open item: Securo MCP auth
> (blocks M2 only).
>
> **Changelog v0.2.1 → v0.3** — API versioning (`/v1`), concurrency stance,
> context-budgeting and session-lifecycle policies added (§4.1, §8); backlog
> appendix added (§12); §11 restructured with options and recommendations.
>
> **Changelog v0.2 → v0.2.1** — four contract decisions folded in from review:
> 1. Attachments: dual-field schema `{mime, data_b64?, ref?}`; inline base64 built in v1, upload endpoint deferred (§4.1).
> 2. Sessions: namespaced by authenticated client, keyed `(client_id, session_id)` (§4.1).
> 3. Confirmation: frozen-payload replay — confirmed actions execute the exact stored call, model bypassed (§4.3).
> 4. Idempotency: optional `client_msg_id` + in-memory dedup window (§4.1); timeouts and retry policy added (§8, §5.7).
>
> Derived additions: machine-readable error codes (§4.1), confirmation-token
> field on Order (§4.1), new config knobs (§5.7).

---

## 1. Design principle — three parts, two contracts

**The three parts (each interchangeable):**

- **A · Interface** — where orders come in and replies go out. *Telegram today; an Android app, a TUI, or a CLI tomorrow.*
- **B · Harness** — the stable core: model + orchestration + tool routing. Rarely changes.
- **C · Tool backends** — the capabilities. *Securo today; another finance app, a document store, Docker, anything tomorrow.* All spoken to over MCP.

**The two contracts that make swaps cheap:**

- **Contract 1 — Interface ↔ Harness: the Harness API.** A transport-neutral, network-reachable request/response API. Every interface is a thin *adapter* that translates its native I/O to and from this API. **Telegram is an adapter, not part of the core.**
- **Contract 2 — Harness ↔ Tools: MCP.** Backends are MCP servers. Swapping Securo for another app = changing one endpoint in a config list. The core holds **no** app-specific logic.

**Honest framing of effort:** the tool-side decoupling (C) is essentially free —
MCP is designed for exactly this, so don't over-engineer it. The interface-side
decoupling (A) is the part that requires real design; spend your effort on the
Harness API boundary. And: define all three boundaries now, but *implement one of
each* (Telegram + Securo). Don't build adapters or backends you don't need yet —
the contracts guarantee they'll drop in later.

---

## 2. Scope

**v1 — build one of each, behind clean contracts:**
- Harness core exposing the Harness API.
- One interface adapter: Telegram (locked to your chat ID, text orders).
- Tool backends over MCP: read-only Linux diagnostics first, then Securo's MCP server.
- Read-only by default; confirmation gate designed in, destructive writes wired last.

**Designed-for, built later (must slot into existing contracts, no core rework):**
- A second interface adapter (Android app / TUI / CLI) — the real test of the interface contract.
- A tool-backend swap (another app in place of Securo) — the real test of the tool contract.
- OCR / image pipeline (carried as an attachment on an Order → OCR tool → structure → Securo write).
- Docker/network/storage writes; scheduled maintenance (cron/systemd/n8n, never the model — §8).

---

## 3. Architecture

```
  A · INTERFACES              Contract 1          B · HARNESS (core)        Contract 2         C · TOOL BACKENDS
  (interchangeable)          Harness API         ─────────────────           MCP              (interchangeable, MCP)
 ┌───────────────┐            (HTTP/WS)          ┌──────────────────┐                          ┌────────────────────┐
 │ Telegram      │──┐                            │  API server      │                     ┌───►│ linux-mcp (RO) [1st]│
 │ adapter [v1]  │  │                            │  Orchestrator    │                     │    ├────────────────────┤
 ├───────────────┤  ├──►  Order / Response  ──►  │  MCP client      │──►  tool registry ──┼───►│ securo-mcp     [2nd]│
 │ Android app   │  │        (JSON)              │  Pending actions │                     │    ├────────────────────┤
 │ TUI / CLI     │──┘                            │  Ollama/Qwen3-8B │                     └───►│ other-app-mcp (swap)│
 └───────────────┘                               └──────────────────┘                          └────────────────────┘
   own their user-auth;                          knows nothing about                            swap = change one
   authenticate to the API                       Telegram or Securo                             endpoint in config
```

---

## 4. The two contracts

### 4.1 Contract 1 — Harness API (interface boundary)

- **Transport & versioning:** HTTP + JSON for v1 (request/response); every route lives under `/v1` (e.g. `POST /v1/orders`). Additive schema fields never break adapters; breaking changes ship as a parallel `/v2`, never in place. Upgrade path: WebSocket/SSE for streamed replies and server push (needed for a TUI dashboard or push notifications). **Decided (§11):** request/response for v1; SSE lands additively under `/v1` when a consumer exists.
- **Order (in):**
  ```
  { session_id, text, client_msg_id?, confirmation_token?,
    attachments?: [ {mime, data_b64?, ref?} ], client_meta?: {...} }
  ```
- **Response (out):**
  ```
  { text, needs_confirmation?: bool, confirmation_token?: str, structured?: [...] }
  ```
- **Sessions:** the harness keeps per-session context. Sessions are **namespaced by the authenticated client**: the store is keyed `(client_id, session_id)`. Each adapter maps its native identity (Telegram chat ID, Android user) onto a `session_id` that is local to that adapter. A valid token addressing another client's session gets `unknown_session` — cross-adapter session hijacking is impossible by construction. Sessions are deliberately not portable across adapters. The core never sees Telegram objects. **Lifecycle:** in-memory only in v1 — a harness restart clears sessions and adapters simply start fresh; idle expiry via `SESSION_TTL_H` (default 24); conversation history is trimmed by the context-budget policy (§8).
- **Idempotency:** adapters SHOULD send `client_msg_id` — a stable identifier for the native input (the Telegram adapter passes its native `update_id`). The harness keeps an in-memory LRU of recently seen ids (`DEDUP_WINDOW_MIN`, default 60); a repeat within the window is rejected with `duplicate_order` and **never re-executed**. The window is memory-only: a restart clears it, which is acceptable because cross-restart duplicates are already bounded by adapter offset handling.
- **Attachments:** optional binaries so *any* interface can feed the future OCR path identically. The schema carries both transports now so it never breaks: `data_b64` = inline base64 (**v1 implementation**, cap 10 MB raw bytes per order, `ATTACH_MAX_BYTES`; wire size ≈ +33%), `ref` = opaque handle for a future `POST /v1/files` upload endpoint backed by server-side TTL temp storage (built when the OCR pipeline lands). Exactly one of `data_b64`/`ref` per attachment. Downloading files from Telegram (bot API limit 20 MB) is adapter work, not harness work.
- **Auth:** the harness authenticates *adapters* via a per-client token. It does **not** know about end-users — that's the adapter's job (§7).
- **Errors:** non-2xx responses carry JSON `{ "error_code": ... }` so every adapter renders failures identically. Defined codes: `unauthorized`, `invalid_order`, `unknown_session`, `duplicate_order`, `confirmation_unknown`, `confirmation_expired`, `confirmation_used`, `order_budget_exceeded`, `backend_unavailable`.

### 4.2 Contract 2 — MCP (tool boundary)

- The harness is an MCP client. At startup it connects to a configured list of MCP servers, discovers their tools, namespaces them (`linux.get_disk_usage`, `securo.propose_create_transaction`), and presents one merged schema to the orchestrator.
- **Securo is one entry in that list.** Replacing it with another app is a config change; if the replacement speaks MCP, the harness needs no code change.
- Any app-specific *policy* (e.g. "bill data goes to Securo") lives in configuration, not in the core — so the policy moves with the config when you swap backends.

**Tool safety classification (M3).** Every discovered tool must be classified
read-only-safe or destructive before it can be dispatched. Two rejected designs
are recorded for posterity: an exact-tool-name env list can't express a tool
that is dual-mode *by argument* (Securo's `propose_*` tools default to a
harmless preview and only write when called with `apply: true` — one tool
name, two safety classes); and standard MCP tool annotations
(`readOnlyHint`/`destructiveHint`) would be ideal but no backend is obliged to
send them — Securo sends none.

Instead: **the model classifies each tool at discovery**, using only what MCP
guarantees every tool has — its name, description, and input schema — and
emits one mechanical, persisted rule: `safe`, `destructive`, or
`destructive_when {key, value}` (the model derives the triggering argument
itself; the harness never hardcodes an argument name like `apply`). That rule
is what dispatch consults, synchronously, in plain Rust — **the model never
executes the classification decision for a specific order, only authors a
rule ahead of time** (AGENTS.md #3's "model never executes confirmed actions"
extends naturally: it also never adjudicates one). Fail-closed throughout: a
tool the model hasn't classified yet, or whose verdict didn't parse, is
`destructive` until a real verdict lands. A tool's schema fingerprint
(sha256 of name+description+parameters) is checked on every reconnect; a
mismatch means the backend changed the tool's contract, and the tool reverts
to `destructive`/pending until reclassified — drift detection with no extra
mechanism. `DESTRUCTIVE_TOOLS` (unchanged exact-name semantics) is the
operator's override, checked first and always winning. A human can also hand-edit
the persisted policy file directly and mark an entry `"source":"operator"`,
which pins it against automatic reclassification. Classification runs at boot;
if the model is unavailable, affected tools stay `Pending` (destructive) and a
background task retries them once the model answers again — a transient model
outage degrades to "everything new is gated," never to "everything new is trusted."

**Known limitation, observed live (§11 decision #11):** fail-closed only
catches a verdict that doesn't parse — it does not catch a confident, clean,
*wrong* one. Against the real Securo endpoint, 28 of 29 tools classified
correctly and one did not, despite an identical schema convention to its
correctly-classified siblings. Reviewing the generated policy file after a new
or updated backend's first boot is a real operational step this design
expects, not a hypothetical one.

### 4.3 Confirmation semantics (Contract 1 extension)

Destructive tools are never dispatched on the model's word alone. The gate is a
**frozen-payload replay**, guaranteeing *what the user confirmed is exactly what executes*:

- **Proposal:** the orchestrator selects a destructive tool (§4.2 classification)
  → it stores the exact `{tool_name, args}` in a pending-action store keyed
  `(client_id, session_id, confirmation_token)` and returns `needs_confirmation`
  + a fresh `confirmation_token` (128 bits of CSPRNG output). The response
  text renders the proposed arguments generically (`key=value, ...`, no
  backend-specific field names) — informed consent from the mechanical prompt
  alone, not dependent on the model having already shown a backend's own
  preview earlier in the conversation.
- **Redemption:** an Order carrying `confirmation_token` is validated against the
  store (same client, same session, unexpired, unused) and the **stored payload is
  dispatched verbatim over MCP. The model is bypassed entirely at execution time.**
  The token is deleted *before* dispatch (single-use even on failure — a transient
  backend error requires a fresh confirmation, which is the safe direction).
- **Token rules:** single-use; TTL `CONFIRM_TTL_MIN` (default 10); bound to the
  issuing client and session. Invalid redemptions return the distinct error codes
  `confirmation_unknown` / `confirmation_expired` / `confirmation_used`.
- **Text while an action is pending (revised, M-confirm):** an explicit
  `confirmation_token` on the Order still redeems outright — that contract path is
  unchanged. For everything else, the harness itself classifies the client's plain
  text against the single most recent unexpired pending action for that
  `(client_id, session_id)` (`crate::confirm`, harness-internal, never exposed to
  adapters — AGENTS.md: adapters must not reach a model). The classifier answers
  exactly one of three things:
  - **Confirm** — dispatches through the same unchanged redemption path: frozen
    payload verbatim, model bypassed at execution, token deleted before dispatch.
  - **Reject** — the action is cancelled immediately (`PendingStore::cancel`,
    same single-use bookkeeping as a redemption) rather than left to expire; a
    later message can never resurrect it.
  - **Unrelated** (including every classifier failure — unreachable model,
    timeout, unparseable output) — the message falls through and is processed as
    an ordinary fresh order, exactly as the pre-revision behavior was. A pending
    action is confirmable any time up to `CONFIRM_TTL_MIN`, not only on the very
    next message — you may ask clarifying questions first.

  **What this does and does not change.** This revises the prior "no implicit
  yes, no natural-language confirmation parsing" rule — that is the explicit
  point of the change. It does **not** touch AGENTS.md #3: the model still never
  executes a confirmed action, and can never see or alter *what* would execute —
  it only ever judges *whether consent was given*, from the same rendered
  arguments already shown in the proposal. `CONFIRM_MODE=token_only` (§5.7)
  restores the strict pre-revision behavior without a code change, for anyone who
  wants the exact-token-only guarantee back.

  **Accepted risks, stated rather than buried:** a wrong `confirm` verdict
  executes a real write — bounded by the action already having been rendered to
  the user and being unalterable by the model, but a genuine weakening versus an
  exact string match. The pending action's own args (model/user-derived text) are
  visible to the classifier, so crafted content is a theoretical prompt-injection
  vector against the verdict, mitigated but not eliminated by prompt wording and
  the strict three-value output contract. The `CONFIRM_TTL_MIN`-wide window means
  a "yes" said several turns later, possibly about something else, could be
  judged a confirmation of an older pending write.
- **Echo:** a successfully executed confirmation includes `{tool, args, result}`
  in the Response's `structured` field. Every redemption attempt — success or
  failure — is additionally appended to the persistent, hash-chained audit
  log (§12.1, M3): the `structured` field is what the caller sees; the audit
  log is the harness's own durable record, independent of any client.

---

## 5. Components

### 5.1 Interface adapters (Part A — interchangeable)
- **Adapter contract:** translate native input → `Order` (mapping native message ids onto `client_msg_id`); render `Response` and error codes → native output; enforce their own user access; authenticate to the Harness API with a client token.
- Each adapter is its **own process** and may run on a different box from the harness.
- **v1 adapter — Telegram bridge:** long-polls Telegram, drops any sender not in `ALLOWED_CHAT_IDS`, ignores its own messages (echo-loop prevention), forwards text (images later), renders replies and confirmation prompts. Outbound-only by design (`getUpdates` long polling needs no inbound ports or TLS). **Exactly one poller per bot token** — a second instance gets HTTP 409 from Telegram, so the adapter container runs as a single replica.
- **Future adapters:** Android app (calls the Harness API over the network), TUI, CLI — none require core changes.

### 5.2 Harness core (Part B — stable)
Hosts the Harness API server, orchestrator, MCP client, pending-action store,
idempotency cache (both in-memory in v1), and model runtime. Contains **no**
Telegram- or Securo-specific code.

### 5.3 Orchestrator (agent loop)
Assembles prompt (system + merged tool schema + order), runs the tool-call loop against Ollama, dispatches read-only tools directly and destructive tools through the safety gate (freeze payload → mint token), caps tool calls per order, enforces the per-order wall-clock budget (`ORDER_BUDGET_S`), returns a final `Response`.

### 5.4 Model runtime
Ollama serving `qwen3:8b`; `num_ctx` 16K–32K; native tool-calling on; low temperature for tool selection. Invoked only for natural-language interpretation — never for executing a confirmed action (§4.3). Accessed **exclusively through the internal `ModelProvider` trait** (§10): Ollama is the first adapter impl; future providers (OpenAI-compat, others) drop in behind the same trait without touching the orchestrator.

### 5.5 MCP client
Connection manager described in §4.2. Handles reconnection and reports which backend a failed call belongs to.

### 5.6 Tool backends (Part C — external, interchangeable)
`linux-mcp-server` (read-only, 1st) → `securo-mcp` (2nd) → future (Docker, etc.). Not built here; connected via config.

### 5.7 Config & secrets
```
# Harness core
HARNESS_API_BIND=127.0.0.1:8080     # LAN/TLS only if a remote adapter needs it (§7)
HARNESS_API_CLIENT_TOKENS=...        # one per adapter
OLLAMA_URL=http://127.0.0.1:11434
OLLAMA_MODEL=qwen3:8b
OLLAMA_NUM_CTX=32768
OLLAMA_TIMEOUT_S=120                 # per generation call
OLLAMA_THINK=off                      # model reasoning effort: off|on|low|medium|high|max
MCP_TOOL_TIMEOUT_S=30                # default per tool call; per-tool override allowed
ORDER_BUDGET_S=180                   # wall-clock cap per order (orchestrator-enforced)
MAX_TOOL_CALLS=8                     # tool-call cap per order (runaway-loop stop)
DEDUP_WINDOW_MIN=60                  # client_msg_id dedup window
SESSION_TTL_H=24                     # idle session expiry
CONFIRM_TTL_MIN=10                   # confirmation-token lifetime
CONFIRM_MODE=model                   # model|token_only — model-interpreted
                                      #   consent (§4.3, revised) vs. the strict
                                      #   pre-revision explicit-token-only behavior
ATTACH_MAX_BYTES=10485760            # inline attachment cap (raw bytes)
DESTRUCTIVE_TOOLS=                    # exact namespaced tool names always destructive,
                                      #   regardless of args (§4.2) — the operator's
                                      #   override on top of the model-driven classifier
                                      #   below; empty/unset is the common case
# added at M3 — adaptive tool classification (§4.2):
TOOL_POLICY_PATH=./arno-tool-policy.json  # persisted, hand-editable classification (AGENTS.md #7 exception)
AUDIT_LOG_PATH=./arno-audit.jsonl         # hash-chained trail of executed frozen actions (§12.1, AGENTS.md #7 exception)
TRANSCRIPT_LOG_PATH=./arno-transcript.jsonl  # hash-chained conversation/tool-call transcript (§12.2, AGENTS.md #7 exception)
TOOL_POLICY_RETRY_S=300               # retry interval for tools left Pending (model was unavailable at boot)
MCP_SERVERS=linux-mcp:<endpoint>      # endpoint: scheme://… (Streamable HTTP)
                                      #   or [Header=Value;Header2=Value2]scheme://…
                                      #   (Streamable HTTP with per-backend request
                                      #   headers — e.g. an Authorization bearer
                                      #   token; header values must not contain a
                                      #   comma, the list separator)
                                      #   or exec:[KEY=VALUE …] <program> [args…] (stdio
                                      #   spawn; leading KEY=VALUE tokens set env vars on
                                      #   the child directly — e.g. exec:MCP_LINUX_TRANSPORT=
                                      #   stdio /usr/local/bin/mcp-linux — never inherited
                                      #   from harness's own env, which stays validated
                                      #   against its own known-vars list)
# mcp-linux backend:
MCP_LINUX_TRANSPORT=http              # http|stdio (serve mode of its own binary)
MCP_LINUX_BIND=127.0.0.1:9001         # ignored in stdio mode
# added at M2 — Securo is external; not a harness-known var. Its bearer token
# and URL are composed into one MCP_SERVERS entry at the deployment layer
# (compose/.env interpolation), e.g.:
#   securo:[Authorization=Bearer <token>]http://host:8765/mcp
# The harness never reads a var named SECURO_* — the header form above is a
# generic mechanism, not Securo-specific code (§5.2, AGENTS.md #6).

# Telegram adapter (separate process/config)
TELEGRAM_BOT_TOKEN=...
ALLOWED_CHAT_IDS=123456789
HARNESS_API_URL=http://127.0.0.1:8080
HARNESS_API_TOKEN=...                 # this adapter's client token
TELEGRAM_HTTP_TIMEOUT_S=240            # must exceed ORDER_BUDGET_S (default 180)
```

Adapters set their Harness-API HTTP timeout above `ORDER_BUDGET_S`.

**Deployment (decided §11):** each part runs as its own Docker container, orchestrated by compose — one service per part (harness, adapters, each MCP backend), configuration injected as env vars, per-service healthchecks (§12.3), adapter pinned to a single replica (Telegram 409 rule, §5.1).

---

## 6. Control flow (one order)

1. Interface receives native input → adapter checks its own user access → builds an `Order` (`client_msg_id` set, `confirmation_token` attached if redeeming) → calls the Harness API with its client token.
2. Harness authenticates the adapter → dedup check: `client_msg_id` seen inside the window → `duplicate_order`, stop. Otherwise load the client-scoped session.
3. **Redemption path:** Order carries a valid `confirmation_token` → dispatch the stored frozen payload verbatim over MCP (no model involvement) → jump to step 6.
4. Normal path: harness loads the session, assembles the prompt; Ollama returns a final answer or a tool call.
5. Tool's safety class checked: read-only → dispatch via MCP client; destructive → freeze `{tool, args}`, mint token, return `needs_confirmation` (never dispatched here, never auto-retried).
6. Result fed back to the model; loop until final answer, a confirmation proposal, or a limit (max tool calls, `ORDER_BUDGET_S` → `order_budget_exceeded`).
7. Harness returns a `Response`; the adapter renders it natively.

---

## 7. Security & auth (two layers, because the parts are decoupled)

- **Layer 1 — user ↔ adapter:** each adapter owns user access. Telegram: chat-ID allowlist. A future Android app: its own login. This keeps user-auth where the interface-specific knowledge lives.
- **Layer 2 — adapter ↔ harness:** a per-client token on the Harness API. The harness trusts authenticated adapters, not raw users — and scopes sessions, dedup entries, and pending actions per client (§4.1, §4.3).
- **Network posture:** bind the Harness API to `127.0.0.1` while everything is local. **Honest tradeoff:** the moment you want a separate Android app talking to the harness over the network, the Harness API becomes a real network surface — it then needs LAN-restriction or TLS + the client-token auth above. That's the genuine cost of interface decoupling; you can defer it entirely until the Android app exists.
- **Unchanged core rules:** harness runs non-root; read-only tools only in v1; destructive tools behind frozen-payload confirmation (§4.3); no raw shell (only discrete MCP tools); MCP backends bound to localhost.

---

## 8. Reliability constraints (small-model realities)

- Raise `num_ctx` first — the default silently truncates and breaks tool use.
- Native tool-calling; expect occasional mis-routes regardless.
- Keep each tool tightly scoped and well-described; broad open surfaces degrade small-model routing.
- **Never** put the model in a deterministic path — scheduled maintenance, health polls, backups run on cron/systemd/n8n. The model only interprets orders (and never executes confirmations, §4.3).
- Cap tool calls per order to stop runaway loops.

**Context budgeting.** Prompt assembly fills `num_ctx` in fixed priority order: merged tool schemas → system prompt → newest history first. Trimming drops **oldest whole turns**, never splitting a tool-call/result pair. If schemas alone crowd out working room as backends accumulate, drop the lowest-priority backend's schema from the prompt and log it loudly — that is a signal to raise `num_ctx` or prune tools, never something to let truncate silently.

**Concurrency.** v1 executes orders from a single global FIFO queue, one order in flight. Ollama serializes generation regardless, and one queue makes session access plus confirmation redemption race-free by construction. Worst-case wait ≈ queue depth × `ORDER_BUDGET_S`, which adapter HTTP timeouts must absorb. Per-session parallelism can be added later without touching either contract.

**Timeouts & retries (all config-driven, §5.7):**

| Knob | Default | Meaning |
|---|---|---|
| `OLLAMA_TIMEOUT_S` | 120 | Per generation call |
| `MCP_TOOL_TIMEOUT_S` | 30 | Per MCP tool call; per-tool override allowed |
| `ORDER_BUDGET_S` | 180 | Wall-clock cap per order, enforced by the orchestrator |

- Retries: transport-level errors on **read-only** calls → at most 1 automatic retry. Application-level errors → never retried. **Writes/destructive calls → never auto-retried**; the confirmation gate is their retry mechanism.
- On budget breach the loop cancels cleanly and returns `order_budget_exceeded`; completed read-only side effects are reported in `structured`.

---

## 9. Milestones

- **M0 — Core + a throwaway CLI adapter.** Stand up the Harness API and an Ollama round-trip, and drive it from a 20-line CLI adapter. *Building the CLI adapter first forces the interface contract to be real from day one — the cheapest guarantee that Telegram never gets welded to the core.*
- **M1 — Telegram adapter + linux-mcp (RO).** Real orders over Telegram, real read-only answers (disk, services, ports). Validates the full loop at zero write risk.
- **M2 — Securo MCP (read).** Add Securo as a second backend; read finance data. Validates multi-backend aggregation and the tool contract.
- **M3 — First confirmed write.** Enable Securo writes behind the frozen-payload confirmation gate (§4.3), gated by adaptive tool classification (§4.2) rather than a hand-enumerated tool list — the live tool surface (9 dual-mode `propose_*` tools) made a static list unsafe as the primary mechanism.
- **Later — prove the decoupling for real:** a second interface adapter (Android/TUI), a backend swap in place of Securo, OCR via attachments, Docker MCP.

Each milestone is independently useful.

---

## 10. Implementation stack — DECIDED: Rust (v0.5)

> **Fully resolved in v0.5** — crate selection pulled forward from "M0 planning"
> and recorded in `STACK.md` (versions verified 2026-08-25). The Option A/B
> rationale from v0.4 is retained below as recorded history.

**Concrete picks (see STACK.md for the full table):**

- **Core baseline:** tokio, serde/serde_json, tracing, thiserror + anyhow.
- **Contract 1 (Harness API):** axum under `/v1`.
- **Contract 2 (MCP):** rmcp — official SDK, spec `2026-07-28` line, client side in the harness, server side for backends.
- **Model runtime:** internal `ModelProvider` trait; first impl is a thin reqwest client over Ollama's **native `/api/chat`** (per-request `num_ctx`; the OpenAI-compat `/v1` endpoint can't set context size and lacks `tool_choice`). Nothing outside `model/ollama.rs` talks to a provider.
- **Interface adapters:** teloxide long-polling (Telegram); clap-based throwaway CLI at M0. Adapters depend only on the `contract` crate.
- **First MCP backend:** `mcp-linux`, Rust on rmcp's server side, workspace member.
- **Deployment:** Docker multi-stage images + compose, one service per part; Ollama stays on the host (GPU/model access), reached via host-gateway.

**Cargo workspace:** `crates/{contract, harness, adapter-cli, adapter-telegram, mcp-linux}` — the `contract` crate makes Contract 1 physically enforceable in code.

**Recorded rationale (v0.4):** Rust over Python greenfield — durable, performant core matching your background; the tradeoff was ecosystem maturity, since closed by rmcp's production adoption. Caveat kept: RustFox is a *monolithic* Telegram + MCP assistant — a reference to mine, not a base to fork wholesale; its Telegram piece informs the adapter, nothing more.

---

## 11. Decisions

### Resolved (v0.4–v0.9)

| # | Question | Outcome | Notes |
|---|---|---|---|
| 1 | Stack | **Rust** | crate selection fully resolved in v0.5 → `STACK.md`; Option A/B rationale retained in §10 |
| 2 | API reach | **localhost-only** | Evidence: `getUpdates` long-polling is outbound-only — no part of v1 exposes an inbound port (core, adapter, backends all co-located). Revisit only when an adapter runs off-box (Android); TLS/LAN hardening lands then |
| 3 | Transport | **HTTP request/response** | SSE additive under `/v1` once a consumer exists; ties to backlog #6 |
| 4 | First MCP backend | **read-only Linux diagnostics** | Concrete server picked at M1; zero write risk |
| 6 | Process management | **Docker containers** | Compose, one service per part; deployment notes in §5.7, topology in STACK.md §6 |
| 7 | Model runtime seam | **ModelProvider trait** | Ollama = first adapter impl over native `/api/chat` (per-request `num_ctx`; compat `/v1` can't set context or `tool_choice`); providers swap without touching the orchestrator (STACK.md §4) |
| 8 | linux-mcp implementation | **Rust + rmcp server** | workspace member `mcp-linux`; concrete diagnostics tool list still picked at M1 |
| 9 | Destructive-tool classification | **Superseded by #11 (M3)** | Originally a config-driven exact-name list; live Securo discovery showed dual-mode (by-argument) write tools an exact-name list can't express — see #11 |
| 10 | Securo MCP auth | **Bearer token, via a generic header mechanism** | Resolves the former `[CONFIRM]` tag in §5.7. `MCP_SERVERS` gained a header-bearing HTTP form (`name:[Header=Value;...]scheme://…`); Securo's token rides `Authorization` through it. The harness never reads a `SECURO_*` var — the token is composed in at the deployment layer (compose/.env), keeping the core backend-agnostic (§5.2, AGENTS.md #6). Verified against the live Securo MCP server: `initialize` + `tools/list` succeeded (29 tools discovered) through this exact code path. Workspace scoping needs no separate var or header — confirmed live that no Securo tool takes a workspace parameter; the bearer JWT's own `ws_id` claim scopes every call server-side. |
| 11 | Tool safety classification (M3) | **Model-classified at discovery, persisted, plain-Rust-evaluated** | Rejected: exact-name env list (can't express dual-mode-by-argument tools, and drifts open as a backend's tool list evolves); standard MCP annotations (no backend obligated to send them — Securo sends none). Adopted: the model reads each tool's name/description/schema once at discovery and emits a persisted rule (`safe`/`destructive`/`destructive_when{key,value}`); dispatch evaluates the rule synchronously, never calling the model per order (§4.2). Fail-closed: unclassified/ambiguous/drifted tools are `destructive` until a real verdict lands; a background task retries tools left pending after a model outage. `DESTRUCTIVE_TOOLS` survives as the operator's override. This required the two AGENTS.md #7 persistence exceptions (tool policy file, audit log) — see #12. **Live-verified** against the real Securo endpoint (`qwen3:4b-instruct-2507-q4_K_M`): boot classified all 29 real tools in ~24s, correctly landing 20 `safe` / 9 `destructive_when{apply,true}` — matching manual inspection exactly, with zero Securo-specific harness code. **One tool was initially misclassified** (`propose_update_recurring_transaction` → `safe`, though its schema is byte-identical in convention to its 8 correctly-classified siblings) — hand-corrected via the operator-pin escape hatch (`"source":"operator"` in the policy file) once found. This is the concrete, observed shape of the accepted risk: fail-closed catches unparseable/ambiguous verdicts, **not** a confidently wrong one — the model reasoned about the `apply` mechanism correctly in its own stated `reason` text but still emitted the wrong `class` label. Operator review of the generated policy file after first boot against a new/updated backend is a real operational step, not optional hardening. Separately live-verified: with Ollama unreachable, boot still succeeds with all 29 tools `pending` (`safe=0 destructive=0 conditional=0 pending=29`) rather than blocking; the background retry task fires correctly on `TOOL_POLICY_RETRY_S` against the real endpoint. |
| 12 | Audit log persistence | **Hash-chained append-only JSONL, `AUDIT_LOG_PATH`** | Backlog #1 required this "before M3 runs against real books"; AGENTS.md #7 requires a spec revision before any persistent store — this is that revision. Each executed frozen action (success or failure) is appended with a hash covering its own content plus the previous entry's hash; a corrupt/tampered file aborts boot rather than silently starting a fresh chain, since tamper-evidence is the entire point. |
| 13 | Conversation/tool-call transcript persistence | **Separate hash-chained append-only JSONL, `TRANSCRIPT_LOG_PATH`** | Rejected: folding this into the audit log — that log's narrow, auditor-facing scope ("what write executed") would blur into a much noisier general narrative (every read, every chat turn). Adopted: a structurally similar but separate module (`crates/harness/src/transcript.rs`) with its own tagged `Event` enum (`user_message`/`assistant_final`/`tool_call`/`tool_result`/`confirmation_requested`/`confirmation_redeemed`), same hash-chain-and-abort-on-corruption posture as the audit log. `confirmation_requested` is recorded here even though `audit.rs` only ever sees a *redeemed* write — this is the only durable record that a write was ever proposed at all, confirmed or not. This is the third AGENTS.md #7 exception (see #12 in AGENTS.md) and resolves backlog #2 ("log hygiene") for this artifact: verbatim tool args/results are stored by design, same posture as the audit log. |
| 14 | Confirmation ownership and interpretation | **Moved into the harness core (`crate::confirm`); model-interpreted with a `token_only` escape hatch** | Rejected: fixing each adapter's confirmation handling separately — the actual bug was that confirmation was an adapter concern at all, when `AGENTS.md` already forbids adapters reaching a model and the harness already owns the pending-action store; widening `adapter-telegram`'s literal string list (e.g. accepting "yes"/"ok" alongside `[CONFIRM]`) — still just enumerating phrasings, the exact approach rejected. Adopted: one classifier (`crate::confirm`, mirroring `policy.rs`'s proven shape) judges confirm/reject/unrelated for *any* client uniformly; `Confirm` dispatches through the unchanged `redeem()` (AGENTS.md #3 intact — the model judges consent, never content, and can neither see nor alter what executes); `Reject` cancels immediately via a new `PendingStore::cancel`; every failure mode (unreachable model, timeout, unparseable output) is treated identically to `Unrelated`, since unlike `policy.rs`'s tool classification this sits *on* the request path with no pre-computed synchronous default available. `CONFIRM_MODE=token_only` restores the strict exact-token behavior without a redeploy. Found live: `adapter-cli` hardcoded `confirmation_token: None` on every order and only ever printed a token it never resent — a destructive action proposed through it could never execute, confirmed by a real transcript showing the model repeatedly claiming success for a transaction that never ran; `adapter-telegram` discarded its own pending token on any reply other than the exact literal `[CONFIRM]`. |

### Still open

None blocking M3.

**Resolved since v0.2** (folded into this revision):
- Attachment mechanism → dual-field schema, inline base64 in v1, upload endpoint later (§4.1).
- Session scoping → client-namespaced `(client_id, session_id)` (§4.1).
- Confirmation binding → frozen-payload replay, single-use session-bound tokens (§4.3).
- Idempotency → `client_msg_id` + in-memory dedup window; timeouts and retry policy defined (§4.1, §8, §5.7).
- v0.3 → API versioning, concurrency queue, context-budget and session-lifecycle policies folded in; remaining gaps moved to §12.

---

## 12. Backlog (tracked, not designed yet)

None of these block M0–M1.

1. ~~**Audit log**~~ — **Resolved in M3** (§11 decision #12): hash-chained append-only JSONL, `AUDIT_LOG_PATH`. Retention/rotation remains explicitly out of scope (ops concern, not a harness feature).
2. ~~**Log hygiene**~~ — **Resolved in v0.8** (§11 decision #13): two components are explicitly allowed to hold tool args/results verbatim — the audit log (executed writes only) and the transcript (the full conversation/tool-call narrative). Every other component, especially `tracing::` output, still may only log codes and tool names, never payloads (AGENTS.md's existing convention, unchanged). Rotation/secret handling for both files remains explicitly out of scope (ops concern, not a harness feature).
3. **Contract tests + mock MCP server** — golden tests for the Harness API and a stub backend exercising discovery/namespacing/timeouts; health/readiness endpoints surfaced as Docker healthchecks (§11.6).
4. **Prompt-injection stance** — record the accepted-risk rationale (read-only v1 + frozen-payload gate) and revisit when attachments/OCR land.
5. **M3 dry-run** — the first confirmed write targets a sandbox, never production books. Since M2 confirmed workspace scoping lives entirely in the bearer JWT's `ws_id` claim (no separate `SECURO_WORKSPACE_ID` var), the sandbox boundary for M3 must be a *separate token* minted against a test workspace — not a config var the harness plumbs through.
6. **Correlation ID** — Response carries no `order_id`; required once SSE/WS push or async orders exist (ties to §11.3).
7. ~~**Securo `propose_*` tools are mutating, not read-only**~~ — **Resolved in M3** (§11 decision #11): the model classifies each tool at discovery from its schema alone, deriving the `apply`-argument condition itself rather than the harness hardcoding it. All 9 `propose_*` tools now correctly gate on `apply: true`; preview calls (no `apply`) dispatch freely, matching Securo's own preview/apply contract exactly instead of either double-confirming previews or leaving applies unguarded.
8. **Runtime client registration** — today every adapter's token is static config: harness reads the whole map from `HARNESS_API_CLIENT_TOKENS`, each adapter carries its own copy of just its own secret (§5.7, §7 layer 2). An alternative is registering clients against a *running* harness (admin action: supply `client_id` + token, paste the same token into the adapter). Deferred, not rejected — it does not remove the two-places-one-secret duplication (the adapter still receives the secret out of band), and it costs three things this design currently avoids: a **persisted** token store (violates §4.1's in-memory-only rule — otherwise every restart de-registers every adapter), an **authenticated admin surface** to mint tokens (which needs its own bootstrap credential from env, so the env var comes back), and a **revocation/rotation** story. Worth building when adapters become dynamic — third-party clients, per-device tokens, self-service onboarding, or rotation without downtime — at which point it is a designed feature (admin auth, persistence, TTL, revocation), not a config tweak.
