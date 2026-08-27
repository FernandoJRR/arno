//! Global FIFO order queue — the concurrency backbone (SPEC §8).
//!
//! One `mpsc` channel with a single consumer task. All session access and
//! confirmation redemption happens on that consumer, which is what makes them
//! race-free by construction. Do not add concurrent execution paths here.

use crate::api::error::ApiError;
use crate::orchestrator;
use crate::state::SharedState;
use contract::{Order, Response};

pub struct OrderJob {
    pub client_id: String,
    pub order: Order,
    pub reply: tokio::sync::oneshot::Sender<Outcome>,
}

pub enum Outcome {
    Ok(Response),
    Err(ApiError),
}

pub async fn spawn_worker(state: SharedState, mut rx: tokio::sync::mpsc::Receiver<OrderJob>) {
    while let Some(job) = rx.recv().await {
        let result = orchestrator::handle(&state, &job.client_id, job.order).await;
        let outcome = match result {
            Ok(response) => Outcome::Ok(response),
            Err(err) => {
                tracing::info!(code = err.0.as_str(), "order rejected");
                Outcome::Err(err)
            }
        };
        if job.reply.send(outcome).is_err() {
            tracing::debug!("client abandoned order before reply");
        }
    }
}
