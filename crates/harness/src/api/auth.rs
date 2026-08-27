//! Layer-2 auth: harness authenticates *adapters*, never end-users (SPEC §7).
//!
//! Constant-time token comparison via `subtle` across all configured client
//! tokens; candidate count is public config so iterating all of them leaks
//! nothing beyond token length equality (an accepted, documented tradeoff).

use crate::api::error::ApiError;
use crate::state::SharedState;
use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use contract::ErrorCode;
use subtle::ConstantTimeEq;

pub struct AuthedClient(pub String);

impl FromRequestParts<SharedState> for AuthedClient {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let provided = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or(ApiError(ErrorCode::Unauthorized))?;

        for (client_id, token) in &state.cfg.client_tokens {
            if token.len() == provided.len()
                && bool::from(token.as_bytes().ct_eq(provided.as_bytes()))
            {
                return Ok(Self(client_id.clone()));
            }
        }
        Err(ApiError(ErrorCode::Unauthorized))
    }
}
