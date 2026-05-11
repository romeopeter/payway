# `POST /payments` — walkthrough

> **What this is for:** a guided read of the `create_payment` service we built in 2b. Open [`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs) alongside this — the structure here mirrors that file. After reading, you should be able to explain every step, why it's in the order it's in, and what changes in production.

---

## What the endpoint does

`POST /payments` takes a payment request, claims funds via a ledger debit + clearing credit, gets an FX quote, "submits" to the (simulated) downstream provider, and returns the in-flight transaction with `status: processing`. All of that happens inside one DB transaction.

A successful response is **`202 Accepted`** — not 201. The payment isn't done; it's in flight, awaiting the provider's webhook callback (which 2c will handle).

---

## The handler is thin on purpose

[`backend/src/routes/payments.rs`](../backend/src/routes/payments.rs) is ~25 lines of glue:

1. Pull the `Idempotency-Key` header (require, length-check, ASCII).
2. Deserialize the JSON body.
3. Call `create_payment`.
4. Wrap the result as `(StatusCode::ACCEPTED, Json(response))`.

All business logic lives in [`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs). The split matters because:

- The handler is **HTTP-shaped** — knows about headers, status codes, JSON.
- The service is **DB-shaped** — takes `&PgPool`, returns `AppError`, knows nothing about HTTP.
- Tests target the service directly. No HTTP server, no mocking, no port wrangling.

This is the boring kind of layering, and it pays dividends when you want to add a CLI, a queue worker, or a retry job that creates payments through the same code path.

---

## The service flow

`create_payment` runs these steps in this order, all in one DB transaction.

### 1. Pre-flight validation (no DB)
`validate_and_normalize` rejects nonsense: `source_amount > 0`, `destination_currency` is 3 chars, uppercased. Cheap, fails fast, doesn't consume DB resources.

### 2. Compute `request_hash`
SHA-256 of the *re-serialized* request body, hex-encoded. We re-serialize via `serde_json` instead of hashing the raw incoming bytes because HTTP clients and proxies can reorder JSON keys or whitespace; the bytes vary, the logical content doesn't. Re-serializing produces stable bytes for stable structs. See [`learn/concepts/idempotency.md`](concepts/idempotency.md) on why this matters.

### 3. Begin DB transaction
```rust
let mut tx = pool.begin().await?;
```
Everything from here until `tx.commit()` is atomic. Failure = full rollback; no partial state visible.

### 4. Idempotency check
`idempotency::check` does a single `INSERT ... ON CONFLICT DO NOTHING`. Three outcomes:

- **Inserted** → first time we've seen this key; proceed.
- **Conflict, same hash, response_body present** → return cached response (deserialize from `JSONB`).
- **Conflict, different hash** → `AppError::IdempotencyConflict` → 422.

The DB unique index on `(scope, key)` is the *real* concurrency guarantee. Concurrent dupes block on the row lock; whichever wins inserts; the other replays. See [`learn/concepts/idempotency.md`](concepts/idempotency.md) §"Why the database is the only place" for the timing diagram.

### 5. `SELECT ... FOR UPDATE` on sender's `accounts` row
`fetch_sender_for_update` acquires a row-level write lock on the sender. Held until commit. While we hold it, no other transaction can `SELECT FOR UPDATE` the same row — they queue. This is the per-account mutex that prevents double-spend; see [`learn/concepts/double-spend.md`](concepts/double-spend.md).

We don't lock `ledger_entries`; we don't need to. The accounts lock funnels all writes for this account through one writer.

### 6. Validate currencies + recipient
- `source != destination` (the schema CHECK enforces this on insert too — defense in depth)
- both currencies exist and are `is_active = TRUE`
- recipient row exists

These map to specific 400-class errors with human messages.

### 7. Read balance from the ledger
```sql
SELECT COALESCE(SUM(amount), 0)::numeric FROM ledger_entries WHERE account_id = $1
```
If `balance < requested`, return `AppError::InsufficientBalance { balance, requested, currency }`. The error type carries structured fields so the HTTP response can show "you have 50,000 NGN; you tried to send 100,000 NGN" instead of a generic message. Maps to 422.

### 8. FX quote
`fx.quote(source, destination)` returns the rate from the simulated provider. Insert into `fx_quotes` with 60-second `expires_at`. Compute `destination_amount = (source_amount × rate).round_dp(4)`.

The quote is *unclaimed* at this point — `locked_by_transaction_id IS NULL`.

### 9. Insert `transactions` row (status='initiated')
The state machine trigger validates the insert; the history trigger writes a row to `transaction_status_history` automatically. We pass `initiated_at` from the Rust side (`Utc::now()`) so the response carries the same timestamp the DB stored.

