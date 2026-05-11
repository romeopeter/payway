# Idempotency

> **What this is for:** the foundations behind [`backend/src/idempotency.rs`](../../backend/src/idempotency.rs) and the `idempotency_keys` table. After this you should be able to explain (a) why idempotency keys exist, (b) why "deduplicate the side effect" is the wrong mental model, (c) why the database is the only correct place to enforce it.

---

## The problem idempotency solves

Networks fail mid-request. The client sends `POST /payments`, the request reaches your server, you commit the payment, the response gets composed — and then the connection drops before the response gets back to the client. The client has no idea whether the payment happened.

Two bad outcomes from this:

- **Client retries blindly.** Payment happens twice. Customer sends $1,000 instead of $500.
- **Client gives up.** Payment is lost. Vendor doesn't get paid.

The fix is a contract between client and server, mediated by an `Idempotency-Key` header. The client sends a unique key per logical request and promises to re-use that key on retries. The server promises:

1. **At most once execution** — the side effects of the request happen no more than one time, regardless of how many retries arrive.
2. **Always-correct response** — every retry returns the same response (success or error) bit-for-bit.

That's idempotency. It makes "retry on network failure" safe.

---

## What idempotency is NOT

A common misconception: "deduplicate the side effect." Like "if a transaction already exists for this key, return it instead of creating a new one."

Half-right, but it misses an important case. Consider:

1. Client `POST /payments` with key `K`, body `B`.
2. Server validates, finds the recipient doesn't exist, returns `400 {"error": "recipient not found"}`.
3. Connection drops. Client doesn't know what happened.
4. Client retries with key `K`, body `B`.

If we only "deduplicate the transaction creation," there's no transaction to dedupe — the original request failed validation. So we'd re-validate, possibly against now-different recipient state, and return either a *different* error or maybe a success. The client sees inconsistent responses for the "same" request. That's not idempotency.

The correct semantics: **store the original *response*, not just the side effect.** Replays return the original response (success, validation error, or otherwise) regardless of whether the original "did" anything. The client always gets a coherent answer.

That's why `idempotency_keys.response_body` and `response_status` exist. We're not deduplicating side effects — we're caching the response.

---

## Why `request_hash` matters

Suppose a client reuses an idempotency key for a *different* request body. Common cause: a buggy retry library that uses a constant key. Or a mis-coded "save and submit" that always sends key=`save-1`.

What should the server do?

1. **Return the original response anyway.** Client thinks their NEW request succeeded with whatever the original returned. They send the wrong amount and never realize.
2. **Treat it as a fresh request and create a new transaction.** Violates idempotency. If the original is retried again, it now races with this one.
3. **Reject with 422.** Tells the client they have a bug.

We compute `SHA-256(serialized body)` at the start. The `idempotency_keys` row stores it. On a replay attempt:

- Same hash → safe to replay the cached response.
- Different hash → `IdempotencyConflict` → 422.

Stripe and PayPal both document this behavior. Most prototypes don't bother — and silently get it wrong.

**Why hash the re-serialized struct, not the raw incoming bytes?** HTTP clients, encoders, and proxies can reorder JSON keys, vary whitespace, and stringify numbers differently. The bytes vary across retries; the logical content doesn't. Re-serializing via `serde_json` produces stable bytes for stable structs. (If we change the request struct definition, old hashes invalidate — that's fine, since old keys also expire.)

---

## Why the database is the only correct place

You might be tempted to do this in application code (Redis, an in-memory map, anything):

```rust
// WRONG — racy
let exists = check_idempotency_in_kv(key).await?;
if exists {
    return cached_response;
}
do_the_work().await?;
store_in_kv(key, response).await?;
```

Two concurrent requests both reach line 2, both see "doesn't exist," both call `do_the_work`, both store. The side effect happened twice. The classic test-then-set race.

You can fix it with `SETNX` or a distributed lock — but at that point you're adding infrastructure for what Postgres' unique index does for free. And you've introduced a new failure mode: what if the lock service is up but Postgres is down (or vice versa)?

The Postgres way is the right way:

