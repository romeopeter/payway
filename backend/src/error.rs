use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
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

    #[error("idempotency key reused with a different request body")]
    IdempotencyConflict,

    #[error("database error")]
    Database(#[from] sqlx::Error),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        // Public-safe message vs. internal detail. Client-error variants leak
        // their message; server-error variants log detail and return generic.
        let (status, message) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, self.to_string()),
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, self.to_string()),
            AppError::IdempotencyConflict => (StatusCode::UNPROCESSABLE_ENTITY, self.to_string()),
            AppError::Database(_) | AppError::Internal(_) => {
                tracing::error!(error = ?self, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_string(),
                )
            }
        };

        (status, Json(json!({ "error": message }))).into_response()
    }
}