### 10. Lock the FX quote to this transaction
```sql
UPDATE fx_quotes SET locked_by_transaction_id = $tx_id WHERE id = $quote_id
```
The partial unique index on `locked_by_transaction_id` ensures a quote is claimed by exactly one transaction. If two threads ever raced for the same quote (they shouldn't), one would fail the unique constraint.

### 11. Insert ledger pair
Two rows in one statement:
```
(transaction_id, sender_account_id,    -source_amount, source_currency, 'debit_sender')
(transaction_id, source_clearing_id,   +source_amount, source_currency, 'credit_clearing')
```
The deferred zero-sum trigger fires at commit and validates these net to zero per `(transaction_id, currency)`. If they don't, commit fails — the bug becomes loud.

We don't credit the recipient yet. That happens on webhook completion in 2c.

### 12. Simulated provider call
For the prototype, this is just `format!("SIM-{}", Uuid::new_v4())`. No network IO. **In production this is wrong** — see "What production would do differently" below.

### 13. Transition to `processing`
```sql
UPDATE transactions SET status = 'processing', provider_reference = $1, provider_name = 'sim', submitted_at = $2 WHERE id = $3
```
The state machine trigger validates `initiated → processing`. The history trigger writes another row.

### 14. Cache the response
`serde_json::to_value(&response)` → store in `idempotency_keys.response_body` along with `response_status = 202`. Now retries with the same key replay this exact JSON.

### 15. Commit
At commit, the deferred ledger zero-sum trigger validates. If we'd somehow inserted unbalanced entries (a bug), the commit would fail and *everything* rolls back atomically — including the idempotency claim. The next retry sees no row and proceeds fresh. There is no "we did the work but didn't record the response" half-state.

---

## What production would do differently

The simplification I called out in the proposal: the simulated provider call happens *inside* the DB transaction (step 12). In production this is wrong. Reasons:

1. **The provider call is network IO.** It can hang for seconds to minutes. While it hangs, we hold:
   - the sender account row lock (blocking all other payments from that sender),
   - a DB connection from the pool (starving other requests),
   - Postgres MVCC bloat (long-running transactions hold snapshots).
2. **Timeout ambiguity.** If we time out and roll back, the provider might still be processing the request we sent. We've lost the audit record while real money may still be moving.

The production pattern is the **outbox**:

1. **Transaction A:** idempotency claim → balance check → FX quote → insert transaction (`status='initiated'`) → insert ledger pair → COMMIT. (Funds are now durably "claimed.")
2. **Out of band** (separate worker, after-commit hook, message queue): network-call the provider with retries.
3. **Transaction B:** UPDATE transaction `SET status='processing', provider_reference, submitted_at` → COMMIT.

Now the network IO holds zero DB locks. If the provider is slow or flaky, we retry the call — the transaction stays durably `initiated` with the audit trail intact. If the provider eventually returns success, we transition forward; if it permanently fails, we transition to `failed` (with reversal).

We'll come back to this in Part 4B.5 (provider timeout) and Part 4C (production readiness). For the prototype with a synchronous in-memory simulator, in-transaction is fine — and far simpler to read top-to-bottom.

---

## What's tested

[`backend/tests/payments_create.rs`](../backend/tests/payments_create.rs) covers:

| Test | What it verifies |
|------|------------------|
| `happy_path_debits_sender_and_credits_clearing` | Response shape; balance went down by 1M; clearing went up by 1M |
| `idempotent_replay_returns_same_response` | Same key + body → identical response; only one transaction created |
| `same_key_different_body_is_a_conflict` | 422 IdempotencyConflict; second transaction not created |
| `insufficient_balance_rejects_and_writes_no_ledger` | Structured 422 with balance/requested/currency; no ledger writes |
| `same_currency_pair_rejected` | 400; `source == destination` is rejected |

Each test uses `#[sqlx::test(migrations = "../migrations")]` which spins up a fresh DB per test (with seed data applied) and runs them in parallel.

Run:
```bash
cd backend
DATABASE_URL=postgres://payway:payway_local_dev@localhost:5432/payway \
  cargo test --test payments_create
```

The test runner needs an empty Postgres reachable at the URL; sqlx creates a fresh database per test under the hood and migrates it.

---

## Things I deliberately did NOT do (yet)

- **A `services/repositories/` split.** The service function does both business logic and SQL right now. When we add the webhook handler in 2c, several SQL helpers will become reusable; that's the right time to extract a `repository` module.
- **A `FxRateProvider` trait.** We have one implementation. Introducing a trait now would be a placeholder with no second impl to call out the interface. We add it when 2c needs to inject a deterministic rate provider for tests, *if* deterministic isn't enough by then.
- **Custom `Json` rejection handler.** Axum's default 422 for malformed JSON is fine. Customizing it is polish.
- **Rate limiting and auth.** Out of scope per the spec.

---

## Cross-references

- [`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs) — the implementation
- [`backend/src/idempotency.rs`](../backend/src/idempotency.rs) — the helper
- [`backend/src/fx.rs`](../backend/src/fx.rs) — the simulated FX provider
- [`backend/src/routes/payments.rs`](../backend/src/routes/payments.rs) — the HTTP handler
- [`backend/tests/payments_create.rs`](../backend/tests/payments_create.rs) — tests
- [`learn/concepts/idempotency.md`](concepts/idempotency.md) — deeper on why request_hash, why DB
- [`learn/concepts/double-spend.md`](concepts/double-spend.md) — deeper on FOR UPDATE
- [`learn/schema-design.md`](schema-design.md) §4.3 — concurrency notes from Part 1
