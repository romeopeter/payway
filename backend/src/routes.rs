mod health;
mod payments;
mod webhooks;

use axum::Router;

use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(health::routes())
        .merge(payments::routes())
        .merge(webhooks::routes())
        .with_state(state)
}
