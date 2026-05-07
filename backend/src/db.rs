use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;

pub async fn pool(database_url: &str) -> sqlx::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(Duration::from_secs(5))
        .test_before_acquire(true)
        .connect(database_url)
        .await
}
