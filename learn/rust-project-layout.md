# Rust Project Layout

> **What this is for:** a guided tour of the backend code as it stands after Part 2a. Reading this is the fastest way to map your JS/Python intuitions (assuming you most likely have knowledge of two languages) onto the Rust files in [`backend/`](../backend/). It also explains *why* each module exists, so when we add `routes/payments.rs` in Part 2b, you already know where it slots in.

---

## Mental model: the pieces

Three things hold up the backend, and you should keep them straight in your head:

| Concept            | What it does                                                    | JS / Python analogue |
|--------------------|------------------------------------------------------------------|----------------------|
| **Cargo**          | Package manager + build tool                                     | npm / pip + a build script |
| **Crate**          | A single Rust package; one `Cargo.toml` per crate                | npm package |
| **Module**         | A namespace of code inside a crate; a file or a folder           | An ES module / a Python package |
| **Axum**           | Web framework: routing, extractors, IntoResponse                 | Express / FastAPI |
| **Tower**          | Lower-level middleware abstraction Axum is built on              | (no clean analogue — middleware-as-a-trait) |
| **sqlx**           | Async DB library; you write SQL strings, it gives type safety    | knex (in honesty mode) |
| **tokio**          | Async runtime. `#[tokio::main]` is what makes `async` do anything | Node's event loop, but you opt in |

Most JS-flavored confusion ("but where's the framework? why are there so many crates?") comes from these being separate packages by design. Rust's standard library is small; you compose what you need.

---

## File-by-file walkthrough

### [`backend/Cargo.toml`](../backend/Cargo.toml)

This is `package.json`. Every Rust crate has one. The `[dependencies]` section is where we pin direct deps; transitive deps come along automatically.

A few things worth understanding:

- **Feature flags.** Most crates have features that gate optional code. `sqlx` is the clearest example:
  ```toml
  sqlx = { version = "0.8", features = ["runtime-tokio-rustls", "postgres", ...] }
  ```
  Without `"postgres"`, sqlx wouldn't compile against Postgres at all (it supports MySQL, SQLite too). Without `"macros"`, the `sqlx::query!` macro disappears. Pick what you need; you pay nothing for what you don't.
- **`runtime-tokio-rustls`** picks the async runtime (tokio) and the TLS implementation (rustls — pure-Rust, no system OpenSSL). The alternative would be `runtime-tokio-native-tls`, which links against system OpenSSL.
- **`rust_decimal` with `serde-str`.** Serializes amounts as JSON strings (`"123.4500"`), not numbers. Numbers in JSON go through f64 in most clients, which silently loses precision for money. Strings round-trip exactly.

### [`backend/src/main.rs`](../backend/src/main.rs)

The binary entry point. The `#[tokio::main]` attribute is a macro that wraps your `async fn main` in a synchronous shim that boots the tokio runtime and drives your future to completion. Without it, you'd write the boot code by hand (`Runtime::new()...block_on(...)`).

The body runs in this order:
1. `dotenvy::dotenv().ok()` — loads `.env` if present, otherwise no-op. The `.ok()` discards the error because "no .env file" is fine.
2. `init_tracing()` — sets up structured logging based on `RUST_LOG`.
3. `Config::from_env()` — reads our env vars into a typed struct. Fails fast if anything required is missing.
4. `db::pool(...)` — opens the Postgres pool.
5. `sqlx::migrate!("../migrations").run(...)` — applies any pending migrations. The `migrate!` macro is the special bit:
   - It runs **at compile time**, reading every `.sql` file in `../migrations/` and embedding their contents into the binary.
   - At runtime, the binary doesn't need the `migrations/` directory anywhere — it carries them inside.
   - That's why our Dockerfile copies `migrations/` only into the *builder* stage, not the runtime image.
6. `routes::router(state).layer(...)` — assembles the Axum router with global middleware.
7. `axum::serve(listener, app).await` — actually serve.

### [`backend/src/config.rs`](../backend/src/config.rs)

Loads env vars into a typed `Config` struct. The pattern is intentional:

- Env access is centralised. No `std::env::var(...)` scattered through the codebase.
- Required vs. optional is explicit per field (`env_required` vs. `env_optional`).
- Failure to load is a startup error, not a runtime surprise three hours after deploy.

JS analogue: think of this as the `dotenv-safe` pattern — load and validate at boot.

### [`backend/src/db.rs`](../backend/src/db.rs)

A four-line module that exposes one function: `pool(database_url)`. The pool itself (`PgPool`) is internally `Arc`'d, so cloning is cheap; we clone it into `AppState` and into anything that needs DB access.

`max_connections=20` is a guess, not a tuned value. We'll revisit in production-readiness analysis (Part 4C).

### [`backend/src/error.rs`](../backend/src/error.rs)

The single error type our handlers return. Every fallible handler will be `Result<T, AppError>`. See [`learn/concepts/error-handling.md`](./concepts/error-handling.md) for the deeper pattern; here, the things to notice:

- `#[derive(thiserror::Error)]` — generates `Display` + `Error` trait impls from the `#[error("...")]` annotations.
- `#[from] sqlx::Error` and `#[from] anyhow::Error` — generate `impl From<X> for AppError`. This is what makes `?` work: when you write `db_call().await?` in a handler, the compiler invokes the relevant `From` to convert the `sqlx::Error` into an `AppError::Database`.
- `IntoResponse` — Axum-specific trait that turns the error into an HTTP response. We pattern-match on the variant: client errors leak their message; server errors get logged and return a generic `500`.

