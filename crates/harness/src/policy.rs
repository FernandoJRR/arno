//! Adaptive tool-safety classification (SPEC §4.2/§11, M3).
//!
//! The harness must decide, for any MCP tool from any backend, whether a call
//! to it can change external state. The only two mechanisms that were
//! considered and rejected are recorded in SPEC §11: an exact-name env list
//! (`DESTRUCTIVE_TOOLS`) couples deployment config to one backend's evolving
//! tool list and drifts open; standard MCP tool annotations would be ideal but
//! no backend is obliged to send them (Securo sends none).
//!
//! Instead: the one thing every MCP tool guarantees is a name, description,
//! and input schema — exactly what an LLM reads well. At discovery, the model
//! is asked to classify each unknown/changed tool into a mechanical
//! [`ToolRule`], which is persisted and evaluated by plain Rust at dispatch
//! time (AGENTS.md #3 — the model never executes confirmed actions, and here
//! it never even executes the *decision* to freeze; it only ever authors a
//! rule ahead of time, ahead of any specific order).
//!
//! Fail-closed throughout: an unclassified, unparseable, or drifted tool is
//! `Destructive` until a verdict says otherwise. This is a deliberate,
//! narrowly-scoped exception to AGENTS.md #7 ("in-memory stores stay
//! in-memory"): the policy file is the harness's memory of what backends have
//! declared, and losing it on restart would silently re-open every tool to
//! read-only trust assumptions rather than fail closed, so it persists.

use crate::model::{ChatMessage, CompletionOutput, CompletionRequest, ModelProvider, Role};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// A mechanical, model-authored safety rule for one tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolRule {
    Safe,
    Destructive,
    /// Destructive only when `args[key] == value` — the generic form of
    /// Securo's `apply=true` convention, derived by the model from the
    /// tool's own schema. The harness never hardcodes an argument name.
    DestructiveWhen {
        key: String,
        value: serde_json::Value,
    },
}

impl ToolRule {
    fn is_destructive_for(&self, args: &serde_json::Value) -> bool {
        match self {
            ToolRule::Safe => false,
            ToolRule::Destructive => true,
            ToolRule::DestructiveWhen { key, value } => args.get(key) == Some(value),
        }
    }
}

/// Where a persisted rule came from. Env overrides never appear here — they
/// live outside the file entirely (see [`ToolPolicy::env_overrides`]) and
/// always win, checked first.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    /// A human hand-edited this entry in the policy file. Sticky: automatic
    /// reconciliation never overwrites it, even if the tool's schema drifts —
    /// drift against an operator entry is logged loudly instead.
    Operator,
    /// The model classified this tool; safe to redo automatically once its
    /// schema fingerprint changes.
    Model,
    /// Newly discovered or drifted; not yet classified. Evaluates as
    /// `Destructive` (fail-closed) until the retry task resolves it.
    Pending,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEntry {
    pub rule: ToolRule,
    pub source: RuleSource,
    /// sha256 hex of the tool's discovered schema JSON (name+description+
    /// parameters, i.e. exactly `mcp::schema_value`'s output). A change here
    /// means the backend changed the tool's contract — back to `Pending`.
    pub fingerprint: String,
    /// The model's one-line justification, kept for human review of the file.
    #[serde(default)]
    pub reason: String,
    pub classified_at_ms: u64,
}

/// On-disk shape (SPEC §5.7 `TOOL_POLICY_PATH`) — plain, human-editable JSON.
/// `_note` is written on every save purely so a human opening the file finds
/// the hand-editing convention in the file itself, not just in docs.
#[derive(Debug, Serialize, Deserialize)]
struct PolicyFile {
    #[serde(rename = "_note")]
    note: String,
    tools: HashMap<String, ToolEntry>,
}

const NOTE: &str = "Hand-editable. Set \"source\":\"operator\" on an entry to pin it \
— automatic reconciliation will never overwrite an operator entry, even if the \
tool's schema changes later (a drift warning is logged instead). Entries with \
\"source\":\"model\" or \"pending\" are safe to delete; they regenerate at boot.";

/// Persisted, model-classified tool safety rules, plus the env-level override
/// set that always wins (SPEC §5.7 `DESTRUCTIVE_TOOLS` — unchanged semantics,
/// now the operator's highest-precedence escape hatch rather than the primary
/// mechanism).
pub struct ToolPolicy {
    env_overrides: HashSet<String>,
    entries: RwLock<HashMap<String, ToolEntry>>,
    path: PathBuf,
}

