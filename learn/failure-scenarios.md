# Failure Scenarios (Spec Part 4B)

> **What this is for:** the spec asks how the system handles five concrete failure scenarios. For each, this doc says what our implementation does *today*, what it *should* do (with code sketches where we haven't built it yet), and what we'd reference in production-readiness work.

Throughout, I distinguish between:

- **What we built.** Verifiable in `backend/src/`.
- **What we'd build for production.** Sketch-level; not in the code yet.

---

## 4B.1 — Double-spend

**Scenario:** Two concurrent `POST /payments` requests arrive with *different* idempotency keys but for the *same sender account*, whose balance only covers one payment.

### What we built

The flow at `create_payment` does, in order:

1. `pool.begin()` — start a DB transaction.
2. Idempotency claim (different keys here, so both proceed).
3. **`SELECT ... FOR UPDATE` on the sender's `accounts` row.** This is the per-account mutex.
4. Read the sender's balance from the ledger.
5. If balance < requested, return `InsufficientBalance` (no ledger writes).
6. Otherwise, write the debit/credit pair, transition to `processing`, commit.

The lock is the answer. Walking through the race with the lock in place:

```
T+0  Request A                            | Request B
T+1  BEGIN; SELECT FOR UPDATE accounts    |
T+2    - acquires the row write lock      | BEGIN; SELECT FOR UPDATE accounts
T+3  read balance: 1,000,000              |   - blocks waiting for A's lock
T+4  1,000,000 >= 800,000 ? yes           |
T+5  insert ledger pair (-800k / +800k)   |
T+6  UPDATE status = 'processing'         |
T+7  COMMIT                               |
T+8                                       |   - lock released; acquires it
T+9                                       | read balance: 200,000
T+10                                      | 200,000 >= 800,000 ? no
T+11                                      | return InsufficientBalance; ROLLBACK
```

No overdraft, no money created. Request B sees the *post-commit* balance — Postgres' `READ COMMITTED` isolation guarantees each statement reads the latest committed data, and `SELECT FOR UPDATE` ensures Request B's read happens after Request A commits.

### Reference

- [`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs) `fetch_sender_for_update`
- [`learn/concepts/double-spend.md`](concepts/double-spend.md) — full design rationale
- [`backend/tests/payments_create.rs`](../backend/tests/payments_create.rs) `insufficient_balance_rejects_and_writes_no_ledger` — covers the sequential half of the proof

### What we'd add for production

The test covers sequential ordering but not the concurrent race itself. A production test suite would include a real concurrency test:

```rust
#[sqlx::test(migrations = "../migrations")]
async fn concurrent_overdraft_attempts_only_one_succeeds(pool: PgPool) {
    let fx = Arc::new(SimulatedFxProvider::new());
    let pool = Arc::new(pool);

    // Two payments, each for 60M NGN. Only one fits in 100M balance.
    let (r1, r2) = tokio::join!(
        attempt_payment(pool.clone(), fx.clone(), "key-a", dec!(60000000)),
        attempt_payment(pool.clone(), fx.clone(), "key-b", dec!(60000000)),
    );

    let outcomes = [&r1, &r2];
    let successes = outcomes.iter().filter(|r| r.is_ok()).count();
    let failures = outcomes.iter().filter(|r| matches!(r, Err(AppError::InsufficientBalance{..}))).count();
    assert_eq!(successes, 1);
    assert_eq!(failures, 1);
}
```

Reliable concurrency tests are hard (results depend on timing); in CI we'd run them many times and assert the invariant holds across all runs. This is more polish than gap — the lock pattern is correct; we just haven't exercised it in tests.

---

## 4B.2 — Webhook arrives before the API response

**Scenario:** The downstream provider sends the webhook callback *before* our `POST /payments` handler has finished writing the transaction to the database.

### Why this can or can't happen

In our prototype, **it can't happen**. Reasons:

- We don't *call* a real provider — the "submit to provider" is `format!("SIM-{}", uuid)` inside the same DB transaction.
- We generate the `provider_reference` ourselves (`SIM-<uuid>`). The provider doesn't know about it until the (imaginary) submission, which is inside our transaction.
- The DB transaction commits atomically: either the row exists with the reference, or nothing exists.

In **production with a real provider**, it absolutely can happen. The realistic timeline:

```
T+0   We POST to provider's /create-payment       (start of network call)
T+1   Provider receives, stores, returns 201       (network round-trip)
T+2   Provider immediately enqueues "processing" webhook
T+3   Provider's webhook system fires             ┐
T+4   We receive webhook, look up provider_ref    │ ← race window
T+5   Our DB transaction commits                   ┘
```

Between T+3 and T+5, the webhook arrives at our `/webhooks/provider` but `SELECT FROM transactions WHERE provider_reference = ?` returns nothing. With our current code, the handler:

1. Verifies signature ✓
2. Inserts `webhook_events` row (signature_valid=true, status=pending)
3. Looks up transaction by provider_reference — not found
4. Marks webhook `processing_status='ignored'` with reason `'unknown provider_reference'`
5. Returns 200

**The webhook is logged but never processed.** Worse, since we returned 200, the provider won't retry. The payment stays in `processing` forever (or until manual intervention).

### What would handle it

Two parts:

**1. Outbox pattern: client-chosen reference, durable before any network call.**

Instead of letting the provider mint the reference, we mint it ourselves at transaction creation time and submit it as a `client_reference_id`:

```rust
// In create_payment, BEFORE any network call:
let client_reference_id = Uuid::new_v4();
sqlx::query("INSERT INTO transactions (..., client_reference_id, ...) VALUES (...)")
    .bind(client_reference_id)
    ...
tx.commit().await?;  // Committed: durable record exists.

// AFTER commit (in a separate worker):
let provider_ref = provider.submit(client_reference_id, amount, ...).await?;
sqlx::query("UPDATE transactions SET provider_reference = $1, status = 'processing' WHERE id = $2")
    ...
```

Now the provider's webhook can include the `client_reference_id` (we tell them to). When we receive it, we look up by *that*, which we know exists because we committed it before the network call.

**2. Webhook replay queue for unmatched events.**

Even with client_reference_ids, transient races (e.g., a stale read replica) can happen. The webhook_events row with `status='ignored'` and `reason='unknown...'` should be re-attempted by a background worker:

```sql
SELECT id, raw_payload
FROM webhook_events
WHERE processing_status = 'ignored'
  AND processing_error LIKE 'unknown provider_reference%'
  AND received_at > NOW() - INTERVAL '24 hours'
ORDER BY received_at
LIMIT 50;
```

For each, re-extract the reference and re-attempt. If the transaction now exists, process it; bump `processed_at`. If still not found after some grace period (1 hour, say), it's a genuine orphan — alert ops.

### Reference

- Current "always 200, log everything" stance is what makes the audit trail durable: [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) `process`
- The "unknown reference" path is intentional: see [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) "Why always 200"

The structural prerequisite (outbox + client_reference_id) is item #1 in [`learn/production-readiness.md`](production-readiness.md).

---

## 4B.3 — Stale FX quote

**Scenario:** A user receives a quote, waits 10 minutes, then confirms. The market rate has moved 3% against them.

### What we built

The schema supports this:

```sql
CREATE TABLE fx_quotes (
  id                       UUID PRIMARY KEY,
  ...
  rate                     NUMERIC(20, 8) NOT NULL,
  expires_at               TIMESTAMPTZ NOT NULL,
  locked_by_transaction_id UUID UNIQUE  -- partial: where IS NOT NULL
);
```

But there is **no separate `POST /quotes` endpoint yet**. In `create_payment`, the quote is generated and used atomically — there's no "user waits 10 minutes" window.

So the scenario as described doesn't apply to our current API surface. The 60-second `expires_at` we insert is essentially decoration; nothing reads it.

### What the system should do

The intended UX flow is **quote → confirm → commit**:

1. Client `POST /quotes` with `{source_currency, source_amount, destination_currency}`.
2. Server returns `{quote_id, rate, destination_amount, expires_at}`.
3. Client displays the rate to the user. User decides.
4. Client `POST /payments` with `{quote_id, sender_account_id, recipient_id}` (no amount — the quote has it).
5. Server validates: `quote_id` exists, `expires_at > NOW()`, `locked_by_transaction_id IS NULL`. Locks and proceeds.

If the quote has expired (step 4 happens after `expires_at`):

- Return **`410 Gone`** with `{"error": "quote_expired", "expires_at": "..."}`.
- The client must re-quote (`POST /quotes`) and re-confirm with the user.

**Why force re-quote and not auto-re-quote?**

Three options:
- **A. Reject with error, force re-quote.** ← my pick
- B. Auto re-quote and proceed with the new rate (silently).
- C. Auto re-quote, return new rate to user for explicit confirmation.

A is correct for an API: the contract is "I committed to this rate; honor or fail." B silently changes terms the user agreed to — that's a customer support nightmare and arguably a regulatory issue (you quoted X, charged Y). C requires a re-confirmation UX, which is the UI's job — the API just exposes the primitives.

### Implementation sketch

```rust
// New endpoint
pub async fn create_quote(
    pool: &PgPool,
    fx: &SimulatedFxProvider,
    body: CreateQuoteRequest,
) -> Result<QuoteResponse, AppError> {
    let rate = fx.quote(&body.source_currency, &body.destination_currency)?;
    let destination_amount = (body.source_amount * rate).round_dp(4);
    let expires_at = Utc::now() + Duration::seconds(60);

    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fx_quotes (...) VALUES (...) RETURNING id"
    )...;

    Ok(QuoteResponse { id, rate, destination_amount, expires_at })
}

// Modified create_payment: accepts optional quote_id
pub async fn create_payment(
    ...
    body: CreatePaymentRequest,  // body now has Option<Uuid> quote_id
) -> Result<CreatePaymentResponse, AppError> {
    // Inside the DB transaction, after the sender lock:
    let (quote_id, rate, destination_amount) = match body.quote_id {
        Some(qid) => {
            // Re-validate this user's quote.
            let q: FxQuoteRow = sqlx::query_as(
                "SELECT id, rate, base_amount, quote_amount, expires_at, locked_by_transaction_id
                 FROM fx_quotes WHERE id = $1 FOR UPDATE"
            ).bind(qid).fetch_optional(&mut *tx).await?
              .ok_or(AppError::BadRequest("unknown quote_id".into()))?;

            if q.expires_at < Utc::now() {
                return Err(AppError::QuoteExpired { expired_at: q.expires_at });
            }
            if q.locked_by_transaction_id.is_some() {
                return Err(AppError::BadRequest("quote already used".into()));
            }
            // Also verify the quote matches the request (currencies, amount).
            (qid, q.rate, q.quote_amount)
        }
        None => {
            // Existing behavior: fresh quote.
            let rate = fx.quote(...)?;
            let amt = (body.source_amount * rate).round_dp(4);
            let qid = insert_fx_quote(...).await?;
            (qid, rate, amt)
        }
    };
    // ... rest unchanged
}
```

Plus a new `AppError::QuoteExpired { expired_at }` variant mapping to HTTP 410.

### Tunable: the 60-second TTL

60 seconds is too short for a quote-and-confirm UX where the user reads, considers, maybe answers a 2FA prompt. Real systems use 30 seconds to several minutes depending on volatility:

| Pair | Typical TTL | Reasoning |
|------|-------------|-----------|
| Major pairs (USD/EUR) | 30-60s | Liquid market, frequent quote refresh |
| EM pairs (USD/NGN) | 60-300s | Less liquid, traders accept longer holds |
| Exotic / restricted | up to 5 min | Some are quoted hourly |

Make the TTL a per-pair configuration. Out of scope for prototype.

### Reference

- [`migrations/0001_initial_schema.sql`](../migrations/0001_initial_schema.sql) — `fx_quotes` already has `expires_at` and `locked_by_transaction_id`
- [`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs) `insert_fx_quote` — only used in atomic flow today

