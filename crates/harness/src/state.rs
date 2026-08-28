//! Shared application state handed to the API router and queue worker.

use crate::audit::AuditLog;
use crate::config::Config;
use crate::model::ModelProvider;
use crate::orchestrator::ToolExecutor;
use crate::policy::ToolPolicy;
use crate::queue::OrderJob;
use crate::stores::{dedup::DedupStore, pending::PendingStore, sessions::SessionStore};
use std::sync::Arc;

pub struct AppState {
    pub cfg: Config,
    pub sessions: SessionStore,
    pub dedup: DedupStore,
    pub pending: PendingStore,
    pub provider: std::sync::Arc<dyn ModelProvider>,
    pub executor: std::sync::Arc<dyn ToolExecutor>,
    pub order_tx: tokio::sync::mpsc::Sender<OrderJob>,
    /// Model-classified tool-safety rules (SPEC §4.2, M3) — an `Arc` since the
    /// background retry task (`main.rs`) holds its own handle alongside the
    /// orchestrator's.
    pub policy: Arc<ToolPolicy>,
    /// Persistent audit trail of executed frozen actions (SPEC §12.1, M3).
    /// Always present in production (`main.rs` opens it unconditionally);
    /// `None` only in tests that don't exercise the redemption path.
    pub audit: Option<AuditLog>,
}

pub type SharedState = std::sync::Arc<AppState>;
