use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use rust_decimal::Decimal;
use serde_json::json;

// AppError is the single error type our handlers return. Anywhere a handler
// can fail, it returns Result<T, AppError>. The `?` operator (see
// learn/concepts/error-handling.md) converts other error types into this one
// via the `#[from]` impls below.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,

    #[error("invalid input: {0}")]
    BadRequest(String),

    #[error("insufficient balance: {balance} {currency} < requested {requested}")]
    InsufficientBalance {
        balance: Decimal,
        requested: Decimal,
        currency: String,
    },

    #[error("idempotency key reused with a different request body")]
    IdempotencyConflict,

    #[error("FX pair not supported: {0} -> {1}")]
    UnsupportedFxPair(String, String),

    #[error("database error")]
    Database(#[from] sqlx::Error),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Public-safe message vs. internal detail. Client-error variants leak
        // their message; server-error variants log detail and return generic.
        let (status, body) = match &self {
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                json!({ "error": self.to_string() }),
            ),
            AppError::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                json!({ "error": self.to_string() }),
            ),
            AppError::InsufficientBalance {
                balance,
                requested,
                currency,
            } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({
                    "error": "insufficient_balance",
                    "message": self.to_string(),
                    "balance": balance.to_string(),
                    "requested": requested.to_string(),
                    "currency": currency,
                }),
            ),
            AppError::IdempotencyConflict => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": self.to_string() }),
            ),
            AppError::UnsupportedFxPair(_, _) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({ "error": self.to_string() }),
            ),
            AppError::Database(_) | AppError::Internal(_) => {
                tracing::error!(error = ?self, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({ "error": "internal server error" }),
                )
            }
        };

        (status, Json(body)).into_response()
    }
}
