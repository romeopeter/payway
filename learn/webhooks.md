# `POST /webhooks/provider` — walkthrough

> **What this is for:** a guided read of the webhook processor we built in 2c. Open [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) alongside this — the section numbers here match the code's `Step 1..6` comments. After reading, you should be able to explain why each step is in this order, what the state machine is doing, and how reversal entries preserve double-entry invariants.

---

## What the endpoint does

`POST /webhooks/provider` receives status updates from the downstream payment provider for payments we previously submitted. Two real outcomes are interesting:

- **`completed`** — the recipient's bank accepted the credit. We finish the settlement on our books: credit the recipient's external account in the destination currency, debit our destination clearing.
- **`failed`** — the payment didn't land. We reverse the sender's debit via *new* append-only ledger entries that compensate the original.

Three more outcomes don't change the books but still get logged:

- Invalid or missing signature → log, return 200, ignore.
- Unknown `provider_reference` → log, return 200, ignore. (See "Why always 200" below.)
- Webhook for a transaction not in `processing` (late dupe, manual override, etc.) → log, return 200, ignore.

The handler always returns **`200 OK`** for any of the above. Only unexpected infrastructure failures (DB unreachable) bubble up as 5xx, which is the signal the provider needs to retry.

---

## The handler is even thinner than 2b

[`backend/src/routes/webhooks.rs`](../backend/src/routes/webhooks.rs) is ~25 lines:

1. Extract `X-Webhook-Signature` header (if any).
2. Extract body as **`Bytes`** — *not* `Json<T>`. The HMAC is computed over the bytes the provider sent; if we parse and re-serialize first, whitespace and key order can break the signature. See [`learn/concepts/webhook-security.md`](concepts/webhook-security.md).
3. Call `webhooks::process(pool, secret, provider, &body, signature)`.
4. Map any `Ok(ProcessingResult)` to `200 OK`. Only `Err(AppError)` becomes 5xx.

All business logic lives in [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs).

---

## The service flow

All work runs inside one DB transaction (`let mut tx = pool.begin().await?`). Several "early exit" paths commit the partial state (just the `webhook_events` log row) before returning — that's intentional, since we want the audit trail to be durable even on the unhappy paths.

### Step 1 — Signature check

```rust
let signature_valid = match signature {
    Some(sig) => verify_hmac_sha256(secret.as_bytes(), raw_body, sig).is_ok(),
    None => false,
};
```

The verifier is in [`backend/src/webhook_signature.rs`](../backend/src/webhook_signature.rs), a deliberately separate module so the security-sensitive primitive is easy to find and unit-test. It uses `hmac::Mac::verify_slice`, which **compares in constant time** — see the concept doc for why `==` on byte slices is wrong here.

If invalid (or absent): insert a `webhook_events` row with `signature_valid = false`, `processing_status = 'ignored'`, commit, return `Ignored`. The provider gets 200; we have evidence.

### Step 2 — Parse JSON

Only after signature passes do we trust the body enough to deserialize. If parsing fails, the raw bytes are still the source of truth — we log them verbatim with `processing_status = 'failed'` and `processing_error = "malformed JSON: ..."` so a replay or postmortem can use them.

### Step 3 — Dedup via the unique index

```sql
INSERT INTO webhook_events (..., processing_status)
VALUES (..., 'pending')
ON CONFLICT (provider, provider_event_id) WHERE provider_event_id IS NOT NULL
DO NOTHING
RETURNING id
```

Same idea as idempotency in 2b. Two simultaneous deliveries of the same event ID can't both win — the unique index serializes them, one inserts, the other gets nothing back from `RETURNING`. The "loser" returns `Duplicate` without processing.

This is why **`(provider, provider_event_id)` not just `provider_event_id`** — different providers might collide on event id strings.

The partial-index `WHERE provider_event_id IS NOT NULL` matters because some webhook rows (invalid sig, malformed body) have NULL event_id and shouldn't participate in dedup. The ON CONFLICT clause has to repeat the same predicate so Postgres uses the right index.

### Step 4 — Look up the transaction with `FOR UPDATE`

```sql
SELECT ... FROM transactions WHERE provider_reference = $1 FOR UPDATE
```

Two reasons for the lock:

1. We're about to update `status` and write ledger entries. A concurrent reversal (Part 4B.4 will introduce this) or another webhook delivery would race us without a lock.
2. The state machine trigger in Postgres rejects invalid transitions, but the *check* of "is this transaction still in `processing`" is racy without the lock. With the lock, we read-then-write atomically.

If the row doesn't exist → unknown `provider_reference`. Log with `'ignored'`, commit, return `Ignored`. **No 404.** See "Why always 200" below.

### Step 5 — State machine guard

```rust
if txn.status != "processing" {
    mark_webhook(&mut tx, ..., "ignored", ...).await?;
    return Ok(ProcessingResult::Ignored);
}
```

This catches the "late webhook for an already-settled payment" case — `completed → completed` would technically be a no-op for the trigger (which has `IF OLD.status = NEW.status THEN RETURN NEW`), but we'd still execute the ledger writes if we didn't check. **Catching it here prevents double-credit.**

This is the second layer of idempotency: the unique-index dedup catches duplicates of the *same event_id*; this catches duplicates of the *same payment outcome* delivered via different event_ids. Both are real things providers do.

### Step 6 — Dispatch on `status`

