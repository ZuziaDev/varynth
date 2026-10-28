use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden(String),
    #[error("bad request")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict(String),
    #[error("upstream unavailable")]
    Unavailable,
    #[error("rate limited")]
    RateLimited(String),
    #[error("billing unavailable")]
    BillingUnavailable,
    #[error("internal")]
    Internal,
}

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(%error, "database");
        Self::Internal
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message, kind) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized".to_string(), "auth"),
            Self::Forbidden(message) => (
                StatusCode::FORBIDDEN,
                message.clone(),
                "forbidden",
            ),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                message.clone(),
                "invalid_request_error",
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "model not found".to_string(), "not_found"),
            Self::Conflict(message) => (StatusCode::CONFLICT, message.clone(), "conflict"),
            Self::Unavailable => (
                StatusCode::BAD_GATEWAY,
                "upstream unavailable".to_string(),
                "upstream",
            ),
            Self::RateLimited(message) => (
                StatusCode::TOO_MANY_REQUESTS,
                message.clone(),
                "rate_limit",
            ),
            Self::BillingUnavailable => (
                StatusCode::NOT_IMPLEMENTED,
                "self-service plan changes are not enabled on this server".to_string(),
                "billing_unavailable",
            ),
            Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
                "internal",
            ),
        };
        (status, Json(json!({ "error": { "message": message, "type": kind } }))).into_response()
    }
}