```sql
INSERT INTO idempotency_keys (scope, key, ...) VALUES (...)
ON CONFLICT (scope, key) DO NOTHING;
```

The unique index on `(scope, key)` *is* the lock. Two concurrent INSERTs of the same key:

1. Both attempt to insert.
2. The unique constraint forces row-level serialization at the write path.
3. The first succeeds; the second sees the existing row and `ON CONFLICT DO NOTHING` makes it a no-op.
4. The second's `rows_affected()` is `0` — that's how it knows it lost the race.
5. Loser reads the existing row and replays the cached response.

There is no race window. Check-and-insert is one atomic statement. This is what [`backend/src/idempotency.rs`](../../backend/src/idempotency.rs) does.

---

## Timing diagram for concurrent retries

```
T+0   Request 1 (key=K)              | Request 2 (key=K, same body)
T+1   BEGIN TX                       |
T+2   INSERT idempotency_keys        | BEGIN TX
T+3     - acquires row write lock    | INSERT idempotency_keys
T+4     - row inserted, response NULL|   - blocks on the write lock
T+5   ... do work ...                |
T+10  UPDATE idempotency_keys        |
        SET response = ...           |
T+11  COMMIT                         |
T+12                                 |   - unblocks
T+13                                 | INSERT sees existing row -> DO NOTHING
T+14                                 | rows_affected = 0
T+15                                 | SELECT existing row
T+16                                 | hash matches; response_body present
T+17                                 | return cached response
```

Both clients see the same answer. Request 2 never executed the side effect.

If Request 1 had failed and rolled back at T+11:

```
T+11  ROLLBACK                       |
T+12                                 |   - unblocks
T+13                                 | INSERT now succeeds (no row exists)
T+14                                 | proceeds normally
```

Request 2 just becomes the "first." This is the correct behavior — the original failure shouldn't poison future retries.

---

## TTL and cleanup

Keys live for 24 hours (`expires_at = NOW() + INTERVAL '24 hours'` at insert). After that the same key can be reused for a new request.

A periodic cleanup job is needed to actually delete expired rows; without it, the table grows forever. Not built yet — production-readiness item. Implementation is straightforward: a background task (or pg_cron) running `DELETE FROM idempotency_keys WHERE expires_at < NOW()` once an hour.

Why 24h? Long enough that legitimate retry windows don't expire (clients typically give up within seconds-to-minutes of failure). Short enough that the table doesn't bloat. Stripe uses 24h for the same reasons.

---

## Common antipatterns

**Storing the key without the response.** "Have we seen this key?" doesn't get you a coherent retry — you also need the original response. If your idempotency design has no response cache, you're storing a flag, not implementing idempotency.

**Application-level dedup with separate check/insert.** Anything where check and insert are two different operations is racy. Use the DB unique index.

**Treating idempotency as "exactly-once delivery."** It isn't. It's "at-most-once execution + always-correct response on retry." The network can still drop responses; the database can still be unreachable; the client still has to retry. Idempotency just makes those retries safe.

**Hashing the raw incoming bytes.** Tempting because "the bytes are what came in." But they vary across encoders and proxies for reasons unrelated to logical content. Re-serialize the deserialized struct and hash that.

**Auto-deleting on read.** Some implementations delete the idempotency row after a "successful" replay. Don't — a third retry would create a duplicate. Let TTL handle cleanup.

**Per-tenant keys without scoping.** Two different customers using the same key string (e.g. `"key-1"`) shouldn't collide. We use a `scope` column (`'POST /payments'` etc.) so the unique constraint is `(scope, key)`. In a multi-tenant system, scope would also include tenant id.

---

## Cross-references

- [`backend/src/idempotency.rs`](../../backend/src/idempotency.rs) — the helper
- [`backend/src/domain/payments.rs`](../../backend/src/domain/payments.rs) — usage in `create_payment`
- [`learn/payments-create.md`](../payments-create.md) §4 — where this fits in the flow
- [`learn/schema-design.md`](../schema-design.md) §1.5 — schema rationale for `idempotency_keys`
