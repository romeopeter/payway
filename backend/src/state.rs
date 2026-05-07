use sqlx::PgPool;

// Shared state passed to every handler via Axum's State extractor.
// Cheap to clone (PgPool is internally Arc'd).
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
}
