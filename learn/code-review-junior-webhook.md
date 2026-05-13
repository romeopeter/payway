# Code Review — Junior Webhook Handler (Spec Part 4A)

> **What this is for:** the spec asks us to review a webhook handler written by a junior developer and identify every issue. For each issue, the spec wants: what the problem is, why it matters specifically in a payments context (not just "best practice"), and how to fix it. We have a working implementation in [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) and [`backend/src/routes/webhooks.rs`](../backend/src/routes/webhooks.rs) — the fixes here reference that code so you can see the contrast.

---

## The code being reviewed

```javascript
app.post('/webhooks/payment-provider', async (req, res) => {
  const payload = req.body;
  const signature = req.headers['x-webhook-signature'];

  if (!signature) {
    return res.status(401).send('Missing signature');
  }

  const transaction = await db.query(
    'SELECT * FROM transactions WHERE provider_reference = $1',
    [payload.reference]
  );

  if (!transaction.rows[0]) {
    return res.status(404).send('Transaction not found');
  }

  if (payload.status === 'completed') {
    await db.query(
      'UPDATE transactions SET status = $1, completed_at = NOW() WHERE id = $2',
      ['completed', transaction.rows[0].id]
    );
    await db.query(
      'UPDATE accounts SET balance = balance + $1 WHERE id = $2',
      [payload.amount, transaction.rows[0].recipient_account_id]
    );
  } else if (payload.status === 'failed') {
    await db.query(
      'UPDATE transactions SET status = $1 WHERE id = $2',
      ['failed', transaction.rows[0].id]
    );
    await db.query(
      'UPDATE accounts SET balance = balance + $1 WHERE id = $2',
      [transaction.rows[0].amount, transaction.rows[0].sender_account_id]
    );
  }

  res.status(200).send('OK');
});
```

This handler has 15 distinct issues. I'll group them by severity, leading with the ones that allow money to be created or stolen.

---

## CRITICAL: Money-loss and money-creation bugs

### 1. The signature is never actually verified

**What:** The handler reads `x-webhook-signature` from the headers and checks only that it is *present*. There is no HMAC computation. An attacker who can reach the endpoint can send any payload with any (or even any garbage) value in that header, and the code happily proceeds.

**Why it matters in payments:** Anyone on the internet can claim "payment X completed" and the recipient gets credited. Anyone can claim "payment Y failed" and the sender gets re-credited *while the original recipient also still got paid by the real provider*. This is the single most expensive bug in webhook code, and it's the most common.

