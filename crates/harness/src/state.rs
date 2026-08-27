//! Shared application state handed to the API router and queue worker.

use crate::config::Config;
use crate::model::ModelProvider;
use crate::orchestrator::ToolExecutor;
use crate::queue::OrderJob;
use crate::stores::{dedup::DedupStore, pending::PendingStore, sessions::SessionStore};

pub struct AppState {
    pub cfg: Config,
    pub sessions: SessionStore,
    pub dedup: DedupStore,
    pub pending: PendingStore,
    pub provider: std::sync::Arc<dyn ModelProvider>,
    pub executor: std::sync::Arc<dyn ToolExecutor>,
    pub order_tx: tokio::sync::mpsc::Sender<OrderJob>,
}

pub type SharedState = std::sync::Arc<AppState>;
