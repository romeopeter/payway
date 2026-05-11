use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};

use crate::domain::payments::{create_payment, CreatePaymentRequest, CreatePaymentResponse};
use crate::error::AppError;
use crate::state::AppState;

const IDEMPOTENCY_HEADER: &str = "idempotency-key";

pub fn routes() -> Router<AppState> {
    Router::new().route("/payments", post(create))
}

// Thin handler: pulls headers + body, delegates to the domain service,
// renders the result as 202 Accepted. All business logic lives in
// `crate::domain::payments::create_payment`.
async fn create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreatePaymentRequest>,
) -> Result<(StatusCode, Json<CreatePaymentResponse>), AppError> {
    let idempotency_key = require_idempotency_key(&headers)?;

    let response = create_payment(&state.pool, &state.fx, &idempotency_key, body).await?;

    Ok((StatusCode::ACCEPTED, Json(response)))
}

fn require_idempotency_key(headers: &HeaderMap) -> Result<String, AppError> {
    let raw = headers
        .get(IDEMPOTENCY_HEADER)
        .ok_or_else(|| AppError::BadRequest("missing Idempotency-Key header".into()))?
        .to_str()
        .map_err(|_| AppError::BadRequest("Idempotency-Key must be ASCII".into()))?
        .trim();

    if raw.is_empty() || raw.len() > 255 {
        return Err(AppError::BadRequest(
            "Idempotency-Key must be 1..=255 characters".into(),
        ));
    }

    Ok(raw.to_string())
}