impl ToolPolicy {
    /// Loads `path` if present; a missing file is empty (first boot), and a
    /// corrupt/unreadable file is logged loudly and treated as empty rather
    /// than blocking boot — every tool simply starts `Pending` (fail-closed)
    /// and gets reclassified.
    pub fn load(path: PathBuf, env_overrides: HashSet<String>) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<PolicyFile>(&text) {
                Ok(file) => file.tools,
                Err(e) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "tool policy file corrupt — starting empty (every tool pending, fail-closed)"
                    );
                    HashMap::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "tool policy file unreadable — starting empty (every tool pending, fail-closed)"
                );
                HashMap::new()
            }
        };
        Self {
            env_overrides,
            entries: RwLock::new(entries),
            path,
        }
    }

    /// Best-effort atomic save (write-then-rename): a crash mid-save leaves
    /// the previous, still-valid file in place rather than a torn write.
    fn save(&self) {
        let entries = self.entries.read().expect("policy lock").clone();
        let file = PolicyFile {
            note: NOTE.to_owned(),
            tools: entries,
        };
        let body = match serde_json::to_string_pretty(&file) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!(error = %e, "tool policy serialize failed — not persisted this round");
                return;
            }
        };
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, &body) {
            tracing::error!(path = %tmp.display(), error = %e, "tool policy write failed");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            tracing::error!(path = %self.path.display(), error = %e, "tool policy rename failed");
        }
    }

    /// The only runtime dispatch check (SPEC §4.2 classification). Env
    /// override wins outright; otherwise an unclassified tool is
    /// `Destructive` by construction (`entries.get` returning `None` falls
    /// through to the same fail-closed default as an explicit `Pending`).
    pub fn is_destructive(&self, tool: &str, args: &serde_json::Value) -> bool {
        if self.env_overrides.contains(tool) {
            return true;
        }
        match self.entries.read().expect("policy lock").get(tool) {
            Some(entry) => entry.rule.is_destructive_for(args),
            None => true,
        }
    }

    /// Syncs against freshly discovered tool schemas (SPEC §4.2): inserts
    /// never-seen tools as `Pending`; flips `Model`-sourced entries back to
    /// `Pending` when their fingerprint no longer matches (the backend
    /// changed the tool's contract — drift detection, "for free"); logs but
    /// never touches a drifted `Operator` entry (human authority wins,
    /// visibly). Persists immediately so a crash right after doesn't lose the
    /// drift signal. Returns the tool names now `Pending` and needing
    /// classification.
    pub fn reconcile(&self, schemas: &[serde_json::Value]) -> Vec<String> {
        let mut pending = Vec::new();
        {
            let mut entries = self.entries.write().expect("policy lock");
            for schema in schemas {
                let Some(name) = tool_name(schema) else {
                    continue;
                };
                let fingerprint = fingerprint_of(schema);
                match entries.get_mut(name) {
                    None => {
                        entries.insert(
                            name.to_owned(),
                            ToolEntry {
                                rule: ToolRule::Destructive,
                                source: RuleSource::Pending,
                                fingerprint,
                                reason: String::new(),
                                classified_at_ms: now_ms(),
                            },
                        );
                        pending.push(name.to_owned());
                    }
                    Some(entry) if entry.fingerprint == fingerprint => {
                        // Unchanged. A prior Pending entry still needs classifying.
                        if entry.source == RuleSource::Pending {
                            pending.push(name.to_owned());
                        }
                    }
                    Some(entry) if entry.source == RuleSource::Operator => {
                        tracing::warn!(
                            tool = name,
                            "tool schema changed but an operator-pinned policy entry \
                             is never auto-overwritten — review manually"
                        );
                    }
                    Some(entry) => {
                        tracing::info!(
                            tool = name,
                            old_fingerprint = %entry.fingerprint,
                            "tool schema changed — re-classifying (fail-closed until resolved)"
                        );
                        entry.rule = ToolRule::Destructive;
                        entry.source = RuleSource::Pending;
                        entry.fingerprint = fingerprint;
                        entry.reason.clear();
                        pending.push(name.to_owned());
                    }
                }
            }
        }
        self.save();
        pending
    }

    /// Classifies exactly `names` (expected: a subset of the tools `reconcile`
    /// returned) against their schemas, one model call per tool — small
    /// models route single, focused questions far more reliably than a
    /// batched one. Persists each resolution as it lands, so a mid-run
    /// failure doesn't lose earlier progress. Returns `(resolved, still_pending)`.
    pub async fn classify_pending(
        &self,
        provider: &dyn ModelProvider,
        schemas: &[serde_json::Value],
        names: &[String],
    ) -> (usize, usize) {
        let mut resolved = 0;
        let mut still_pending = 0;
        for name in names {
            let Some(schema) = schemas.iter().find(|s| tool_name(s) == Some(name.as_str())) else {
                continue;
            };
            match classify_one(provider, schema).await {
                Some((rule, reason)) => {
                    let fingerprint = fingerprint_of(schema);
                    let mut entries = self.entries.write().expect("policy lock");
                    entries.insert(
                        name.clone(),
                        ToolEntry {
                            rule,
                            source: RuleSource::Model,
                            fingerprint,
                            reason,
                            classified_at_ms: now_ms(),
                        },
                    );
                    drop(entries);
                    resolved += 1;
                }
                None => {
                    tracing::debug!(tool = %name, "tool classification still pending");
                    still_pending += 1;
                }
            }
        }
        if resolved > 0 {
            self.save();
        }
        (resolved, still_pending)
    }

    /// Tool names currently `Pending` (used by the boot log and the retry task).
    pub fn pending_tools(&self) -> Vec<String> {
        self.entries
            .read()
            .expect("policy lock")
            .iter()
            .filter(|(_, e)| e.source == RuleSource::Pending)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// `(safe, destructive, conditional, pending)` counts, for the boot summary log.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let entries = self.entries.read().expect("policy lock");
        let mut safe = 0;
        let mut destructive = 0;
        let mut conditional = 0;
        let mut pending = 0;
        for entry in entries.values() {
            if entry.source == RuleSource::Pending {
                pending += 1;
                continue;
            }
            match entry.rule {
                ToolRule::Safe => safe += 1,
                ToolRule::Destructive => destructive += 1,
                ToolRule::DestructiveWhen { .. } => conditional += 1,
            }
        }
        (safe, destructive, conditional, pending)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn tool_name(schema: &serde_json::Value) -> Option<&str> {
    schema["function"]["name"].as_str()
}

/// Fingerprints exactly the fields `mcp::schema_value` builds (name,
/// description, parameters) — the tool's full observable contract. Stable
/// because this workspace never enables serde_json's `preserve_order`
/// feature, so object keys serialize in consistent (B-Tree) order.
fn fingerprint_of(schema: &serde_json::Value) -> String {
    let body = serde_json::to_string(&schema["function"]).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(body.as_bytes());
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

const CLASSIFY_SYSTEM_PROMPT: &str = "You are a safety classifier for tool-calling. \
You will be shown one MCP tool's name, description, and JSON input schema. Decide \
whether calling it can create, modify, delete, or otherwise change external state \
(a write), or whether it only reads/queries state (safe). Some tools are two-mode: \
safe by default but one specific argument switches them to actually executing a \
change (for example an `apply` or `confirm` boolean that defaults to false and is \
documented as making the change real when true).\n\n\
Respond with EXACTLY one line of JSON and nothing else — no markdown fences, no \
prose before or after:\n\
{\"class\":\"safe|destructive|conditional\",\"key\":<argument name or null>,\"equals\":<value or null>,\"reason\":\"<one short sentence>\"}\n\n\
Rules: class=\"safe\" only if the tool cannot change state under any arguments. \
class=\"destructive\" if it always changes state. class=\"conditional\" only if you \
can name the exact argument name and value that switches it from a harmless preview \
to a real change — set key/equals to exactly that. If uncertain, answer \"destructive\" \
— never guess \"safe\". Destructive tools can be described as \'update\' or \'change\' \
for example: \'update_transaction\' or \'change_ticket\'";

async fn classify_one(
    provider: &dyn ModelProvider,
    schema: &serde_json::Value,
) -> Option<(ToolRule, String)> {
    let name = tool_name(schema)?;
    let description = schema["function"]["description"].as_str().unwrap_or("");
    let parameters = &schema["function"]["parameters"];
    let user_msg =
        format!("Tool name: {name}\nDescription: {description}\nInput schema: {parameters}");

    let req = CompletionRequest {
        messages: vec![
            ChatMessage::new(Role::System, CLASSIFY_SYSTEM_PROMPT),
            ChatMessage::new(Role::User, user_msg),
        ],
        tools: Vec::new(),
        context_tokens: 4096,
    };

    let text = match provider.complete(req).await {
        Ok(CompletionOutput::Final(text)) => text,
        Ok(CompletionOutput::ToolCalls(_)) | Err(_) => return None,
    };
    parse_verdict(&text)
}

/// Lenient extraction (tolerates prose/code fences around the JSON object),
/// strict validation (anything that doesn't cleanly fit the expected shape
/// yields `None` — a guess is never substituted for a verdict).
fn parse_verdict(text: &str) -> Option<(ToolRule, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let reason = value
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let rule = match value.get("class").and_then(serde_json::Value::as_str)? {
        "safe" => ToolRule::Safe,
        "destructive" => ToolRule::Destructive,
        "conditional" => {
            let key = value.get("key")?.as_str()?.to_owned();
            let equals = value.get("equals")?.clone();
            if equals.is_null() {
                return None;
            }
            ToolRule::DestructiveWhen { key, value: equals }
        }
        _ => return None,
    };
    Some((rule, reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str, description: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": description,
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    struct FixedProvider(&'static str);
    #[async_trait::async_trait]
    impl ModelProvider for FixedProvider {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionOutput, crate::model::ModelError> {
            Ok(CompletionOutput::Final(self.0.to_owned()))
        }
    }

    struct FailingProvider;
    #[async_trait::async_trait]
    impl ModelProvider for FailingProvider {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionOutput, crate::model::ModelError> {
            Err(crate::model::ModelError::Timeout)
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "arno-policy-test-{tag}-{}-{}.json",
            std::process::id(),
            now_ms()
        ))
    }

    #[test]
    fn never_classified_tool_is_destructive_fail_closed() {
        let policy = ToolPolicy::load(tmp_path("unclassified"), HashSet::new());
        assert!(policy.is_destructive("mock.anything", &serde_json::json!({})));
    }

    #[test]
    fn env_override_always_wins_regardless_of_file() {
        let path = tmp_path("env-override");
        let policy = ToolPolicy::load(path, HashSet::from(["mock.safe".to_owned()]));
        policy.reconcile(&[schema("mock.safe", "reads only")]);
        // Even once classified safe, an env override still forces destructive.
        {
            let mut entries = policy.entries.write().unwrap();
            entries.get_mut("mock.safe").unwrap().rule = ToolRule::Safe;
            entries.get_mut("mock.safe").unwrap().source = RuleSource::Model;
        }
        assert!(policy.is_destructive("mock.safe", &serde_json::json!({})));
    }

    #[test]
    fn destructive_when_matches_only_the_configured_arg_value() {
        let rule = ToolRule::DestructiveWhen {
            key: "apply".into(),
            value: serde_json::json!(true),
        };
        assert!(!rule.is_destructive_for(&serde_json::json!({})));
        assert!(!rule.is_destructive_for(&serde_json::json!({"apply": false})));
        assert!(rule.is_destructive_for(&serde_json::json!({"apply": true})));
    }

    #[test]
    fn reconcile_marks_new_tools_pending_and_persists() {
        let path = tmp_path("reconcile-new");
        let policy = ToolPolicy::load(path.clone(), HashSet::new());
        let pending = policy.reconcile(&[schema("mock.fresh", "does a thing")]);
        assert_eq!(pending, vec!["mock.fresh".to_owned()]);
        assert!(policy.is_destructive("mock.fresh", &serde_json::json!({})));

        // Persisted: reloading from disk sees the same pending entry.
        let reloaded = ToolPolicy::load(path, HashSet::new());
        assert!(reloaded.is_destructive("mock.fresh", &serde_json::json!({})));
    }

    #[test]
    fn fingerprint_change_reclassifies_model_entries_but_not_operator_entries() {
        let policy = ToolPolicy::load(tmp_path("drift"), HashSet::new());
        policy.reconcile(&[schema("mock.a", "v1 description")]);
        {
            let mut entries = policy.entries.write().unwrap();
            let e = entries.get_mut("mock.a").unwrap();
            e.rule = ToolRule::Safe;
            e.source = RuleSource::Model;
        }
        let pending = policy.reconcile(&[schema("mock.a", "v2 description — changed")]);
        assert_eq!(
            pending,
            vec!["mock.a".to_owned()],
            "model entry re-pends on drift"
        );
        assert!(policy.is_destructive("mock.a", &serde_json::json!({})));

        // Now pin it as an operator entry and drift again — must NOT re-pend.
        {
            let mut entries = policy.entries.write().unwrap();
            let e = entries.get_mut("mock.a").unwrap();
            e.rule = ToolRule::Safe;
            e.source = RuleSource::Operator;
            e.fingerprint = fingerprint_of(&schema("mock.a", "v2 description — changed"));
        }
        let pending = policy.reconcile(&[schema("mock.a", "v3 — drifted again")]);
        assert!(
            pending.is_empty(),
            "operator entries are never auto-reclassified"
        );
        assert!(!policy.is_destructive("mock.a", &serde_json::json!({})));
    }

    #[test]
    fn corrupt_policy_file_is_not_fatal_and_starts_empty() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "{ not valid json").unwrap();
        let policy = ToolPolicy::load(path, HashSet::new());
        assert!(policy.is_destructive("anything.at_all", &serde_json::json!({})));
    }

    #[tokio::test]
    async fn classify_pending_parses_clean_json_verdict() {
        let policy = ToolPolicy::load(tmp_path("classify-clean"), HashSet::new());
        let schemas = [schema("mock.read", "lists things, no side effects")];
        let pending = policy.reconcile(&schemas);
        let provider = FixedProvider(
            r#"{"class":"safe","key":null,"equals":null,"reason":"read-only listing"}"#,
        );
        let (resolved, still_pending) =
            policy.classify_pending(&provider, &schemas, &pending).await;
        assert_eq!((resolved, still_pending), (1, 0));
        assert!(!policy.is_destructive("mock.read", &serde_json::json!({})));
    }

    #[tokio::test]
    async fn classify_pending_tolerates_code_fences_around_json() {
        let policy = ToolPolicy::load(tmp_path("classify-fenced"), HashSet::new());
        let schemas = [schema(
            "mock.write",
            "creates a record; apply=true persists it",
        )];
        let pending = policy.reconcile(&schemas);
        let provider = FixedProvider(
            "```json\n{\"class\":\"conditional\",\"key\":\"apply\",\"equals\":true,\"reason\":\"apply=true persists\"}\n```",
        );
        let (resolved, _) = policy.classify_pending(&provider, &schemas, &pending).await;
        assert_eq!(resolved, 1);
        assert!(policy.is_destructive("mock.write", &serde_json::json!({"apply": true})));
        assert!(!policy.is_destructive("mock.write", &serde_json::json!({"apply": false})));
    }

    #[tokio::test]
    async fn classify_pending_leaves_malformed_or_failed_verdicts_pending() {
        let policy = ToolPolicy::load(tmp_path("classify-malformed"), HashSet::new());
        let schemas = [schema("mock.unclear", "ambiguous tool")];
        let pending = policy.reconcile(&schemas);

        let bad_json = FixedProvider("I cannot decide.");
        let (resolved, still_pending) =
            policy.classify_pending(&bad_json, &schemas, &pending).await;
        assert_eq!((resolved, still_pending), (0, 1));
        assert!(policy.is_destructive("mock.unclear", &serde_json::json!({})));

        let failing = FailingProvider;
        let (resolved, still_pending) = policy.classify_pending(&failing, &schemas, &pending).await;
        assert_eq!((resolved, still_pending), (0, 1));
    }

    #[tokio::test]
    async fn retry_after_provider_recovers_resolves_previously_pending_tool() {
        let path = tmp_path("retry-recovers");
        let policy = ToolPolicy::load(path, HashSet::new());
        let schemas = [schema("mock.flaky", "reads a report")];
        let pending = policy.reconcile(&schemas);

        // First attempt: provider down — tool stays pending (destructive).
        let down = FailingProvider;
        let (resolved, still_pending) = policy.classify_pending(&down, &schemas, &pending).await;
        assert_eq!((resolved, still_pending), (0, 1));
        assert!(policy.is_destructive("mock.flaky", &serde_json::json!({})));

        // Retry task wakes later, provider is back: the same pending set resolves.
        let still = policy.pending_tools();
        assert_eq!(still, vec!["mock.flaky".to_owned()]);
        let up = FixedProvider(r#"{"class":"safe","key":null,"equals":null,"reason":"read-only"}"#);
        let (resolved, still_pending) = policy.classify_pending(&up, &schemas, &still).await;
        assert_eq!((resolved, still_pending), (1, 0));
        assert!(!policy.is_destructive("mock.flaky", &serde_json::json!({})));
        assert!(policy.pending_tools().is_empty());
    }

    #[test]
    fn parse_verdict_rejects_conditional_missing_key_or_equals() {
        assert!(parse_verdict(r#"{"class":"conditional","reason":"x"}"#).is_none());
        assert!(
            parse_verdict(r#"{"class":"conditional","key":"apply","equals":null,"reason":"x"}"#)
                .is_none()
        );
        assert!(parse_verdict(r#"{"class":"nonsense"}"#).is_none());
    }
}
