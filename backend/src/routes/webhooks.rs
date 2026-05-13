use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

use crate::domain::webhooks;
use crate::error::AppError;
use crate::state::AppState;

const SIGNATURE_HEADER: &str = "x-webhook-signature";
const PROVIDER_NAME: &str = "sim";

pub fn routes() -> Router<AppState> {
    Router::new().route("/webhooks/provider", post(receive))
}

// Body is extracted as `Bytes`, not `Json<T>`. HMAC must be computed over the
// raw bytes the provider sent — see learn/concepts/webhook-security.md.
async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), AppError> {
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    // The service returns:
    //   Ok(Processed | Ignored | Duplicate) — all map to 200 OK
    //   Err(AppError)                       — only on infrastructure failures
    //                                          (DB down, etc); becomes 5xx so
    //                                          the provider retries.
    //
    // The "always 200 for known outcomes" rule is the spec requirement and is
    // explained in learn/concepts/webhook-security.md.
    let _outcome = webhooks::process(
        &state.pool,
        &state.webhook_secret,
        PROVIDER_NAME,
        &body,
        signature.as_deref(),
    )
    .await?;

    Ok((StatusCode::OK, Json(json!({ "status": "ok" }))))
}
