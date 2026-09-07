//! `/v1` routes (SPEC §4.1). New routes: add here + wiremock contract test.

use crate::api::auth::AuthedClient;
use crate::api::error::ApiError;
use crate::queue::{OrderJob, Outcome};
use crate::state::SharedState;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use contract::{ErrorCode, Order};

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/orders", post(orders))
        .route("/v1/logs/verify", get(logs_verify))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Full-chain verification of the audit log and transcript (SPEC §11 decision
/// #15). Bearer-authed like every other route; returns counts and chain heads
/// only — never payloads (AGENTS.md logging convention). 200 with
/// `"ok": true` when every segment of both chains replays clean.
async fn logs_verify(
    _auth: AuthedClient,
    State(state): State<SharedState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut report = serde_json::Map::new();
    for (name, path) in [
        ("audit", &state.cfg.audit_log_path),
        ("transcript", &state.cfg.transcript_log_path),
    ] {
        let mut segments = crate::chains::segments_for(path);
        if path.exists() {
            segments.push(path.clone());
        }
        match crate::chains::replay_chain(&segments) {
            Ok(head) => {
                report.insert(
                    name.to_owned(),
                    serde_json::json!({
                        "ok": true,
                        "segments": segments.len(),
                        "entries": head.seq,
                        "head": head.hash,
                    }),
                );
            }
            Err(e) => {
                report.insert(
                    name.to_owned(),
                    serde_json::json!({ "ok": false, "error": e.to_string() }),
                );
            }
        }
    }
    let all_ok = report
        .values()
        .all(|v| v["ok"] == serde_json::Value::Bool(true));
    report.insert("ok".to_owned(), serde_json::Value::Bool(all_ok));
    Ok(Json(serde_json::Value::Object(report)))
}

/// Body extractor that keeps the error contract: malformed JSON becomes
/// `invalid_order`, never an axum default rejection body.
struct OrderJson(Order);

impl<S: Send + Sync> axum::extract::FromRequest<S> for OrderJson {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<Order>::from_request(req, state).await {
            Ok(Json(order)) => Ok(Self(order)),
            Err(_) => Err(ApiError(ErrorCode::InvalidOrder)),
        }
    }
}

async fn orders(
    auth: AuthedClient,
    State(state): State<SharedState>,
    OrderJson(order): OrderJson,
) -> Result<axum::response::Response, ApiError> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    state
        .order_tx
        .send(OrderJob {
            client_id: auth.0,
            order,
            reply: reply_tx,
        })
        .await
        .map_err(|_| ApiError(ErrorCode::BackendUnavailable))?; // worker gone = shutting down

    match reply_rx.await {
        Ok(Outcome::Ok(response)) => Ok((StatusCode::OK, Json(response)).into_response()),
        Ok(Outcome::Err(err)) => Err(err),
        Err(_recv_dropped) => Err(ApiError(ErrorCode::BackendUnavailable)),
    }
}
