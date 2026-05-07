use anyhow::Context;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod config;
mod db;
mod error;
mod middleware;
mod routes;
mod state;

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load .env if present. In production with real env vars, this is a no-op.
    dotenvy::dotenv().ok();

    init_tracing();

    let config = Config::from_env().context("loading configuration from environment")?;

    let pool = db::pool(&config.database_url)
        .await
        .context("connecting to postgres")?;

    // Migrations are embedded at compile time by the macro, so they ship
    // inside the binary; the migrations/ dir is not needed at runtime.
    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .context("running database migrations")?;

    let state = AppState { pool };

    let app = routes::router(state).layer(TraceLayer::new_for_http());
    let app = middleware::request_id::wrap(app);

    let addr = SocketAddr::from(([0, 0, 0, 0], config.port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding to {addr}"))?;

    tracing::info!(%addr, "payway backend listening");

    axum::serve(listener, app)
        .await
        .context("axum server failed")?;

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("payway_backend=debug,tower_http=info,sqlx=warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).compact())
        .init();
}
