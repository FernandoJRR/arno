//! Error surface: every non-2xx carries `{"error_code": ...}` (SPEC §4.1).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use contract::{ErrorBody, ErrorCode};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiError(pub ErrorCode);

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "order rejected: {}", self.0.as_str())
    }
}

impl std::error::Error for ApiError {}

impl From<contract::ContractError> for ApiError {
    fn from(_: contract::ContractError) -> Self {
        Self(ErrorCode::InvalidOrder)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.0.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(ErrorBody { error_code: self.0 })).into_response()
    }
}
