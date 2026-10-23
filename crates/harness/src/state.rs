//! Shared application state handed to the API router and queue worker.

use crate::audit::AuditLog;
use crate::config::Config;
use crate::model::ModelProvider;
use crate::plugins::PluginRegistry;
use crate::policy::ToolPolicy;
use crate::queue::Job;
use crate::stores::{dedup::DedupStore, pending::PendingStore, sessions::SessionStore};
use crate::transcript::TranscriptLog;
use std::sync::Arc;

pub struct AppState {
    pub cfg: Config,
    pub sessions: SessionStore,
    pub dedup: DedupStore,
    pub pending: PendingStore,
    pub provider: std::sync::Arc<dyn ModelProvider>,
    /// Loaded plugins; each order works on one snapshot (SPEC §11 #24).
    pub plugins: Arc<PluginRegistry>,
    pub order_tx: tokio::sync::mpsc::Sender<Job>,
    /// Model-classified tool-safety rules (SPEC §4.2, M3) — an `Arc` since the
    /// background retry task (`main.rs`) holds its own handle alongside the
    /// orchestrator's.
    pub policy: Arc<ToolPolicy>,
    /// Persistent audit trail of executed frozen actions (SPEC §12.1, M3).
    /// Always present in production (`main.rs` opens it unconditionally);
    /// `None` only in tests that don't exercise the redemption path.
    pub audit: Option<AuditLog>,
    /// Persistent conversation/tool-call transcript (SPEC §12.2). Always
    /// present in production; `None` only in tests that don't exercise it.
    pub transcript: Option<TranscriptLog>,
    /// The last order's model call, for `/v1/ready` (SPEC §11 #31).
    pub model_health: ModelHealth,
}

/// In-memory, like every other store (AGENTS.md #7).
#[derive(Default)]
pub struct ModelHealth(std::sync::Mutex<LastCall>);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LastCall {
    pub elapsed_ms: Option<u64>,
    pub timed_out: bool,
}

impl ModelHealth {
    pub fn record(&self, call: LastCall) {
        *self.0.lock().expect("model health lock") = call;
    }

    pub fn last(&self) -> LastCall {
        *self.0.lock().expect("model health lock")
    }
}

pub type SharedState = std::sync::Arc<AppState>;
