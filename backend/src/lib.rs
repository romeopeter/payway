// Library entry: all modules live here so integration tests in `tests/`
// can import them. main.rs is a thin binary that uses this library.

pub mod config;
pub mod db;
pub mod domain;
pub mod error;
pub mod fx;
pub mod idempotency;
pub mod middleware;
pub mod routes;
pub mod state;
