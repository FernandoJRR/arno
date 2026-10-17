# Home Server Harness — Spec v0.12 (draft)

Supersedes v0.9 (v0.10 was the log-rotation revision, §11 #15, which never
bumped this header). Restructured around a single principle: **three independently
replaceable parts, joined by two stable contracts.** Any part can be swapped
without touching the other two. Concrete stack and deployment topology live in
the companion file `STACK.md`; agent-facing rules in `AGENTS.md`.

> Status: draft for review. Assumptions needing confirmation are marked
> **[CONFIRM]**. Open decisions are in §11.
>
> **New in v0.12** — Plugins (§4.5, §11 #21–#24). Each MCP backend is a
> versioned TOML manifest in `PLUGINS_DIR` (one explicit exception to
> env-only config) with tool groups, attachment params and declared
> permissions. Per-plugin secrets in `<id>.env`. Plugin trust
> (untrusted = every call needs approval) lives in the tool policy file; a
> disclosure report is written at every load. A flagged tool-group router
> (`ROUTER_ENABLED`) narrows what a small model sees. Reload between orders
> on SIGHUP. `MCP_SERVERS` is deprecated.
>
> **Also in v0.11** — OCR accuracy (§4.4, §11 #19): image preprocessing,
> confidence-picked multi-read, `[?word]` doubt marks, `tessdata_best`,
> `MCP_OCR_PREPROCESS`.
> Number hygiene (§11 #20): short answers (`OLLAMA_NUM_PREDICT`), relayed
> backend `checks`, OCR misread checks, ungrounded-number note
> (`NUMBER_GUARD`).
>
> **Changelog v0.9 → v0.11** — Generic decisions (§4.3 rewritten, §11 #18).
> A frozen destructive call now ends the order with a neutral
> `Response.decision` (id, prompt, details, options, `free_text`,
> `expires_at`) that each adapter renders natively (Telegram inline buttons,
> CLI numbered menu); the answer is a new Order carrying `decision_answer`.
> The core owns *what* the options are, adapters only *how* they look. Only an
> explicit `approve` on a live decision id executes; free text and plain
> replies never do. Model-interpreted plain-text confirmation (v0.9,
> `crate::confirm`) is **removed**: a 4B model judging consent was the
> weakest link of the write path. `CONFIRM_MODE` is still parsed but ignored.
> `needs_confirmation`/`confirmation_token` stay populated for `kind:
> confirm` (token == decision id) and deprecated until `/v2`.
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
- ~~OCR / image pipeline (carried as an attachment on an Order → OCR tool → structure → Securo write).~~ **Text extraction resolved (§11 decision #17)**: attachment → `mcp-ocr.extract_text` → text lands in the model's context; the model still has to structure and dispatch a Securo write itself (no new core mechanism needed there — same tool-call path as any other order). PDF bills, `ref`-form upload, and an Ollama-vision engine remain follow-ups.
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
  { session_id, text, client_msg_id?, confirmation_token? (deprecated),
    attachments?: [ {mime, data_b64?, ref?} ], client_meta?: {...},
    decision_answer?: { id, value? | text? } }
  ```
- **Response (out):**
  ```
  { text, needs_confirmation?: bool (deprecated), confirmation_token?: str (deprecated),
    structured?: [...], decision?: {...} (§4.3) }
  ```
- **Sessions:** the harness keeps per-session context. Sessions are **namespaced by the authenticated client**: the store is keyed `(client_id, session_id)`. Each adapter maps its native identity (Telegram chat ID, Android user) onto a `session_id` that is local to that adapter. A valid token addressing another client's session gets `unknown_session` — cross-adapter session hijacking is impossible by construction. Sessions are deliberately not portable across adapters. The core never sees Telegram objects. **Lifecycle:** in-memory only in v1 — a harness restart clears sessions and adapters simply start fresh; idle expiry via `SESSION_TTL_H` (default 24); conversation history is trimmed by the context-budget policy (§8).
- **Idempotency:** adapters SHOULD send `client_msg_id` — a stable identifier for the native input (the Telegram adapter passes its native `update_id`). The harness keeps an in-memory LRU of recently seen ids (`DEDUP_WINDOW_MIN`, default 60); a repeat within the window is rejected with `duplicate_order` and **never re-executed**. The window is memory-only: a restart clears it, which is acceptable because cross-restart duplicates are already bounded by adapter offset handling.
- **Attachments:** optional binaries so *any* interface can feed the future OCR path identically. The schema carries both transports now so it never breaks: `data_b64` = inline base64 (**v1 implementation**, cap 10 MB raw bytes per order, `ATTACH_MAX_BYTES`; wire size ≈ +33%), `ref` = opaque handle for a future `POST /v1/files` upload endpoint backed by server-side TTL temp storage (built when the OCR pipeline lands). Exactly one of `data_b64`/`ref` per attachment. Downloading files from Telegram (bot API limit 20 MB) is adapter work, not harness work.
- **Auth:** the harness authenticates *adapters* via a per-client token. It does **not** know about end-users — that's the adapter's job (§7).
- **Errors:** non-2xx responses carry JSON `{ "error_code": ... }` so every adapter renders failures identically. Defined codes: `unauthorized`, `invalid_order`, `unknown_session`, `duplicate_order`, `confirmation_unknown`, `confirmation_expired`, `confirmation_used`, `order_budget_exceeded`, `backend_unavailable`, `decision_unknown` (404), `decision_expired` (410), `decision_used` (409), `decision_invalid_value` (400).

### 4.2 Contract 2 — MCP (tool boundary)

- The harness is an MCP client. At startup it connects to the MCP server of each plugin (§4.5), discovers their tools, namespaces them (`linux.get_disk_usage`, `securo.propose_create_transaction`, `ocr.extract_text`), and presents one merged schema to the orchestrator.
- **`mcp-ocr`** (§11 decision #17) is the OCR backend: one read-only tool, `extract_text`, that turns an attachment handle (§4.4) into recognized text via Tesseract (CPU, no VRAM contention with the tool-calling model — the right default on a GPU already saturated by qwen3). Read-only by construction, so it auto-classifies `safe` (§4.2 classifier below) with zero backend-specific harness code.
- **Securo is one plugin.** Replacing it with another app is a manifest change; if the replacement speaks MCP, the harness needs no code change.
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

### 4.3 Decision semantics (Contract 1 extension)

Destructive tools are never dispatched on the model's word alone. When a human
choice is needed the order **ends** with a neutral `decision`; the answer is a
**new** Order. Nothing is suspended mid-flight, so "one order in flight"
(§8) and the MCP tool timeout are untouched.

- **Decision object** (`Response.decision`):
  ```
  { id, kind: confirm|choose|form, prompt, details: [{label, value}],
    options: [{value, title, style: primary|danger|default}],
    free_text: bool, expires_at: RFC3339 }
  ```
  The core owns the options; adapters own rendering (buttons, menus). An
  adapter that ignores `decision` still shows `text`, but cannot approve.
  Only `kind: confirm` is emitted today; `choose` (schema-driven
  disambiguation) and `form` (MCP elicitation subset) are reserved.
- **Proposal (`kind: confirm`):** the orchestrator selects a destructive tool
  (§4.2 classification) → stores the exact `{tool_name, args}` keyed
  `(client_id, session_id, decision_id)` and returns the decision. `prompt`,
  `details` and `options` are built **deterministically** from the frozen
  payload and the tool schema (one detail row per argument, label from the
  schema property `title` else the parameter name) — never from model text.
  Options are `approve` / `reject`; `free_text: true`.
- **Answer** (`Order.decision_answer`, exactly one of `value` / `text`): the id
  is validated (same client and session, unexpired, unused) and **consumed
  before anything else happens** — single-use even on failure.
  - `approve` → the stored payload is dispatched verbatim over MCP, model
    bypassed entirely, never auto-retried.
  - `reject` (or any other offered value) → nothing executes.
  - a value not offered → `decision_invalid_value`.
  - `text` → **never approves**: the decision is discarded, the model is told
    the proposal was not executed, and the text runs as a normal order (a new
    proposal yields a new decision).
- **Supersede rule:** at most one live decision per `(client_id,
  session_id)`. A new proposal, an answer, or any plain order (no
  `decision_answer` / token) discards older ones, so a stale button can never
  approve an older payload. Plain replies such as "yes" never execute.
- **Id rules:** `dcs_` + 128 bits of CSPRNG; TTL `CONFIRM_TTL_MIN` (default
  10); errors `decision_unknown` / `decision_expired` / `decision_used`.
- **Legacy tokens (deprecated until `/v2`):** for `kind: confirm` the Response
  also carries `needs_confirmation: true` and `confirmation_token ==
  decision.id`; an Order carrying `confirmation_token` still redeems through
  the same path with the `confirmation_*` error codes.
- **Echo:** a successfully executed confirmation includes `{tool, args, result}`
  in the Response's `structured` field. Every redemption attempt — success or
  failure — is additionally appended to the persistent, hash-chained audit
  log (§12.1, M3): the `structured` field is what the caller sees; the audit
  log is the harness's own durable record, independent of any client.

---

### 4.4 Attachment ingestion (Contract 1 extension)

Image bytes have no path to a tool without this mechanism: `Attachment.data_b64`
arrives on the `Order`, but the model authors tool arguments itself — it cannot
emit a base64 blob, and no tool can see the wire payload directly. The harness
bridges the gap with a generic, tool-agnostic handle:

- **Persist:** for each inline (`data_b64`) attachment whose mime is an allowed
  image type (`image/png`, `image/jpeg`, `image/webp`) and size is within
  `ATTACH_MAX_BYTES`, the harness decodes it and writes the raw bytes to
  `ATTACH_DIR/<handle>` (mode 0600), where `<handle>` is a fresh 128-bit CSPRNG
  token (same minting approach as a confirmation token, §4.3). `ref`-form
  attachments are skipped (future `POST /v1/files` upload endpoint).
- **Manifest:** when an order persisted at least one attachment, the harness
  injects one extra `System` message into that order's prompt assembly (never
  into the persistent session store, so it never leaks into a later order):
  `"Attached files available to tools: [handle=..., type=...]. To read or act
  on a file, pass its handle as that tool's `image_ref` argument."` (the
  declared attachment parameter names). This
  names a handle and a mime type, **never a specific tool** (AGENTS.md #1) — any
  attachment-consuming backend (`mcp-ocr` today, others later) wires in purely
  by declaring `tools.attachment_params` in its plugin manifest (§4.5), no core
  change. It sits right after the latest user turn, and until the first tool
  runs the model is offered only tools with an attachment parameter (small
  models ignore the file otherwise). Legacy `MCP_SERVERS` backends keep the
  old `*_ref` name-suffix convention.
- **`checks` convention (§11 #20):** any tool result may carry a top-level
  `checks: [string]` (a JSON object, or a string holding one). The harness
  appends them verbatim to that order's final answer under "⚠ Checks:"
  (deduplicated, at most 5, 300 chars each), so a deterministic backend
  finding reaches the user even when the model ignores it. The model is
  given the result **without** `checks` (it would only repeat them); the
  transcript keeps the full result. Tool-agnostic, like attachment params.
- **Read first (§11 #25):** when exactly one tool offered on the first turn
  takes attachments, with one attachment param and no other required
  argument, the harness calls it itself for each handle before the model's
  first turn. The calls are recorded and gated exactly like model calls
  (transcript `tool_call` id `prefill-N`; an untrusted or destructive tool
  ends the order on a decision instead). The model then answers from the
  result, with the full catalogue (or routed `related` groups) available.
- **Nudge:** when no read-first applies (several attachment tools, or
  extra required arguments) and the model answers in plain text without
  having called any tool, the answer is withheld once and the model is told
  to call the tool that takes the file. At most one nudge per order.
- **Handle repair:** with exactly one attachment on the order, an attachment
  argument naming an unknown handle is rewritten to that handle before
  logging, classification and dispatch (small models drop characters when
  copying 32-hex handles). With several attachments nothing is guessed.
- **Resolve:** a tool receiving a handle (e.g. `ocr.extract_text`'s `image_ref`)
  joins it against its own `*_ATTACH_DIR` mount of the same shared volume.
  `extract_text` takes no language argument: `MCP_OCR_LANG` alone decides
  (the model forced `eng` on Spanish bills).
- **Recognition (§11 #19):** `extract_text` reads the original image and a
  cleaned copy (upscaled, contrast-stretched, deskewed, kept grayscale) with
  page-segmentation modes 3 and 6 in parallel, keeps the reading with the
  highest confidence-weighted word count, and writes words Tesseract is
  under 60% sure of as `[?word]`. Before returning it drops noise: words
  less than half alphanumeric (printed rules, borders) and lines where
  every word is doubtful and none is a number (logo, photo edge). The
  result adds `confidence`, `doubtful_words`, `dropped_lines`, `variant`
  and `checks` to `{text, chars}`. `checks` names likely misreads: a number
  that fits no relation with its close neighbours but would, if one
  commonly confused digit (3↔8, 0↔9, 1↔7, …) were different — `x = a + b`,
  `b − a = x` over three different lines, or an amount written in words
  (`text2num`, English/Spanish). Integers glued to letters (`C131`) are
  codes and never repaired; a repair must be unique. A repair backed by two
  independent pieces of evidence (a words amount plus a relation, or two
  relations with no shared partner) is **applied in place** as
  `80.46 [corrected from 30.46]`, so the model copies the right value;
  weaker ones stay "may be a misread of". `corrections` counts the applied
  ones. The cleaned copy is a
  private temp file of the OCR process, never on the shared volume.
  Handle charset is restricted to `[A-Za-z0-9_-]`, checked on both the minting
  and resolving sides — a traversal guard against `../` even though the
  harness itself never emits anything else.
- **Lifetime:** handles are scoped to one order. The harness deletes each
  order's files on every return path (success, budget-exceeded, backend
  error) — best-effort, never blocking the response. The entire `ATTACH_DIR`
  is purged at harness boot regardless, since anything surviving a prior
  process exit is stale by construction. This is scratch, not durable state
  (AGENTS.md #7): unlike the tool-policy/audit/transcript exceptions, nothing
  here needs to survive a restart.

### 4.5 Plugins (§11 #21–#24)

A plugin is how a hosted service's MCP tools reach the model. It contributes
**only tools and text** — no hooks, no code in the core path. The core reads
generic metadata from it and never names an app (AGENTS.md #1).

**Manifest.** `PLUGINS_DIR/<id>.toml`, file name equal to `id`:

```toml
arno_manifest = 1                 # unknown version or key → load fails loudly
id = "securo"                     # [a-z0-9-]; the tool namespace (securo.<tool>)
name = "Securo finance"
version = "1.4.0"
provenance = "first-party"        # first-party | third-party (label only)

[server]
kind = "remote"                   # remote | container (operator-run, image pinned)
url = "https://host:3130/mcp"
# image = "registry/x@sha256:..." # required for container
auth = { kind = "bearer", secret = "SECURO_MCP_AUTH" }

[permissions]                     # declared, shown in the disclosure report
egress = ["host:3130"]
volumes = []                      # e.g. ["attachments:/attachments:ro"]
secrets = ["SECURO_MCP_AUTH"]     # read from <id>.env, never the process env

[tools]
enabled = ["*"]                   # operator allowlist, `*`/`?` globs on bare names
attachment_params = []            # params taking an attachment handle (§4.4)

[[groups]]
id = "transactions"
when = "Log, list or change expenses and income"   # routing hint, ≤ 80 chars
how = "Resolve account names first."               # ≤ 400 chars, shown only when active
tools = ["list_transactions", "propose_create_transaction"]
related = ["accounts", "ocr.read"]                 # local or <plugin>.<group>
```

- Group ids are qualified as `<plugin>.<group>`. A plugin without groups is
  one implicit group `<plugin>`; ungrouped tools go to `<plugin>.other`. A
  group naming an undiscovered tool is a warning (servers drift).
- Disabled tools are removed from both the prompt and dispatch.
- `when`/`how`/`name` are stripped of control characters and capped at load;
  third-party tool descriptions are capped at 400 chars. Group text reaches
  the model only inside a delimited data block, never as instructions.
- **No manifest field can loosen policy.** There is no "safe" field; the
  classifier, operator pins and `DESTRUCTIVE_TOOLS` stay authoritative.

**Secrets.** `<id>.env` (`KEY=VALUE`, no expansion) beside the manifest. The
harness reads it at load and attaches values only to that plugin's
connection; values never enter the process env, logs, errors or the
transcript. A missing declared secret fails that plugin only.

**Failures.** A manifest parse error aborts the load. A plugin whose secrets,
connection or discovery fail is kept as `failed` and skipped; the rest load.
An unreachable legacy `MCP_SERVERS` backend still aborts boot (unchanged).
When any manifest plugin failed, one summary warning follows, e.g.
`1 of 3 enabled plugins could not be reached — running without them`.

**Containers.** The harness never gets the Docker socket. For
`kind = "container"` the disclosure report carries a compose snippet; the
operator adds and runs it.

**Trust (§11 #22).** Stored per plugin in the tool policy file under
`plugins` — no new persistent store (AGENTS.md #7):

- A new plugin is `untrusted`: **every** call produces a decision, reads
  included, regardless of classifier output.
- `trusted`: the normal classifier policy (§4.2) applies.
- First-party plugins whose tools were already classified before plugins
  existed are migrated as `trusted`.
- Any change to the manifest hash, or any new or changed tool schema
  fingerprint, sets the plugin back to `untrusted`.
- Promotion is by the operator: set `plugins.<id>.level` to `"trusted"` and
  `source` to `"operator"` in the policy file, then restart or reload.
- Legacy `MCP_SERVERS` backends have no trust entry and keep classifier-only
  behavior.

**Disclosure.** At every load the harness logs one line per plugin and writes
`PLUGINS_REPORT_PATH` (JSON): kind, server host, egress, volumes, secret
*names*, status, trust, tool count and safe/conditional/destructive/pending
counts, groups, and a compose snippet for container plugins. It is a derived
snapshot, overwritten each load and never read back — not a store.

**Router (§11 #23), behind `ROUTER_ENABLED` (default off).** Before the main
loop of an order, one small completion with structured output (Ollama
`format`, a JSON schema whose enum is the candidate group ids plus `none` and
`all`) picks a group from `id: when` lines, the latest message and up to four
earlier turns. The pick is sticky for the order; after the first dispatch the
group's `related` groups are added. `none` offers no tools; `all`, an unknown
id or a router error fall back to the unrouted selection. With attachments,
candidates are limited to groups holding a tool with an attachment param. The
pick is recorded as a `router_pick` transcript event with its latency. Flag
off = exactly the unrouted behavior. No auto-retry with `all` on a wrong pick
in v1: measure first (`arno-replay`).

**Reload (§11 #24).** The loaded set sits behind one swappable handle; each
order works on a snapshot. A reload re-reads the policy file (to pick up
promotions) and `PLUGINS_DIR`, connects, discovers, reconciles trust and
classification, writes the disclosure, then swaps. It runs as a job on the
FIFO queue, so it always lands between orders (§8). Triggered at boot and by
SIGHUP. There is no admin API: a future one must be separate from Contract 1
with its own token, and adapters must never be able to install or reload
plugins.

## 5. Components

### 5.1 Interface adapters (Part A — interchangeable)
- **Adapter contract:** translate native input → `Order` (mapping native message ids onto `client_msg_id`); render `Response` and error codes → native output; enforce their own user access; authenticate to the Harness API with a client token.
- Each adapter is its **own process** and may run on a different box from the harness.
- **v1 adapter — Telegram bridge:** long-polls Telegram, drops any sender not in `ALLOWED_CHAT_IDS`, ignores its own messages (echo-loop prevention), forwards text (images later), renders replies and renders `decision`s (§4.3) as inline keyboard buttons, sending the tapped option back as `decision_answer`. Outbound-only by design (`getUpdates` long polling needs no inbound ports or TLS). **Exactly one poller per bot token** — a second instance gets HTTP 409 from Telegram, so the adapter container runs as a single replica.
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
`linux-mcp-server` (read-only, 1st) → `securo-mcp` (2nd) → `mcp-ocr` (read-only OCR: `extract_text`, §11 decision #17) → future (Docker, etc.). Not built here; connected via config.

### 5.7 Config & secrets
```
# Harness core
HARNESS_API_BIND=127.0.0.1:8080     # LAN/TLS only if a remote adapter needs it (§7)
HARNESS_API_CLIENT_TOKENS=...        # one per adapter
OLLAMA_URL=http://127.0.0.1:11434
OLLAMA_MODEL=qwen3:8b
OLLAMA_NUM_CTX=16384                  # 16k keeps qwen3:4b ~94% on a 6 GB GPU; 32k spilled
                                      #   39% to CPU, ~4 tok/s (§11 #20)
OLLAMA_TIMEOUT_S=120                 # per generation call
OLLAMA_THINK=off                      # model reasoning effort: off|on|low|medium|high|max
OLLAMA_SEED=42                        # fixed sampler seed: deterministic tool selection
                                      #   (§11 decision #16); 0 = provider default
OLLAMA_TOP_K=1                        # greedy sampling: 1 = pick the top token, killing the
OLLAMA_NUM_PREDICT=400                # token cap per completion; a cut final answer is
                                      #   marked (0 = provider default, §11 #20)
NUMBER_GUARD=true                     # name numbers in a final answer that no tool result
                                      #   or user message contains (§11 #20)
                                      #   sampling tail that lets a small model answer in
                                      #   prose instead of emitting the structured call
                                      #   (§11 decision #16); 0 = provider default
MCP_TOOL_TIMEOUT_S=30                # default per tool call; per-tool override allowed
ORDER_BUDGET_S=180                   # wall-clock cap per order (orchestrator-enforced)
MAX_TOOL_CALLS=8                     # tool-call cap per order (runaway-loop stop)
DEDUP_WINDOW_MIN=60                  # client_msg_id dedup window
SESSION_TTL_H=24                     # idle session expiry
CONFIRM_TTL_MIN=10                   # decision / confirmation-token lifetime
# CONFIRM_MODE                        # deprecated (§11 #18): parsed, ignored, warns at boot
ATTACH_MAX_BYTES=10485760            # inline attachment cap (raw bytes)
ATTACH_DIR=./data/attachments        # ephemeral scratch dir for inbound image attachments
                                      #   (§4.4); purged at boot, per-order lifetime, never
                                      #   the tool-policy/audit/transcript kind of durable
DESTRUCTIVE_TOOLS=                    # exact namespaced tool names always destructive,
                                      #   regardless of args (§4.2) — the operator's
                                      #   override on top of the model-driven classifier
                                      #   below; empty/unset is the common case
# added at M3 — adaptive tool classification (§4.2):
TOOL_POLICY_PATH=./data/arno-tool-policy.json  # persisted, hand-editable classification (AGENTS.md #7 exception)
AUDIT_LOG_PATH=./data/arno-audit.jsonl         # hash-chained trail of executed frozen actions (§12.1, AGENTS.md #7 exception)
TRANSCRIPT_LOG_PATH=./data/arno-transcript.jsonl  # hash-chained conversation/tool-call transcript (§12.2, AGENTS.md #7 exception)
TOOL_POLICY_RETRY_S=300               # retry interval for tools left Pending (model was unavailable at boot)
# added at v0.9 → v0.10 — log rotation & verification (§11 decision #15):
AUDIT_LOG_ROTATE_BYTES=2097152        # rotate audit log at next boot past this size (2 MiB); 0 disables
TRANSCRIPT_LOG_ROTATE_BYTES=10485760  # rotate transcript at next boot past this size (10 MiB); 0 disables
LOG_KEEP_SEGMENTS=12                  # rotated segments retained per store (oldest pruned first)
LOG_VERIFY_ON_BOOT=false              # full-chain verify (all segments) at boot; fails boot on any break

# added at v0.12 — plugins (§4.5):
PLUGINS_DIR=./config/plugins          # <id>.toml manifests + optional <id>.env secrets
PLUGINS_REPORT_PATH=./data/arno-plugins-report.json  # disclosure report, rewritten every load
ROUTER_ENABLED=false                  # tool-group router (§11 #23)

MCP_SERVERS=                          # DEPRECATED (§11 #21): each entry becomes an implicit
                                      #   legacy plugin, with a warning at boot; the same id
                                      #   in a manifest and here fails loudly.
                                      # Format, name:<endpoint>: scheme://… (Streamable HTTP)
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
# mcp-ocr backend (§11 decision #17), its own process — none of these are
# harness-known vars:
MCP_OCR_TRANSPORT=http                # http|stdio (serve mode of its own binary)
MCP_OCR_BIND=0.0.0.0:9002             # ignored in stdio mode
MCP_OCR_ATTACH_DIR=/attachments       # this backend's own mount of the shared scratch
                                      #   volume (§4.4) — named independently of the
                                      #   harness's ATTACH_DIR, per component (§5.7 rule)
MCP_OCR_LANG=eng+spa                  # tesseract -l language code(s); bills skew Spanish
MCP_OCR_ENGINE=tesseract              # only "tesseract" is built; "ollama-vision" is a
                                      #   documented future OcrEngine impl (STACK.md §3)
MCP_OCR_PREPROCESS=on                 # on|off; off reads only the original image (§11 #19)
# Securo is external and, since v0.12, a plugin: URL in
# config/plugins/securo.toml, bearer token in config/plugins/securo.env.
# The harness never reads a var named SECURO_* (§4.5, AGENTS.md #1).

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

1. Interface receives native input → adapter checks its own user access → builds an `Order` (`client_msg_id` set, `decision_answer` attached if answering a decision) → calls the Harness API with its client token.
2. Harness authenticates the adapter → dedup check: `client_msg_id` seen inside the window → `duplicate_order`, stop. Otherwise load the client-scoped session.
3. **Decision path:** Order carries `decision_answer` (or a legacy `confirmation_token`) → resolve it without the model; only `approve` dispatches the stored frozen payload verbatim over MCP → jump to step 7. Any other order first supersedes pending decisions.
4. Normal path: harness loads the session, assembles the prompt; Ollama returns a final answer or a tool call.
5. Tool's safety class checked: read-only → dispatch via MCP client; destructive → freeze `{tool, args}`, mint a decision id, return a `decision` (never dispatched here, never auto-retried).
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

### Resolved (v0.4–v0.12)

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
| 14 | Confirmation ownership and interpretation | **Partly superseded by #18 (model interpretation removed; ownership stays in the core).** Moved into the harness core (`crate::confirm`); model-interpreted with a `token_only` escape hatch** | Rejected: fixing each adapter's confirmation handling separately — the actual bug was that confirmation was an adapter concern at all, when `AGENTS.md` already forbids adapters reaching a model and the harness already owns the pending-action store; widening `adapter-telegram`'s literal string list (e.g. accepting "yes"/"ok" alongside `[CONFIRM]`) — still just enumerating phrasings, the exact approach rejected. Adopted: one classifier (`crate::confirm`, mirroring `policy.rs`'s proven shape) judges confirm/reject/unrelated for *any* client uniformly; `Confirm` dispatches through the unchanged `redeem()` (AGENTS.md #3 intact — the model judges consent, never content, and can neither see nor alter what executes); `Reject` cancels immediately via a new `PendingStore::cancel`; every failure mode (unreachable model, timeout, unparseable output) is treated identically to `Unrelated`, since unlike `policy.rs`'s tool classification this sits *on* the request path with no pre-computed synchronous default available. `CONFIRM_MODE=token_only` restores the strict exact-token behavior without a redeploy. Found live: `adapter-cli` hardcoded `confirmation_token: None` on every order and only ever printed a token it never resent — a destructive action proposed through it could never execute, confirmed by a real transcript showing the model repeatedly claiming success for a transaction that never ran; `adapter-telegram` discarded its own pending token on any reply other than the exact literal `[CONFIRM]`. |
| 15 | Log rotation, verification, and location | **JSONL retained; boot-time size rotation with cross-file chain continuation; `GET /v1/logs/verify`; single canonical `./data/` location** | Evaluated before adopting: **SQLite/redb as the store** — rejected because the chain's tamper-evidence is *replaying the bytes*, and an embedded engine's free pages/WAL can be edited without the chain seeing it (posture weakens); loses the `jq`/`tail` inspectability compose deliberately cites; ~900-line migration for an append problem the files don't have. If queryability ever matters, the additive path is a read-side SQLite *mirror* of the JSONL, never a replacement. **External anchoring (Rekor/signed checkpoints)** — rejected now (network dependency + new infra for a single-user home server; YAGNI), noted for a future threat-model change. **JSON format family** — single JSON array/pretty-printed document rejected outright (whole-file rewrite per append breaks append-only and the chain invariant); RFC 7464 JSON Sequences rejected (no advantage — payloads are JSON-escaped anyway); Parquet/Avro/ORC rejected (binary, loses inspectability, not append-ordered); CEF/LEEF/Syslog-JSON rejected (SIEM transport envelopes, not local durable storage). **Adopted**: JSONL stays byte-identical (NDJSON-conformant); the only wire-format addition is the rotation handoff fields inside the first entry of a new segment (additive, ignored by old parsers). Rotation fires **at boot only** (never mid-run — keeps AGENTS.md #5's single-writer FIFO untouched and the seam deterministic): if the active file exceeds `AUDIT_LOG_ROTATE_BYTES`/`TRANSCRIPT_LOG_ROTATE_BYTES`, it is renamed to `<stem>-<UTC timestamp>.jsonl` beside the active file and the new active file's first entry chains onto the rotated file's last hash via a `rotation` handoff field (`from_file`, `prev_chain_head`) — a mismatch against the newest sibling's tail is `Corrupt` (fail-closed, same posture as an edited line). `LOG_KEEP_SEGMENTS` prunes oldest rotated segments (whole files only, never interiors; the active file is never pruned). `LOG_VERIFY_ON_BOOT` replays all segments as one logical chain at open and aborts boot on any break. `GET /v1/logs/verify` (same bearer auth as all other routes; returns counts/hashes only, never payloads) is the operator's runtime check. **Location**: `./data/` is the one canonical home for all three stores in every mode (local dev defaults moved from `./arno-*` to `./data/arno-*` — the repo-root copies were stale dev leftovers and were consolidated away); rotated segments live in the same directory, keeping segment discovery, permissions, backup, and the bind mount trivial. Tail-truncation of the newest segment remains undetectable from the files alone (true before this change too) — the off-host copy (STACK §9 backup) is the mitigation. Retention/rotation was previously deferred as an ops concern (backlog #1/#2); this decision brings rotation and verification into the harness as config-driven features while off-host backup remains ops. |
| 16 | Tool-recognition reliability for small models | **Deterministic sampling + text tool-call rescue + success-claim guard — all output-side, no user phrasing requirements** | Evidence: live transcript (qwen3:4b-instruct-2507) showed 4 of 12 assistant finals asserting execution ("has been successfully created/applied", "is now live") with ZERO tool call in flight. Evaluated: **keyword tool-surface preselection** — rejected because matching the user's order text against tool names rewards specific vocabulary and degrades on loose phrasing ("handle my Amazon purchase"); the scope rule is that no layer may constrain user phrasing. Its replacements (intent-classifier first pass; descriptions-only first pass) are deferred until live evals show the 29-tool surface still distracting the model. **JSON grammar-constrained decoding** — deferred (Ollama `/api/chat` `format`+tools underdocumented; risks suppressing reasoning). **Bigger model/quant** — operator path (README), not harness code. **Adopted** (every layer acts on the model's output side): (1) deterministic sampling — `OLLAMA_SEED=42`, `OLLAMA_TOP_K=1` (0 = provider default) collapse the sampling tail that lets a small model wander into prose; (2) text tool-call rescue — a Final reply containing exactly one tool-call-shaped JSON object (`{"tool":…,"arguments":{…}}` / `{"name":…}`) is re-interpreted as the structured call and flows through the SAME classification/dispatch path (fail-closed on 0 or >1 candidates; AGENTS.md #3 intact); (3) success-claim guard — a Final asserting execution with no tool call this order is withheld, a corrective nudge is injected, and the model loops (bounded by `ORDER_BUDGET_S`; on exhaustion the text IS delivered, never silently swallowed, with an ops warn). Patterns drawn from the real false claims; regression-guarded against over-flagging (a "here are the available categories" final passes through). **Preview case (v0.12):** a success claim after calls that only *previewed* a `destructive_when` tool (dispatched without its acting argument; live 2026-10-17: "Expense created successfully" after an `apply`-less propose) gets one nudge naming the acting argument from the policy rule, so a real request ends on a decision. If the model still claims success (live: qwen3:4b re-called without `apply` and said "successfully created and applied"), the harness ends the order on the decision itself: the model's preview arguments plus the policy's acting argument, approve-gated like any other (AGENTS.md #3). The claim match is wider here ("Created expense…" was the second live phrasing, missed by the patterns), and since no pattern list is complete, every answer that ends an order after a preview gets a harness-written "ℹ Not saved yet: this is only a preview…" line, whatever the model wrote. Reads that say "successfully" are untouched. |
| 17 | OCR / attachment ingestion | **Tesseract via a new `mcp-ocr` backend, wired in through a generic attachment-handle mechanism (§4.4)** | Hardware constraint drove the engine choice: qwen3:4b-instruct already occupies ~3.5–4.5 GB of a 6 GB VRAM budget, leaving no room for a co-resident vision model; Tesseract runs on CPU at ~0 VRAM with sub-second latency on printed text (bills, small documents), so it is the v1 default. Rejected for v1: an Ollama-vision backend — kept possible behind the same `OcrEngine` trait seam (mirrors `CommandRunner`/`SystemCtl` in `mcp-linux`) but not built, since it would compete with the tool-calling model for the same GPU. The harder problem was getting image bytes to a tool at all: `Order.attachments` was validated and then dropped, never shown to the model or any backend, since the model authors tool arguments itself and cannot emit a base64 blob. Adopted: the harness persists inbound image attachments to a shared ephemeral scratch volume, mints a random handle, and injects one generic manifest message naming the handle and mime type — **never a tool name** (AGENTS.md #1) — so any attachment-consuming backend wires in purely via `MCP_SERVERS` config. `mcp-ocr`'s `extract_text` tool resolves handle → file on its own mount of the same volume, guarded against path traversal (handle charset `[A-Za-z0-9_-]` only). Scratch, not durable state (AGENTS.md #7): handles live for one order, deleted on every return path, and the whole directory is purged at boot. Out of scope for v1, tracked as follow-ups: PDF bills (needs `pdftoppm` rasterization before OCR), `ref`-form attachment upload (`POST /v1/files`), the Ollama-vision engine, and cross-order attachment reuse. |
| 20 | Wrong or slow numeric answers from a 4B model | **Short answers + deterministic number hygiene on the output side: `OLLAMA_NUM_PREDICT` cap and a ≤6-line instruction; backend `checks` relayed verbatim (§4.4 convention); OCR misread checks by confusable-digit repair; an ungrounded-number note** | Live: the model copied faded digits faithfully (`Q30.46` while its own line items summed to 80.46 and the amount in words said *Ochenta*; meter 5114 → 5104 with 80 kWh used) and in earlier runs invented totals (Q38.01, Q230.48); answers took ~2 min. qwen3:4b neither cross-checks nor computes reliably, so arithmetic moved into code: `mcp-ocr` searches for single confusable-digit repairs that satisfy `a + b`, `b − a` or a words amount — generic over document types (no field names), limited by three-distinct-lines partners, a ±3-line "already explained" window, a unique-repair rule and letter-glued integers being codes; on the sample bill it yields exactly 30.46→80.46 and 5104→5194. The harness appends any tool's `checks` and names answer numbers absent from tool results/user messages (sums and differences of two sources count as grounded; integers <10 ignored) — a note, never a re-prompt, so no added latency (`NUMBER_GUARD=false` to disable). Rejected: a calculator tool (the model must think to call it; each call ≈ another 20–60 s pass), two-pass structured extraction (slowest; needs per-document rules again), a bigger model (no VRAM headroom on 6 GB, slower), prompt-only cross-checking (unreliable at 4B, longer output). Ollama prompt/output timings are now logged per completion. Prompt wording matters: "offer more detail instead of restating whole documents or tool results" made the model stop calling OCR (0/4 phrasings in a replay against the live model, vs 2/4 for the old wording); "keep final answers to at most 6 short lines … Always call a tool when one is needed to answer" called it 4/4. Follow-up the same day: the live answer still said Q30.46 (the model copies the printed digit) and repeated the relayed checks, and took 99 s — 89 s of it generating at 3.8 tok/s. Measured with a ~9.9k-token prompt: `num_ctx` 32768 → 61% of the model on the GPU, 3.9 tok/s; 16384 → 94%, 19 tok/s; 12288 → 100%, 50 tok/s. Adopted 16384 (12288 left ~2k tokens of history beside the ~9.5k-token tool catalogue), an output reserve in prompt assembly, and a 3-bytes-per-token estimate so a tight window never shifts out the system prompt. Strong OCR repairs are applied in the text (option C — deterministic, plays to the model's strength of copying; rejected: instructing the model to correct, unreliable at 4B), and `checks` are hidden from the model. Writes stay gated: a corrected amount reaches Securo only through a decision the user approves (§4.3). |
| 19 | OCR accuracy on phone photos | **In-process preprocessing (Rust `image`/`imageproc`, no extra system software) + parallel multi-read picked by Tesseract's own word confidence + `[?word]` doubt marks + pinned `tessdata_best` eng/spa** | Measured on a real 720×1280 Telegram photo of a bill. Baseline (original image, psm 3, Debian fast models) misread `Q 10`→`ato`, `0.00`→`co.`, `Consumo 80`→`60`, and lost `Lectura Anterior` entirely. Binarizing (Sauvola) and median filtering were tried and **rejected**: they erased faint digit strokes that grayscale kept. Adopted: upscale to ~2800 px long side (≤3×), 1–99 percentile contrast stretch, projection-profile deskew, white border, then reads of {original/psm 3, cleaned/psm 3, cleaned/psm 6}, keeping the highest Σconf/100 over words ≥30%. With `tessdata_best` the chosen reading gets every field right except digits the paper itself prints faded (the totals' "8" prints as "3"), which Tesseract reads at ~90% confidence — no threshold can flag those, so the words-vs-digits cross-check stays the model's job. Latency ≈5 s per photo (3 single-threaded Tesseract processes). `MCP_OCR_PREPROCESS=off` is the rollback. Noise trimming (follow-up, same day): the live answer still promoted edge debris to a fact ("Service Provider: Seraclo…"); dropping rule fragments and all-doubtful non-numeric lines cut the sample from 44 to 32 lines (1299 → 977 chars, doubtful words 36 → 6) with every bill field intact. Rejected: ImageMagick/OpenCV (extra system software), perspective correction (needs OpenCV; photos are mostly flat). |
| 18 | Human-in-the-loop choices | **Generic `decision` in Contract 1; core owns options, adapters own rendering; model-interpreted confirmation removed (supersedes #14's interpretation half)** | Rejected: keeping plain-text consent judged by the model — with buttons available it only adds a path where a 4B model's guess executes a real write. Rejected: suspending orders while waiting for an answer — breaks "one order in flight" and the MCP timeout; a decision ends the order instead. Adopted: options/details built deterministically from the frozen payload and schema; single live decision per session (supersede); free text never approves; legacy `needs_confirmation`/`confirmation_token` kept (token == decision id) until `/v2`; `CONFIRM_MODE` parsed but ignored for one release. Telegram renders inline keyboards with short callback keys (64-byte `callback_data` limit); CLI renders a numbered menu. Future consumers: `choose` from schema validation, an `ask_user` meta-tool, MCP elicitation as `form` |
| 21 | How a hosted service plugs in | **Plugin = versioned TOML manifest in `PLUGINS_DIR` pointing at an MCP server (remote URL or operator-run container); contributes only tools and text; per-plugin `<id>.env` secrets; `MCP_SERVERS` deprecated** | The flat catalogue (38 tools, ~9.8k prompt tokens) overwhelms qwen3:4b: OCR was 0/3 with it and 3/3 alone, and a 10.5k-token completion took 89 s. The core needed generic metadata (groups, attachment params, permissions) without app knowledge. **Explicit exception to env-only config (AGENTS.md #6)**: a manifest is structured, versioned, per-plugin data that env vars encode badly; unknown versions and keys still fail loudly. Rejected: plugin hooks/code in the core path (attack surface, no need yet); the harness running containers (would need the Docker socket — it generates a compose snippet instead). The `*_ref` suffix convention moved into `tools.attachment_params`. The Linux plugin keeps id `linux-mcp` so tool names and policy keys survive the migration. |
| 22 | Trusting third-party plugins | **Disclosure report + enforced per-plugin trust (`untrusted`: every call needs a decision; `trusted`: classifier) + provenance label; operator promotes; any manifest or tool-schema change demotes** | Classifier verdicts are model-made and can be confidently wrong (#11); for code written by someone else, reads can leak too. Trust is stored in the existing tool policy file (no new store, AGENTS.md #7). Manifests only tighten policy: no field can mark a tool safe. Provenance is a label only, not enforced. Rejected for now: signing (cosign), egress enforcement (declared and disclosed, not sandboxed). Author text is sanitized, capped and fenced as data. |
| 23 | Shrinking what the model sees | **Operator allowlist (always on) → one structured-output router call picking a tool group → sticky group plus `related` groups after the first dispatch; behind `ROUTER_ENABLED`, default off** | Ollama has no `tool_choice` (see the tool-recognition lesson), so the only lever is what the model is offered. Ollama `format` with an enum makes the pick parseable by construction. Rejected: keyword preselection (punishes loose phrasing, #16); embedding retrieval and a tool-search meta-tool (more moving parts; revisit after measuring). Failure mode to watch: a wrong pick hides the right tool; no auto-retry with `all` in v1. Enable only when `arno-replay` shows equal-or-better first-call accuracy with fewer prompt tokens and lower latency. |
| 24 | Adding plugins without a restart | **Swappable registry + reload job on the FIFO queue (boot, SIGHUP); no admin API yet** | The queue already guarantees one order in flight (§8), so a reload queued there lands between orders and an in-flight order keeps its snapshot by construction. A future TUI install flow needs an admin API; it must be separate from Contract 1, use its own token, and never be reachable by adapters. |
| 25 | Small model ignores an attached file | **Read first: when exactly one offered tool takes attachments (one attachment param, no other required arg), the harness calls it before the model's first turn; otherwise one nudge after a plain answer** | `arno-replay` showed OCR depending on the caption even with `ocr.extract_text` as the only tool offered: "What this says?" and an empty caption 0/3, "Please tell me what this says" 3/3 (greedy decoding makes it deterministic). Ollama has no `tool_choice`, so no prompt wording forces the call. Reading first is safe: the call goes through the same policy gate (an untrusted or destructive tool yields a decision), the arguments are only the handle, and it only runs when there is exactly one candidate. Every photo gets read, which is what nearly every photo order wants (~5 s of OCR). Rejected: filling in empty captions (guesses intent, leaves short captions failing); retrying with different sampling (undoes #16). |

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

1. ~~**Audit log**~~ — **Resolved in M3** (§11 decision #12): hash-chained append-only JSONL, `AUDIT_LOG_PATH`. ~~Retention/rotation remains explicitly out of scope (ops concern, not a harness feature).~~ Rotation and verification now implemented as config-driven harness features (§11 decision #15); off-host backup remains an ops concern.
2. ~~**Log hygiene**~~ — **Resolved in v0.8** (§11 decision #13): two components are explicitly allowed to hold tool args/results verbatim — the audit log (executed writes only) and the transcript (the full conversation/tool-call narrative). Every other component, especially `tracing::` output, still may only log codes and tool names, never payloads (AGENTS.md's existing convention, unchanged). ~~Rotation/secret handling for both files remains explicitly out of scope (ops concern, not a harness feature).~~ Rotation + verification now in the harness (§11 decision #15); off-host copy/secret handling remains ops.
3. **Contract tests + mock MCP server** — golden tests for the Harness API and a stub backend exercising discovery/namespacing/timeouts; health/readiness endpoints surfaced as Docker healthchecks (§11.6).
4. **Prompt-injection stance** — record the accepted-risk rationale (read-only v1 + frozen-payload gate) and revisit now that attachments/OCR have landed (§11 decision #17): recognized text from an image flows into the model's context exactly like any other tool result, so a bill or document crafted to contain instruction-like text is a theoretical injection vector, mitigated but not eliminated by `extract_text` being read-only and the frozen-payload gate still standing between any resulting tool call and a destructive write.
5. **M3 dry-run** — the first confirmed write targets a sandbox, never production books. Since M2 confirmed workspace scoping lives entirely in the bearer JWT's `ws_id` claim (no separate `SECURO_WORKSPACE_ID` var), the sandbox boundary for M3 must be a *separate token* minted against a test workspace — not a config var the harness plumbs through.
6. **Correlation ID** — Response carries no `order_id`; required once SSE/WS push or async orders exist (ties to §11.3).
7. ~~**Securo `propose_*` tools are mutating, not read-only**~~ — **Resolved in M3** (§11 decision #11): the model classifies each tool at discovery from its schema alone, deriving the `apply`-argument condition itself rather than the harness hardcoding it. All 9 `propose_*` tools now correctly gate on `apply: true`; preview calls (no `apply`) dispatch freely, matching Securo's own preview/apply contract exactly instead of either double-confirming previews or leaving applies unguarded.
8. **Runtime client registration** — today every adapter's token is static config: harness reads the whole map from `HARNESS_API_CLIENT_TOKENS`, each adapter carries its own copy of just its own secret (§5.7, §7 layer 2). An alternative is registering clients against a *running* harness (admin action: supply `client_id` + token, paste the same token into the adapter). Deferred, not rejected — it does not remove the two-places-one-secret duplication (the adapter still receives the secret out of band), and it costs three things this design currently avoids: a **persisted** token store (violates §4.1's in-memory-only rule — otherwise every restart de-registers every adapter), an **authenticated admin surface** to mint tokens (which needs its own bootstrap credential from env, so the env var comes back), and a **revocation/rotation** story. Worth building when adapters become dynamic — third-party clients, per-device tokens, self-service onboarding, or rotation without downtime — at which point it is a designed feature (admin auth, persistence, TTL, revocation), not a config tweak.