**Fix:** Compute `HMAC-SHA256(secret, raw_body_bytes)`, compare against the header in **constant time** (do not use `==`). Reject (with 200 + log, per our design — see issue #3) anything that fails. Our [`backend/src/webhook_signature.rs`](../backend/src/webhook_signature.rs) does this via `Mac::verify_slice`. See [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) for the details.

### 2. No idempotency — duplicate webhooks double-credit

**What:** The provider retries on any non-2xx response, and even on 2xx if their ack timeout fires. The same event can arrive 2, 5, 50 times. This handler has no way to recognize a retry: it'll re-run the UPDATE and the balance modification every time. Even when the second UPDATE is a no-op (status already `completed`), the balance `UPDATE ... SET balance = balance + amount` still fires and adds money.

**Why it matters in payments:** Provider retries are routine — they happen on every brief network hiccup. Within hours of going live, a real version of this code would have multiple-credited several customers. Reversing those credits is operationally expensive and the audit trail is poor.

**Fix:** The webhook payload must include an `event_id`. Use a unique index on `(provider, event_id)` and `INSERT ... ON CONFLICT DO NOTHING` — the database is the only place this can be enforced safely. Our [`domain/webhooks.rs`](../backend/src/domain/webhooks.rs) does this with the `webhook_events` table; see [`learn/concepts/idempotency.md`](concepts/idempotency.md) for why application-level dedup is racy.

### 3. `404` for unknown reference — operational + security failure

**What:** When `provider_reference` doesn't match any transaction, the handler returns `404 Not Found`. Two problems:

**Why it matters operationally:** Providers retry indefinitely on non-2xx. A single misrouted webhook becomes infinite retries; many of them become a DDoS on your endpoint.

**Why it matters for security:** Response codes leak existence. An attacker (or curious party) can probe with arbitrary reference strings and learn which transactions exist on your system from the response code. That's an enumeration oracle, useful for follow-on attacks.

**Fix:** Return `200 OK` always — for unknown references, for invalid signatures, for malformed bodies. Log everything to `webhook_events` so the audit trail is complete. Distinguishing outcomes is what `processing_status` is for: `processed | ignored | failed`. See [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) "Why always 200" and the spec Part 2 §`POST /webhooks/provider` requirement.

### 4. `UPDATE accounts SET balance = balance + $1` violates double-entry

**What:** The handler mutates a `balance` column directly. The matching counter-entry — debiting the source clearing on completion, debiting clearing on reversal — is missing entirely.

**Why it matters in payments:** Double-entry is the integrity invariant of any financial ledger: every credit has a corresponding debit; the system never creates or destroys value. Without it, you cannot answer "where did this money come from?" or "is our balance sheet correct?" When something goes wrong (and it will), there's no audit trail to reconcile against. Regulators ask for double-entry; auditors ask for double-entry; production payments engineers will not work without it.

**Fix:** Append-only signed-amount entries in a `ledger_entries` table, with a constraint that every transaction's entries sum to zero per currency. Our schema enforces this with a deferred trigger; see [`learn/schema-design.md`](schema-design.md) §1.3.

For *completion*, write two entries in the destination currency:
```
(tx_id, dest_clearing,         -destination_amount, dest_ccy, 'debit_dest_clearing')
(tx_id, recipient_external,    +destination_amount, dest_ccy, 'credit_recipient')
```

For *failure*, write reversal entries in the source currency that net the original debit:
```
(tx_id, sender,        +source_amount, src_ccy, 'reverse_debit_sender')
(tx_id, src_clearing,  -source_amount, src_ccy, 'reverse_credit_clearing')
```

See [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) `handle_completion` and `handle_failure`.

### 5. No DB transaction wrapping the two UPDATEs

**What:** Each `db.query` runs as its own autocommit. There are two writes per branch: the status UPDATE and the balance UPDATE. If the second fails (network blip, constraint violation, anything), the first has already committed.

**Why it matters in payments:** A `completed` status with no matching credit means the customer sees "your payment succeeded" but the recipient never got the money. Conversely, a `failed` status without the reversal means the sender's debit stands but the failure is marked — they've lost their money. Either state is a customer-facing incident.

**Fix:** Wrap all writes in `BEGIN ... COMMIT`. If anything fails, the whole thing rolls back. Our service function in [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) opens `pool.begin()` once and commits at the end; partial states are impossible.

### 6. No state machine guard — late webhooks corrupt state

**What:** The handler doesn't check the transaction's current status before updating it. A late `completed` webhook for an already-completed transaction would re-run the UPDATE (no-op for status) AND re-run the balance UPDATE (re-credits the recipient). A `failed` webhook arriving after `completed` would mark the transaction `failed` AND re-credit the sender — leaving both the recipient and the sender with the funds.

**Why it matters in payments:** Real providers occasionally send late or out-of-order events. Without a state guard, those events corrupt the ledger. This is essentially "double-spend in reverse" — the system is generating money from nothing.

**Fix:** Check the transaction's status with `SELECT ... FOR UPDATE` (the lock is also necessary; see issue #7), and reject anything that isn't in the expected source state (`processing`). Our `domain/webhooks.rs` does this and marks the webhook `ignored` with a reason. The DB-level state machine trigger from [`migrations/0001_initial_schema.sql`](../migrations/0001_initial_schema.sql) is a second line of defense — it'll reject impossible transitions even if the application forgets.

### 7. Race condition: no row lock on the transaction during update

**What:** `SELECT * FROM transactions WHERE provider_reference = $1` reads without any lock. If two webhooks for the same payment arrive concurrently, both can read the same `processing` row, both proceed, both update — same race as the double-spend in [`learn/concepts/double-spend.md`](concepts/double-spend.md), but on the webhook side.

**Why it matters in payments:** Concurrent webhooks are not hypothetical. Providers fire retries; load balancers replay requests; humans manually re-trigger events from dashboards. Without the lock, the same payment can complete twice.

**Fix:** `SELECT ... FROM transactions WHERE provider_reference = $1 FOR UPDATE`. The row lock serializes concurrent handlers for the same payment. Combined with the state machine guard (#6), this makes the handler safe under arbitrary concurrent delivery.

---

## SERIOUS: Audit, observability, and trust

### 8. The raw webhook event is never logged

**What:** No `webhook_events` table, no row written, no record kept. If processing fails, there's no way to replay; if the provider disputes "we sent you event X" there's no evidence; if a bug is found and the fix needs to be applied to historic events, there's nothing to apply it to.

**Why it matters in payments:** Disputes and reconciliation are routine in real operations. "We never received that webhook" is a common provider claim — you need the raw bytes, the timestamp, the signature to refute it. Auditors will ask for the inbound event log specifically.

**Fix:** INSERT the raw payload (as `BYTEA`), headers, signature, and `signature_valid` flag *before* any business logic runs. Our `webhook_events` table is designed exactly for this; see [`learn/schema-design.md`](schema-design.md) §1.6 and §1 of [`learn/webhooks.md`](webhooks.md).

### 9. `payload.amount` is trusted

**What:** The completion branch credits `payload.amount` — i.e., whatever amount the wire sent. There's no check that it matches what we quoted, what we debited, or what we expected.

**Why it matters in payments:** Even with a real signature, a buggy provider might send a different amount than the actual settlement. Real-world providers have shipped exactly this bug (sending the wrong currency's amount, sending pre-fee vs post-fee, sending the source amount in the destination field). If the handler trusts the wire blindly, the recipient gets the wrong amount and the ledger is inconsistent with the original quote.

**Fix:** Credit the recipient with `transactions.destination_amount` (what we quoted and agreed to deliver), not `payload.amount`. The webhook tells us *what happened*, not *how much*. Our `domain/webhooks.rs` does this — `payload.amount` doesn't even appear in our schema or types.

### 10. Wrong amount field for the failure reversal

**What:** The failure branch uses `transaction.rows[0].amount` for the reversal — but `transactions` in any realistic schema has separate `source_amount` and `destination_amount`. There's no single `amount`. The bug is "code that compiles against an imagined schema" — but if there is an `amount` column and it happens to be the destination amount, the failure reversal credits the sender with the wrong (FX-converted, destination-currency) amount.

**Why it matters in payments:** A reversal that doesn't restore the original debit isn't a reversal — it's a different transaction in a different currency. The sender ends up with the wrong balance and the source clearing account is now inconsistent. Fixing this manually requires accounting team intervention.

**Fix:** Reverse `source_amount` in the `source_currency`. Our `handle_failure` is explicit about this.

### 11. No currency awareness

**What:** `UPDATE accounts SET balance = balance + $1` assumes one balance per account, no currency dimension. There's no way to know what currency we're adjusting. A USD payment crediting a multi-currency "account" is ambiguous; the schema can't represent the answer.

**Why it matters in payments:** Multi-currency is the entire premise of a cross-border product. A schema that doesn't model currency on the ledger row guarantees you'll mix currencies and miss the error. The ledger sum across currencies is meaningless; reports become unreliable.

**Fix:** One account per `(owner, currency, type)`; every ledger entry has a `currency` column; the zero-sum invariant is per-currency. See [`learn/schema-design.md`](schema-design.md) §1.1 and the FK from `ledger_entries.currency` to `currencies.code` (§1.9).

---

## NOTABLE: Things that aren't disasters but are wrong

### 12. `401 Unauthorized` for missing signature

**What:** Beyond the security issue of *not actually verifying* (#1), returning 401 for missing signature also leaks that signatures are required and that a particular header is the trigger. Combined with always-200 elsewhere, this becomes an inconsistent contract.

**Why it matters in payments:** Same operational reasoning as #3 — 401 means infinite retries. And the signature *should* be required, not optional; returning 401 vs 200 conveys policy information to anyone probing.

**Fix:** Return 200; log with `signature_valid = false`.

### 13. JSON parsed before signature verification — even if signature were verified

**What:** `req.body` is already-parsed JSON by the time the handler runs. Even if the handler computed an HMAC, it'd compute it over re-serialized bytes that don't match what the provider signed (see [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) "Why HMAC over raw bytes").

**Why it matters in payments:** Same as #1, but applies as soon as anyone fixes #1 with the obvious-looking-but-wrong code. The retrofit "let me just verify the parsed body" still gets you a broken signature check that rejects valid signatures.

**Fix:** Use the raw body bytes extractor (Axum's `Bytes` in our stack; Express's `express.raw()` middleware or `req.rawBody` in Node depending on setup) and pass the raw bytes to the HMAC verifier. Parse *after* verification.

### 14. No `event_id` in the payload contract

**What:** The handler dispatches on `status` and `reference` only. There's no event identifier. The provider may or may not send one; this handler couldn't dedupe even if it had one because there's no place to look it up.

**Why it matters in payments:** Without an event_id you cannot dedupe; without dedup you double-credit. The handler design needs to commit to a payload schema that includes event_id, the provider needs to commit to populating it, and the database needs an index on it. All three are required.

**Fix:** Define a payload contract that includes `event_id`; unique-index `(provider, provider_event_id)`. See [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs)'s `WebhookPayload` struct.

### 15. Silent failure on unknown `payload.status`

**What:** The `if/else if` branches handle `completed` and `failed`. Anything else — `pending`, `disputed`, `chargeback`, or a typo — falls through to `res.status(200).send('OK')` with no action and no log. The provider thinks we acknowledged the event; we have no record of receiving it.

**Why it matters in payments:** Providers add new statuses over time. The first time the provider sends `chargeback`, this handler silently drops it. We discover the problem when the dispute lands in an inbox.

**Fix:** Explicit branch for unrecognized status; log it as `ignored` with the offending value; surface in monitoring. Our `domain/webhooks.rs` does this — the `_ => { mark_webhook ... }` arm.

---

## Summary

15 issues across one ~30-line handler. Five of them allow money to be created from nothing (#1, #2, #4, #6, #9). Two corrupt state under realistic conditions (#5, #7). The rest are operational and audit failures that compound over time.

The most damning observation: every individual issue here is *easy* to fix in isolation. The handler is broken not because any one piece is hard, but because shipping a webhook handler that's safe requires holding several invariants in your head simultaneously — and forgetting any one is enough.

That's the broader lesson: payment systems aren't hard because the algorithms are clever. They're hard because the correctness conditions are mostly invisible until they break. Code review is part of the answer; database-enforced invariants (signature verification, append-only ledger, unique indexes, state machine triggers) are most of the rest.

---

## Cross-references

- [`backend/src/domain/webhooks.rs`](../backend/src/domain/webhooks.rs) — our implementation; each fix above maps to a piece of this file
- [`backend/src/webhook_signature.rs`](../backend/src/webhook_signature.rs) — HMAC primitive (fixes #1, #13)
- [`learn/concepts/webhook-security.md`](concepts/webhook-security.md) — HMAC, raw bytes, constant-time, always-200 (fixes #1, #3, #4, #13)
- [`learn/concepts/idempotency.md`](concepts/idempotency.md) — dedup pattern (fixes #2)
- [`learn/concepts/double-spend.md`](concepts/double-spend.md) — FOR UPDATE pattern (fixes #7)
- [`learn/schema-design.md`](schema-design.md) §1.3 (double-entry), §1.6 (webhook log), §1.1+§1.9 (currency modeling)
