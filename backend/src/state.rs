use sqlx::PgPool;
use std::sync::Arc;

use crate::fx::SimulatedFxProvider;

// Shared state passed to every handler via Axum's State extractor.
// Cheap to clone: PgPool is internally Arc'd, the FX provider is wrapped
// in our own Arc so cloning AppState is two refcount bumps.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub fx: Arc<SimulatedFxProvider>,
}