**`completed`** → `handle_completion`:
```sql
-- New ledger entries in the DESTINATION currency:
INSERT INTO ledger_entries VALUES
  ($tx_id, $dest_clearing, -$destination_amount, $destination_currency, 'debit_dest_clearing'),
  ($tx_id, $recipient_ext, +$destination_amount, $destination_currency, 'credit_recipient');

UPDATE transactions SET status = 'completed', completed_at = NOW() WHERE id = $tx_id;
```

The recipient's external account is **get-or-created** via:
```sql
INSERT INTO accounts (...)
ON CONFLICT (owner_recipient_id, currency) WHERE owner_recipient_id IS NOT NULL
DO UPDATE SET display_name = accounts.display_name  -- no-op
RETURNING id
```

The "no-op UPDATE" trick is the cleanest way to get a stable `RETURNING id` from both the insert and conflict cases without a second roundtrip. Without DO UPDATE, `DO NOTHING` skips RETURNING on conflict.

The ORIGINAL source-currency entries from `create_payment` stay where they are. The sender's debit is **permanent** on completion — we don't re-credit them. Economically, Payway received NGN and delivered USD; that's a real FX trade. Tracking the FX P&L explicitly is out of scope here (noted in [`learn/schema-design.md`](schema-design.md) §4.1).

**`failed`** → `handle_failure`:
```sql
-- New ledger entries in the SOURCE currency, opposite signs:
INSERT INTO ledger_entries VALUES
  ($tx_id, $sender,        +$source_amount, $source_currency, 'reverse_debit_sender'),
  ($tx_id, $src_clearing,  -$source_amount, $source_currency, 'reverse_credit_clearing');

UPDATE transactions SET status = 'failed', failed_at = NOW(), failure_reason = $1 WHERE id = $tx_id;
```

Append-only: we don't UPDATE the original debit; we add new entries with opposite signs. Both groups (original pair + reversal pair) sum to zero per `(transaction_id, currency)`, so the deferred zero-sum trigger is happy. The total ledger sum for this transaction is still zero.

The `entry_type` distinguishes the original entries from the reversal entries — useful for the timeline view in the dashboard and for any reconciliation tool.

**Unknown status** → log `'ignored'` with the offending value, return. Providers occasionally invent statuses; we don't crash.

---

## Why always `200`

The spec asks us to explain this. Two reasons:

**Operational.** Payment providers retry indefinitely on any non-2xx response. A 404 for an unknown reference means *infinite* retries — a single misrouted webhook can saturate the endpoint. We want providers to stop retrying once we've received and acknowledged a message; the appropriate signal for "we've received it" is 200, regardless of whether it was actionable.

**Security.** Response codes leak existence. A 404 for an unknown reference is an enumeration oracle: an attacker can probe `/webhooks/provider` with arbitrary reference strings and learn from response codes which transactions exist. Always-200 closes that side channel; the result is uniform from the attacker's view.

**What we return 200 for:** valid + processed; valid + already processed (duplicate); valid + transaction not in processing; unknown reference; malformed JSON; invalid signature; missing signature.

**What still returns 5xx:** infrastructure failures we can't recover from (DB unreachable). Letting the provider retry these is correct — they're transient.

This is documented in [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) and reinforced by every test in [`backend/tests/webhooks_provider.rs`](../backend/tests/webhooks_provider.rs).

---

## What's tested

[`backend/tests/webhooks_provider.rs`](../backend/tests/webhooks_provider.rs) covers:

| Test | What it verifies |
|------|------------------|
| `completion_credits_recipient_and_marks_completed` | Status flips, recipient external account credited, dest clearing debited |
| `failure_reverses_sender_debit` | Status flips to `failed`, reversal entries restore sender balance, no recipient writes |
| `invalid_signature_logs_and_returns_ignored` | 200, `signature_valid=false` logged, payment untouched |
| `duplicate_event_id_does_not_double_credit` | Second delivery returns `Duplicate`, no extra ledger entries |
| `unknown_provider_reference_is_logged_and_ignored` | 200, webhook logged as `ignored` with reason |
| `second_completion_for_completed_payment_is_ignored` | Different event_ids, same payment: only one credit_recipient entry |
| `malformed_json_is_logged_with_failed_status` | Raw bytes preserved in webhook_events with processing_error |

Run them:
```bash
cd backend
DATABASE_URL=postgres://payway:payway_local_dev@localhost:5432/payway \
  cargo test --test webhooks_provider
```

There are also unit tests inside [`backend/src/webhook_signature.rs`](../backend/src/webhook_signature.rs) for the HMAC primitive — verifying that flipped hex, wrong secret, and tampered body all reject.

---

## Cross-references

- [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) — implementation
- [`backend/src/routes/webhooks.rs`](../backend/src/routes/webhooks.rs) — thin handler
- [`backend/src/webhook_signature.rs`](../backend/src/webhook_signature.rs) — HMAC primitive with unit tests
- [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) — HMAC, raw bytes, constant time, "why always 200" in depth
- [`learn/code-review-junior-webhook.md`](code-review-junior-webhook.md) — Part 4A: critique of the junior dev's handler, with cross-refs to our implementation
- [`learn/payments-create.md`](payments-create.md) §"What production would do differently" — the outbox pattern that would change how 2b interacts with the provider (and would have the same downstream effects on this webhook flow)