---

## 4B.4 — Partial settlement (recipient bank rejects after completion)

**Scenario:** The provider reports the payment as `completed`. We credit the recipient, mark `completed`. Two days later, the recipient's bank rejects the credit (closed account, fraud hold, etc.) and the funds bounce back.

### What we built

The schema has the state machine edge for this case:

```
completed → reversed   ← allowed by the trigger
```

But we haven't built the *handler* for it. The webhook processor currently dispatches on:

```rust
match payload.status.as_str() {
    "completed" => handle_completion(...).await?,
    "failed"    => handle_failure(...).await?,
    other       => { mark ignored; ... }
}
```

A `"reversed"` (or `"chargeback"`, or `"returned"`) status from the provider would land in the `other` arm and get ignored. The transaction stays `completed`, the recipient stays credited in our books, but the money never actually got to them — the ledger is now wrong.

### The ledger model for a reversal

Append-only: we add new entries with opposite signs to the previous entries, sharing the same `transaction_id`.

After completion, the ledger for transaction T has 4 entries:

| transaction_id | account                 | amount | currency | entry_type            |
|----------------|-------------------------|--------|----------|-----------------------|
| T              | sender (Lagos NGN)      | -1,000,000 | NGN | debit_sender          |
| T              | NGN clearing            | +1,000,000 | NGN | credit_clearing       |
| T              | USD clearing            | -625       | USD | debit_dest_clearing   |
| T              | Acme recipient (USD)    | +625       | USD | credit_recipient      |

