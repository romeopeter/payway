mod health;
mod payments;

use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(health::routes())
        .merge(payments::routes())
        .with_state(state)
}
