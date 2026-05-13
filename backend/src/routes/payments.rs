use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use uuid::Uuid;

use crate::domain::payments::{
    create_payment, get_payment_detail, list_payments, CreatePaymentRequest, CreatePaymentResponse,
    ListPaymentsQuery, ListPaymentsResponse, PaymentDetail,
};
use crate::error::AppError;
use crate::state::AppState;

const IDEMPOTENCY_HEADER: &str = "idempotency-key";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/payments", post(create).get(list))
        .route("/payments/:id", get(detail))
}

// ---------------------------------------------------------------------------
// POST /payments
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// GET /payments/:id
// ---------------------------------------------------------------------------

async fn detail(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<PaymentDetail>, AppError> {
    let payment = get_payment_detail(&state.pool, id).await?;
    Ok(Json(payment))
}

// ---------------------------------------------------------------------------
// GET /payments
// ---------------------------------------------------------------------------

async fn list(
    State(state): State<AppState>,
    Query(params): Query<ListPaymentsQuery>,
) -> Result<Json<ListPaymentsResponse>, AppError> {
    let response = list_payments(&state.pool, params).await?;
    Ok(Json(response))
}
