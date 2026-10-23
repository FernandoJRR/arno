//! Global FIFO order queue — the concurrency backbone (SPEC §8).
//!
//! One `mpsc` channel with a single consumer task. All session access and
//! confirmation redemption happens on that consumer, which is what makes them
//! race-free by construction. Do not add concurrent execution paths here.
//! Plugin reloads ride the same queue, so they always land between orders
//! (SPEC §11 #24).

use crate::api::error::ApiError;
use crate::orchestrator;
use crate::state::SharedState;
use contract::{Order, Response};

pub enum Job {
    Order(Box<OrderJob>),
    /// Re-read `PLUGINS_DIR` and swap the plugin set.
    Reload(Option<tokio::sync::oneshot::Sender<Result<(), String>>>),
}

pub struct OrderJob {
    pub client_id: String,
    pub order: Order,
    pub reply: tokio::sync::oneshot::Sender<Outcome>,
}

pub enum Outcome {
    Ok(Box<Response>),
    Err(ApiError),
}

pub async fn spawn_worker(state: SharedState, mut rx: tokio::sync::mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        match job {
            Job::Order(job) => run_order(&state, *job).await,
            Job::Reload(reply) => {
                let result = crate::plugins::reload(&state)
                    .await
                    .map_err(|e| e.to_string());
                match &result {
                    Ok(()) => tracing::info!("plugins reloaded"),
                    Err(e) => {
                        tracing::error!(error = %e, "plugin reload failed; keeping current set")
                    }
                }
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
        }
    }
}

async fn run_order(state: &SharedState, job: OrderJob) {
    use tracing::Instrument;
    // SPEC §11 #32: steps published while the order runs carry its id.
    let order = crate::events::OrderRef {
        order_id: crate::events::new_order_id(),
        client_id: job.client_id.clone(),
        session_id: job.order.session_id.clone(),
        client_msg_id: job.order.client_msg_id.clone(),
    };
    let span = tracing::info_span!("order", order_id = %order.order_id);
    let result = crate::events::scope(
        order.clone(),
        orchestrator::handle(state, &job.client_id, job.order),
    )
    .instrument(span)
    .await;
    let duplicate = matches!(&result, Err(e) if e.0 == contract::ErrorCode::DuplicateOrder);
    if !duplicate {
        crate::events::publish(&state.events, &order, contract::EventKind::Done);
    }
    let outcome = match result {
        Ok(mut response) => {
            response.order_id = Some(order.order_id);
            Outcome::Ok(Box::new(response))
        }
        Err(err) => {
            tracing::info!(code = err.0.as_str(), "order rejected");
            Outcome::Err(err)
        }
    };
    if job.reply.send(outcome).is_err() {
        tracing::debug!("client abandoned order before reply");
    }
}