Zero-sum per currency: -1,000,000 + 1,000,000 = 0 NGN; -625 + 625 = 0 USD. ✓

For a reversal, append 4 compensating entries:

| transaction_id | account                 | amount | currency | entry_type                   |
|----------------|-------------------------|--------|----------|------------------------------|
| T              | Acme recipient (USD)    | -625       | USD | reverse_credit_recipient     |
| T              | USD clearing            | +625       | USD | reverse_debit_dest_clearing  |
| T              | NGN clearing            | -1,000,000 | NGN | reverse_credit_clearing      |
| T              | sender (Lagos NGN)      | +1,000,000 | NGN | reverse_debit_sender         |

Net per currency across all 8 entries: 0 NGN, 0 USD. ✓

The recipient is debited back, the sender is re-credited, both clearings restored. The deferred zero-sum trigger validates at commit.

### The FX P&L wrinkle

Here's the financial subtlety. At completion, we converted 1M NGN to 625 USD at quote rate 0.000625. To deliver 625 USD, we drew 625 USD from clearing (which we previously funded via an FX trade at *some* rate). Two days later we're reversing:

- Recipient owes us back 625 USD. They have it (we just credited them; they can refund). But they don't — they bounced.
- We had to *settle* the 625 USD with them via the real banking system. To get the 625 USD back, we either: (a) the provider claws it back from the recipient's bank — happens automatically in some networks, possibly not in others — or (b) we eat the loss.

