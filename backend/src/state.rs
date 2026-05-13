use sqlx::PgPool;
use std::sync::Arc;

use crate::fx::SimulatedFxProvider;

// Shared state passed to every handler via Axum's State extractor.
// Cheap to clone: PgPool is internally Arc'd; our explicit Arc<...> wrappers
// make Clone a few refcount bumps.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub fx: Arc<SimulatedFxProvider>,
    // Webhook HMAC secret. Wrapped in Arc to avoid copying on every request.
    // In production this would be a typed `Secret<String>` (from the `secrecy`
    // crate) so Debug never accidentally leaks it; for the prototype, just
    // don't print AppState.
    pub webhook_secret: Arc<String>,
}
