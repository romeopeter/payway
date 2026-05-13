# Payway

A simplified cross-border payment processing service for Nigerian businesses paying foreign suppliers. Implements the full payment lifecycle (initiate → submit → webhook → settle) with strict double-entry ledger integrity, idempotency, and a state machine enforced at the database.

This is a learning project. The substantive parts are the design rationale and failure-mode analysis in [`learn/`](./learn/), not just the running code. Start there if you want to understand the *why*.

## Stack

- **Backend:** Rust + Axum (`backend/`)
- **Frontend:** React (`frontend/` — added in Part 3)
- **Database:** PostgreSQL 16 (sqlx as the client)
- **Orchestration:** Docker Compose

## Quickstart

You only need [Docker](https://docs.docker.com/get-docker/) and Docker Compose. Rust and Node are not required to run.

```bash
cp .env.example .env
docker compose up
```

The backend will:

1. Wait for Postgres to be healthy.
2. Run migrations (idempotent — safe to re-run).
3. Listen on `http://localhost:8080`.

Sanity check:

```bash
curl http://localhost:8080/health
# {"ok":true}
```

## Local development without Docker

Faster inner loop if you have Rust installed:

```bash
docker compose up postgres        # just the database
cd backend
cargo run                          # runs migrations + serves
```

The backend reads `DATABASE_URL` from `.env`. When running the backend natively while Postgres is in Docker, the `DATABASE_URL` host should be `localhost` (the default in `.env.example`).

## Project layout

```
payway/
├── backend/                     Rust + Axum service
│   ├── Cargo.toml
│   ├── Dockerfile
│   ├── src/
│   │   ├── lib.rs               library entry: declares pub modules
│   │   ├── main.rs              binary entry: tracing, config, pool, migrate, serve
│   │   ├── config.rs            env-var loader
│   │   ├── db.rs                Postgres pool setup
│   │   ├── error.rs             AppError + IntoResponse
│   │   ├── state.rs             AppState passed to handlers
│   │   ├── fx.rs                simulated FX rate provider
│   │   ├── idempotency.rs       INSERT...ON CONFLICT helper + replay
│   │   ├── webhook_signature.rs HMAC-SHA256 verifier with unit tests
│   │   ├── domain.rs
│   │   ├── domain/
│   │   │   ├── payments.rs      create_payment service + types
│   │   │   └── webhooks.rs      webhook processor (signature, dedup, state)
│   │   ├── routes.rs            router assembly
│   │   ├── routes/
│   │   │   ├── health.rs        GET /health
│   │   │   ├── payments.rs      POST /payments handler
│   │   │   └── webhooks.rs      POST /webhooks/provider handler
│   │   ├── middleware.rs
│   │   └── middleware/
│   │       └── request_id.rs    x-request-id stamping/propagation
│   └── tests/
│       ├── payments_create.rs   POST /payments integration tests
│       ├── webhooks_provider.rs POST /webhooks/provider integration tests
│       └── payments_read.rs     GET /payments integration tests
├── frontend/                    React app (Part 3)
├── migrations/                  PostgreSQL migrations (sqlx)
│   ├── 0001_initial_schema.sql
│   └── 0002_seed.sql
├── learn/                       Written analysis — read these
│   ├── schema-design.md
│   ├── rust-project-layout.md
│   ├── payments-create.md
│   ├── webhooks.md
│   ├── payments-read.md
│   ├── code-review-junior-webhook.md
│   ├── failure-scenarios.md
│   ├── production-readiness.md
│   └── concepts/
│       ├── error-handling.md
│       ├── idempotency.md
│       ├── double-spend.md
│       └── webhook-security.md
├── docker-compose.yml
├── .env.example
└── requirement.md               original spec
```

## Where the design lives

- [`learn/schema-design.md`](./learn/schema-design.md) — Part 1: schema rationale, balance integrity, currency operations
- [`learn/rust-project-layout.md`](./learn/rust-project-layout.md) — Part 2a: how the Rust code is organized, what each dependency does
- [`learn/payments-create.md`](./learn/payments-create.md) — Part 2b: walkthrough of `POST /payments`, in-transaction vs. outbox pattern
- [`learn/webhooks.md`](./learn/webhooks.md) — Part 2c: walkthrough of `POST /webhooks/provider`, completion vs. reversal ledger entries
- [`learn/payments-read.md`](./learn/payments-read.md) — Part 2d: walkthrough of `GET /payments/:id` and `GET /payments`, pagination + filter patterns
- [`learn/code-review-junior-webhook.md`](./learn/code-review-junior-webhook.md) — Part 4A: critique of the junior dev's broken handler
- [`learn/failure-scenarios.md`](./learn/failure-scenarios.md) — Part 4B: how the system handles double-spend, late webhook, stale FX, partial settlement, provider timeout
- [`learn/production-readiness.md`](./learn/production-readiness.md) — Part 4C: top 5 changes before deploying with real money
- [`learn/concepts/error-handling.md`](./learn/concepts/error-handling.md) — Rust error handling for someone coming from JS/Python
- [`learn/concepts/idempotency.md`](./learn/concepts/idempotency.md) — why request_hash, why DB-only, common antipatterns
- [`learn/concepts/double-spend.md`](./learn/concepts/double-spend.md) — `SELECT FOR UPDATE`, isolation levels, the prepared answer to Part 4B.1
- [`learn/concepts/webhook-security.md`](./learn/concepts/webhook-security.md) — HMAC, raw bytes vs. parsed JSON, constant-time compare, always-200
- More added as we ship each part of the spec.

## Assumptions

- **Postgres is the only datastore.** No Redis, no separate KV. Idempotency and webhook deduplication are enforced by Postgres unique indexes. See [`learn/schema-design.md`](./learn/schema-design.md) §1.5 and §1.6.
- **The downstream payment provider is simulated.** A stub returns a `provider_reference` synchronously; webhook callbacks are triggered manually for testing.
- **Single-tenant.** No `tenant_id` on any table.
- **No authentication on the API.** In production this would sit behind an auth gateway.
- **Cargo.lock is generated by Cargo on first build.** Commit it after the first successful `cargo build` to lock dependency versions.

## Spec

Original requirements are in [`requirement.md`](./requirement.md). If anything in this README contradicts the spec, the README is wrong.