Meanwhile, the FX market has moved. The original 1M NGN ↔ 625 USD trade was done at a specific rate; if we now buy 625 USD to refill clearing, we pay a *different* number of NGN. That spread is **FX P&L** — could be positive or negative.

Our prototype doesn't model FX P&L (noted in [`learn/schema-design.md`](schema-design.md) §4.1). The reversal as sketched assumes the recipient's USD is recoverable and the clearings restore cleanly. In real operations:

- A separate journal entry would be created to absorb the FX P&L into a Payway P&L account.
- The reversal might be partial — the recipient's bank returns the principal but charges a fee that we have to write off.

For the prototype, we model the simple full-reversal case and leave the FX/fee complications for a real treasury accounting layer.

### Implementation sketch

```rust
// Add to payload dispatch:
"reversed" => handle_reversal(
    &mut tx,
    txn.id,
    txn.sender_account_id,
    txn.recipient_id,
    &txn.source_currency,
    txn.source_amount,
    &txn.destination_currency,
    txn.destination_amount,
    payload.failure_reason.as_deref(),
).await?,

async fn handle_reversal(
    conn: &mut PgConnection,
    transaction_id: Uuid,
    sender_account_id: Uuid,
    recipient_id: Uuid,
    source_currency: &str,
    source_amount: Decimal,
    destination_currency: &str,
    destination_amount: Decimal,
    reason: Option<&str>,
) -> Result<(), AppError> {
    let src_clearing = clearing_id(&mut *conn, source_currency).await?;
    let dst_clearing = clearing_id(&mut *conn, destination_currency).await?;
    let recipient_account = recipient_account_id(&mut *conn, recipient_id, destination_currency).await?;

    sqlx::query(
        "INSERT INTO ledger_entries
            (transaction_id, account_id, amount, currency, entry_type)
         VALUES
            -- Destination side: claw back recipient credit
            ($1, $2, $3, $4, 'reverse_credit_recipient'),
            ($1, $5, $6, $4, 'reverse_debit_dest_clearing'),
            -- Source side: refund sender
            ($1, $7, $8, $9, 'reverse_credit_clearing'),
            ($1, $10, $11, $9, 'reverse_debit_sender')"
    )
    .bind(transaction_id)
    .bind(recipient_account).bind(-destination_amount).bind(destination_currency)
    .bind(dst_clearing).bind(destination_amount)
    .bind(src_clearing).bind(-source_amount).bind(source_currency)
    .bind(sender_account_id).bind(source_amount)
    .execute(conn).await?;

    sqlx::query("UPDATE transactions SET status='reversed', failure_reason=$1 WHERE id=$2")
        .bind(reason).bind(transaction_id).execute(conn).await?;
    Ok(())
}
```

Migration to add a `reversed_at TIMESTAMPTZ` column on `transactions` (optional; can also use the `transaction_status_history` row for the reversal timestamp).

### Reference

- State machine: [`migrations/0001_initial_schema.sql`](../migrations/0001_initial_schema.sql) `enforce_transaction_status_transition` — already allows `completed → reversed`
- Append-only ledger discipline: [`learn/schema-design.md`](schema-design.md) §1.3