Why it matters in a payments context: if a database error happens during payment processing, the user does **not** see "FOREIGN KEY constraint violation on idempotency_keys_pkey." They see `500 internal server error` and we see the full error in our logs with a request ID.

### [`backend/src/state.rs`](../backend/src/state.rs)

`AppState` is what Axum passes to handlers via the `State` extractor. Right now it's just the DB pool; over time it'll grow to include FX rate provider, webhook secret, etc. Cloned on every request — keep it cheap.

Convention: anything you'd reach for via dependency injection in another framework, you put on `AppState`.

### [`backend/src/routes.rs`](../backend/src/routes.rs) and [`backend/src/routes/health.rs`](../backend/src/routes/health.rs)

`routes.rs` is the module file (Rust 2018+ convention — the file is named after the module, with submodules in a directory of the same name). It does one thing: declare the submodule and assemble the router.

`routes/health.rs` is the actual `GET /health` handler. The handler signature is the Axum pattern you'll see everywhere:

```rust
async fn health(State(state): State<AppState>) -> (StatusCode, Json<Value>)
```

- `State(state): State<AppState>` is an **extractor**: Axum's mechanism for "give me this thing from the request context." Other extractors include `Json<T>` (parse the body), `Path<T>` (URL params), `Query<T>` (querystring), `Headers`. They're all just types that implement a trait — no magic, no decorators.
- Return type is `(StatusCode, Json<Value>)`. Axum has `IntoResponse` impls for tuples, JSON, status codes, etc., and composes them. Returning `(StatusCode::OK, Json(...))` is "200 with this JSON body." This is the JS analogue of `res.status(200).json(...)`.

### [`backend/src/middleware.rs`](../backend/src/middleware.rs) and [`backend/src/middleware/request_id.rs`](../backend/src/middleware/request_id.rs)

Tower middleware is layered onto the router. We use [`tower-http`](https://docs.rs/tower-http) which ships pre-built middleware for common needs.

`request_id::wrap` adds two layers:

- `SetRequestIdLayer` — if the inbound request has no `x-request-id`, generate one (UUID v4) and add it.
- `PropagateRequestIdLayer` — copy the request id from request to response so callers can correlate.

The `ServiceBuilder` pattern stacks them in the right order. Axum's `.layer()` adds the outermost layer last (counterintuitive but consistent with how the underlying tower works), so we use `ServiceBuilder` to make the order explicit instead of trying to remember which `.layer()` call wins.

Why this matters before any business logic: every log line, every error, every audit record will reference the request id we stamp here. If the request id isn't generated *first*, your logs have requests you can't correlate.

---

## How the modules see each other

The file `main.rs` declares submodules at the top:

```rust
mod config;
mod db;
mod error;
mod middleware;
mod routes;
mod state;
```

Each `mod foo;` tells the compiler "look for `src/foo.rs` (or `src/foo/mod.rs`) and pull it into this crate as a module named `foo`." That's the only mechanism — there is no implicit indexing of `src/`. If you create `src/widgets.rs` and don't add `mod widgets;` somewhere, it does not exist as far as the compiler is concerned.

`pub` controls visibility. By default, items are private to their module. A function I want callable from `main.rs` needs `pub fn`. A field I want readable from another module needs `pub`. There's also `pub(crate)` (visible anywhere in this crate) and `pub(super)` (visible to the parent module) for finer control.

Path syntax: `crate::foo::bar` (absolute), `super::foo` (parent), `self::foo` (current), or `use crate::foo::bar;` to bring it into local scope. JS analogue: a much stricter version of `import { bar } from '../foo'`.

---

## What's missing on purpose

Things you might expect that aren't here yet — they land in 2b and beyond:

- A **domain layer** (`domain/payments.rs`, etc.) with the actual business types (`Payment`, `LedgerEntry`, etc.). I'll add this in 2b alongside `POST /payments` so you see the types and the handler together.
- A **service / repository split**. Some Rust codebases separate "what business logic does" from "how it talks to the DB" via traits. We'll keep it flat for now; introducing the split before there's a reason to is the kind of premature abstraction the [skill](../.claude/skills/payway-guide/SKILL.md) warns against.
- **Tests.** Unit tests inline (`#[cfg(test)] mod tests`), integration tests in `backend/tests/`. Coming with 2b — test-first is hard for a primer; I'd rather show you the working endpoint, then the tests for it.
- **Authentication.** Out of scope per the spec.

---

## Building and running

Locally:
```bash
cd backend
cargo build           # downloads deps + compiles
cargo run             # build + run, looks for .env in ../
cargo check           # type-check only, much faster than build
cargo fmt             # rustfmt — run before committing
cargo clippy          # linter — run before committing
```

Via Docker:
```bash
docker compose up                 # postgres + backend
docker compose up postgres        # just postgres
docker compose logs -f backend    # tail backend logs
docker compose down -v            # stop + remove volumes (drops DB)
```

First Docker build is slow (downloads + compiles all deps). Subsequent builds with unchanged `Cargo.toml` reuse the cached dependency layer — that's what the dummy-main trick in the [Dockerfile](../backend/Dockerfile) is doing.

---

## What to read next

- [`learn/concepts/error-handling.md`](./concepts/error-handling.md) — the `Result`/`?`/`thiserror`/`IntoResponse` pattern in depth. Read before 2b.
- Then back to [`requirement.md`](../requirement.md) §Part 2 to refresh on `POST /payments`.
- Then 2b lands.