---

## 4B.5 — Provider timeout

**Scenario:** Our HTTP call to submit the payment to the downstream provider times out after 30 seconds. We don't know if they received it.

### What we built

Same answer as 4B.2: our "submit" is `format!("SIM-{}", uuid)` inside the DB transaction. It can't time out. The scenario doesn't apply to the prototype.

In production with a real provider, the timeout is the hardest IO problem in distributed systems: **we genuinely don't know what happened.** The request might have:

1. Never reached the provider (we should retry).
2. Reached the provider, was processed successfully, response lost in transit (we must NOT retry without idempotency, or we double-pay).
3. Reached the provider, was being processed, will eventually succeed (we should wait).
4. Reached the provider, was rejected, response lost (we should retry and discover the rejection).

We cannot tell these apart from our side.

### The correct pattern: idempotent submission + outbox

This is item #1 in [`learn/production-readiness.md`](production-readiness.md), but worth stating the mechanism here:

**1. Idempotent on the provider side.** Every submission carries our `client_reference_id` (the transaction's UUID). The provider's API contract: same `client_reference_id` → same logical request, idempotent. This is the same pattern we *built into our own* `POST /payments` for our clients — Stripe, Paystack, and most providers offer this.

So we can safely retry as many times as we want. The first submission to actually reach them creates the payment; subsequent retries return the same response.

**2. Submission is async, outside the DB transaction.**

```
T+0   create_payment: validate, idempotency claim, debit sender, COMMIT
T+1   (durable: transaction exists, status='initiated', client_reference_id known)
T+2   ... worker picks up the transaction ...
T+3   worker: POST to provider with client_reference_id
T+4     - success: UPDATE status='processing', set provider_reference, set submitted_at
T+4'    - timeout: increment submission_attempts, leave for next poll
T+4''   - permanent failure (e.g. 400): UPDATE status='failed', write reversal entries
```

The worker can retry on timeout for as long as the configured retry policy allows. Because the submission is idempotent, retries are safe.

**3. After N attempts (or T hours), escalate.**

If we still can't reach the provider after the retry budget is exhausted, we have a real outage. Two options:

- **A. Fail the payment, refund the customer.** Transition `initiated → failed` with reason `"could not reach provider; please retry"`, write reversal entries to restore the sender's balance, surface in the dashboard.
- **B. Switch to a backup provider.** Mark this submission attempt as failed for provider X, try provider Y. Requires multi-provider routing.

Both are operational decisions, not engineering decisions — they depend on SLAs, customer expectations, and backup capacity. The system needs to expose both options to the operator (a "manual fail" admin action and a "re-route" admin action).

### Why the prototype's in-transaction submission would fail here

Imagine our `create_payment` actually called a real provider in step 12. The flow:

```
T+0   BEGIN
T+1   ... validation, lock, balance check ...
T+2   insert transaction, ledger pair
T+3   provider.submit(...).await        ← 30 second timeout
T+4   timeout fires
T+5   the AppError propagates up
T+6   tx is dropped without commit → ROLLBACK
T+7   handler returns 5xx to client
```

What we know after T+7:
- Customer thinks the payment failed (5xx response) — they may retry, doubling the issue.
- DB has no record (rolled back).
- Provider may or may not have the payment — we don't know.
- Sender's balance is unchanged in our books, but their money might be in flight at the provider.

This is the worst possible state: customer-visible failure, no audit trail, possible real-world money movement we can't account for. The fix is the outbox pattern, full stop.

### What "30 seconds" means for the prototype today

Even with our simulator, the in-transaction approach is wrong in principle. We've documented this in [`learn/payments-create.md`](payments-create.md) "What production would do differently." The fix is on the production-readiness list as item #1.

### Reference

- The outbox sketch in [`learn/payments-create.md`](payments-create.md) §"What production would do differently"
- Production-readiness item #1 in [`learn/production-readiness.md`](production-readiness.md)

---

## Common thread

Three of the five scenarios (4B.2, 4B.4, 4B.5) have the same architectural fix: **decouple money movement from network IO via an outbox-style pattern**.

The other two (4B.1, 4B.3) have specific local fixes — locking and quote expiry — which we've designed for even where we haven't fully built them.

What this surfaces is that our prototype is correct *in the small* (per-transaction integrity, per-account locking, idempotency) but incomplete *in the large* (asynchronous provider integration, multi-step flows). The schema and trigger work supports the larger patterns; the application code needs the outbox layer added on top. That's the agenda for production.
