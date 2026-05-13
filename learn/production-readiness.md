# Production Readiness (Spec Part 4C)

> **What this is for:** the spec asks for the 5 most critical things I'd add or change before deploying this prototype with real money. For each: why it matters, what to actually build (not "add monitoring"), and what failure mode it prevents. The list is ordered by criticality — #1 first.

These are the five I'd insist on. There are many more "would be nice" items; I'm naming the ones whose absence would actively *destroy money or trust*.

---

## 1. Outbox pattern for provider submission

### Why it matters

As built today, [`create_payment`](../backend/src/domain/payments.rs) calls the provider *inside* its DB transaction. That works for the simulator (synchronous in-memory). With a real provider, every payment creation pins:

- A row lock on the sender's `accounts` row, for the duration of the network call.
- A DB connection from the pool (we have 20), for the same duration.
- A Postgres MVCC snapshot (long-running transactions cause bloat and impede VACUUM).

A provider call typically takes 200ms–2s in normal conditions. Under stress or partial outage, it can run to the 30s timeout. During that 30 seconds:

- Every other payment attempt from the same sender blocks on the row lock.
- One out of 20 DB connections is held doing nothing useful.
- The `accounts` row's version chain grows.

Worse, on timeout, we don't know whether the provider received the request. If we roll back our DB transaction (which Rust will do automatically when the function returns an error), we've lost the audit record while the provider may still be processing.

### What failure mode it prevents

1. **Lost transactions** — DB rolls back on timeout; provider still has the payment in flight. We have no record. Customer is debited at the provider, no debit on our books.
2. **Pool exhaustion under provider degradation** — slow provider = held DB connections = our entire service goes down.
3. **Customer-side double-spend on retry** — customer gets 500, retries with same idempotency key; if first attempt actually did succeed at the provider but rolled back here, the retry creates a duplicate.

### Implementation

Two changes:

**(a) Mint our own `client_reference_id` and store it before any network call.** A migration adds:

```sql
ALTER TABLE transactions ADD COLUMN client_reference_id UUID NOT NULL DEFAULT gen_random_uuid();
CREATE UNIQUE INDEX transactions_client_reference_id_unique ON transactions (client_reference_id);
```

The `create_payment` flow commits with `status='initiated'`, `client_reference_id` set, `provider_reference` and `submitted_at` NULL. No provider call.

**(b) Submission worker.** A separate tokio task (or sidecar process) polls:

```sql
UPDATE transactions
SET    submission_attempts = submission_attempts + 1,
       submission_locked_at = NOW()
WHERE  id IN (
  SELECT id FROM transactions
  WHERE  status = 'initiated'
    AND  (submission_locked_at IS NULL OR submission_locked_at < NOW() - INTERVAL '1 minute')
    AND  submission_attempts < 10
  ORDER BY initiated_at
  LIMIT 50
  FOR UPDATE SKIP LOCKED
)
RETURNING id, client_reference_id, sender_account_id, ...;
```

The `FOR UPDATE SKIP LOCKED` makes the worker safe to scale horizontally — multiple worker instances claim disjoint batches without contention.

For each row: call the provider with our `client_reference_id`. On success → update status to `processing`, set `provider_reference`, `submitted_at`. On 4xx → fail definitively, write reversal ledger entries. On 5xx/timeout → leave for next poll cycle (the `submission_locked_at` lease expires, picked up again).

After `submission_attempts >= 10`, the worker stops trying and surfaces in a dashboard alert. An operator decides: manually mark failed (write reversal), retry with adjusted parameters, or switch providers.

**Idempotency guarantee at the provider:** every retry sends the same `client_reference_id`. Real providers (Stripe, Paystack, Adyen) all deduplicate on this field. Even if the network swallowed our request mid-flight, the next retry doesn't create a duplicate at the provider.

Tradeoff: status now transitions through `initiated` (debited, not yet submitted) before reaching `processing` (submitted, awaiting webhook). The frontend timeline view shows this. Operators must accept that "initiated for >1 minute" is a normal transient state, not an alert condition — only "initiated for >5 minutes" or "submission_attempts >= 5" deserves attention.

---

## 2. Authentication and authorization

### Why it matters

Today, anyone who can reach `:8080` can:

- `POST /payments` from `Lagos Imports' account` to any recipient
- `GET /payments/<id>` for any transaction in the system
- `GET /payments` to enumerate everyone's payment history

There is no caller identity. The `sender_account_id` is taken at face value from the request body. The frontend currently has no way to scope queries to "my transactions" — and even if it tried, the backend would honor any request.

For a payments system, this is not a "polish it later" issue. It's the difference between a private API and a public-internet handoff anyone can drive.

### What failure mode it prevents

1. **Unauthorized payment creation** — adversary creates payments from any business's account.
2. **Information disclosure** — adversary enumerates `GET /payments` and learns every business's transaction history (amounts, recipients, routing).
3. **Cross-tenant data leakage** — once we have more than one business client, no way to keep their data separate.

### Implementation

The cheapest credible auth for B2B APIs is **API keys**, scoped per business entity.

**(a) Schema additions:**

```sql
CREATE TABLE api_keys (
  id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  business_id     UUID NOT NULL REFERENCES business_entities(id),
  -- bcrypt or argon2 hash of the key; we never store plaintext after issuance
  key_hash        TEXT NOT NULL UNIQUE,
  -- public prefix (e.g. "pwk_live_") so we can identify keys in logs and
  -- the user can recognize their key in a UI without revealing the secret
  key_prefix      TEXT NOT NULL,
  scopes          TEXT[] NOT NULL DEFAULT ARRAY['payments:write', 'payments:read'],
  created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  last_used_at    TIMESTAMPTZ,
  revoked_at      TIMESTAMPTZ
);
```

Keys are minted via an admin endpoint (or initial provisioning script). The plaintext is shown *once*, never stored. The hash is stored.

**(b) Middleware:**

```rust
async fn require_api_key(
    State(state): State<AppState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let key = extract_bearer_token(&request)?;
    let hash = hash_key(&key);
    let row: Option<(Uuid, Vec<String>)> = sqlx::query_as(
        "SELECT business_id, scopes FROM api_keys WHERE key_hash = $1 AND revoked_at IS NULL"
    ).bind(&hash).fetch_optional(&state.pool).await?;

    let (business_id, scopes) = row.ok_or(AppError::Unauthorized)?;
    request.extensions_mut().insert(Caller { business_id, scopes });
    Ok(next.run(request).await)
}
```

Mounted on every route except `/health` and `/webhooks/provider`. The webhook is authenticated separately by HMAC.

**(c) Authorization at the handler:**

```rust
async fn create(
    State(state): State<AppState>,
    Extension(caller): Extension<Caller>,
    ...
) -> ... {
    require_scope(&caller, "payments:write")?;
    // Verify the sender account belongs to the caller's business
    let sender = sqlx::query!(...).bind(body.sender_account_id);
    if sender.owner_business_id != caller.business_id {
        return Err(AppError::Forbidden);
    }
    ...
}
```

`GET /payments` adds a `WHERE` filter on `accounts.owner_business_id = $caller_business`. Same for `GET /payments/:id` — 404 (not 403) if it belongs to another business, to avoid leaking existence.

### Why API keys and not OAuth

OAuth 2.0 client credentials is more "enterprise" but requires a token endpoint, expiry, refresh logic, and complicates client integration. API keys are simpler, mature in fintech (Stripe, Paystack, Plaid all use them for backend-to-backend), and easy to rotate (revoke old key, generate new). For an internal-tools or B2B API, this is the right complexity level. OAuth/JWT is worth it when there are end-users with their own identities — we're machine-to-machine.

---

## 3. Reconciliation against bank statements

### Why it matters

Our ledger says we have 100M NGN of customer money. Does the bank?

Today, we have no way to know. The ledger is a record of what we *should* be holding based on payments we've processed. The actual money lives at our settlement bank (or banks), and we don't compare.

When they diverge — and they will, sometimes — it's because:

- A payment was credited at the bank but our system didn't process the webhook (e.g., outage).
- A bank-side fee was deducted that we didn't book.
- A fraud chargeback hit the bank account that we haven't been notified about yet.
- A bank operations error (rare but happens — wires sent twice, miscounted).
- A real fraud — money moving out without our system authorizing it.

Without reconciliation, you don't notice these. You operate believing your books are correct, until eventually the bank tells you you're overdrawn or until an auditor finds the discrepancy. Either is a major incident.

### What failure mode it prevents

1. **Silent insolvency** — you think you have customer funds; you don't. You're operating on credit without realizing.
2. **Undetected fraud** — money leaves the bank account without a matching ledger entry. With reconciliation it shows up the next morning; without, it goes unnoticed.
3. **Audit failure** — every regulator and serious enterprise customer will ask "how do you reconcile?" — "we don't" is not an acceptable answer.

### Implementation

A daily back-office workflow:

**(a) Statement ingestion.** Each settlement bank provides daily statements — typically via SWIFT MT940 (legacy), MT942 (intraday), or a banking API (modern). Parse and store into:

```sql
CREATE TABLE bank_statement_lines (
  id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  bank_account_id      TEXT NOT NULL,  -- our identifier for the nostro account at the settlement bank
  statement_date       DATE NOT NULL,
  posted_at            TIMESTAMPTZ NOT NULL,
  amount               NUMERIC(20, 4) NOT NULL,  -- signed: positive credit, negative debit
  currency             CHAR(3) NOT NULL,
  reference            TEXT,    -- bank's reference number
  remitter_info        TEXT,
  raw_record           JSONB NOT NULL,  -- the original parsed record
  ingested_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  reconciled_to        UUID REFERENCES ledger_entries(id),  -- set when matched
  reconciliation_notes TEXT
);
```

**(b) Matching job.** For each unmatched statement line, find candidate ledger entries by (amount, currency, +/- 1 day from posted_at, similar reference). Auto-match when exactly one ledger entry matches; queue for manual review otherwise.

```sql
-- Auto-match candidates
SELECT le.id, bsl.id
FROM bank_statement_lines bsl
JOIN ledger_entries le
  ON  ABS(le.amount) = ABS(bsl.amount)
  AND le.currency    = bsl.currency
  AND le.created_at  BETWEEN bsl.posted_at - INTERVAL '1 day' AND bsl.posted_at + INTERVAL '1 day'
WHERE bsl.reconciled_to IS NULL
  AND le.id NOT IN (SELECT reconciled_to FROM bank_statement_lines WHERE reconciled_to IS NOT NULL)
GROUP BY bsl.id, le.id
HAVING COUNT(*) = 1;
```

**(c) Discrepancy dashboard.** Surface:
- Statement lines with no matching ledger entry (we received money we didn't expect, or sent money we didn't record).
- Ledger entries with no matching statement line older than N days (we recorded a movement that didn't actually happen at the bank).
- Quantity and total value of discrepancies, with threshold alerts.

**(d) Manual reconciliation journal entries.** When an operator decides "yes, that bank-side fee is legitimate, book it," they create a journal entry (using our `journal_id` mechanism from [`learn/schema-design.md`](schema-design.md) §1.8) that records the adjustment, with the bank statement line as evidence.

This is a substantial back-office system, not just an engineering task — it requires a daily ops person to review the discrepancy queue. But the engineering work is finite and high-leverage.

---

## 4. Observability with invariant alerting

### Why it matters

When a payments bug ships, the symptoms are often quiet and slow: one customer's balance off by pennies, gradual drift in a clearing account, a webhook that processed but with the wrong amount. These don't trigger 500 errors or load balancer alarms. They surface as customer complaints — by which point the bug has been replicating for hours or days, and trust is already damaged.

Observability is what makes time-to-detection acceptable.

### What failure mode it prevents

1. **Silent correctness drift** — a code change subtly miscalculates an amount; no error, just wrong numbers. With invariant alerting, the next ledger imbalance fires within minutes.
2. **Late discovery of integrations failures** — webhook processing falls behind; transactions stuck in `processing`. Without alerting, you don't know until a customer asks "where's my money."
3. **Performance regressions invisible until they cause incidents** — a query that gradually slowed from 50ms to 5s ships unnoticed until traffic spikes.

### Implementation

Four layers, in priority order:

**(a) Structured logs with correlation IDs.** Every log line includes `request_id`, `transaction_id`, `account_id`, `currency` (where relevant). [`tracing-subscriber`](https://docs.rs/tracing-subscriber) with JSON output. We already have the [`x-request-id`](../backend/src/middleware/request_id.rs) stamping. Production change: switch the formatter to JSON, ship to a centralized log store (Loki/CloudWatch/Datadog), and grep by any field.

**(b) Metrics (Prometheus or equivalent).**

| Metric | Type | Purpose |
|--------|------|---------|
| `payway_http_requests_total{endpoint, status}` | counter | Rate + error rate per endpoint |
| `payway_http_request_duration_seconds{endpoint}` | histogram | p50/p95/p99 latency |
| `payway_payment_status_total{from, to}` | counter | Lifecycle transitions; alert on stuck states |
| `payway_ledger_balance{account_type, currency}` | gauge | Per-account-class totals; sanity check |
| `payway_fx_quote_margin{pair}` | histogram | Spread between quoted rate and acquisition cost |
| `payway_webhook_processing_status_total{outcome}` | counter | Processed / ignored / failed / pending |
| `payway_webhook_events_unprocessed` | gauge | Should be near zero; alert if growing |

Implementation: [`axum-prometheus`](https://docs.rs/axum-prometheus) for HTTP metrics, manual increments via the [`metrics`](https://docs.rs/metrics) crate for domain counters.

**(c) Distributed tracing.** OpenTelemetry SDK in the backend, exporting to a tracing backend (Jaeger, Tempo, Datadog APM). Every HTTP request becomes a root span; DB calls and provider calls become child spans. When investigating "this payment took 6 seconds," you see the timeline broken down.

**(d) Invariant alerts.** Beyond traditional latency/error-rate alerts:

1. **Ledger zero-sum violation.** Our deferred trigger refuses to commit imbalanced entries — so this shouldn't happen. *But* the alert is the safety net for when our assumptions are wrong. A SELECT runs every minute:
   ```sql
   SELECT transaction_id, currency, SUM(amount)
   FROM ledger_entries
   GROUP BY transaction_id, currency
   HAVING SUM(amount) <> 0;
   ```
   Any rows → page immediately. This should never alert; if it does, it's a P0 because money is no longer conserved.

2. **Webhooks stuck in pending.** `webhook_events WHERE processed_at IS NULL AND received_at < NOW() - INTERVAL '5 minutes'`. Warn at >5; page at >20.

3. **Transactions stuck in initiated/processing.**  Initiated >5min suggests the outbox worker is stuck. Processing >24h suggests the webhook never arrived.

4. **Balance vs. cache drift.** If we add the materialized balance cache from [`learn/schema-design.md`](schema-design.md) §1.4, run periodic reconciliation: cached balance vs. SUM(ledger). Drift = a write bypassed the cache update. Alert immediately.

5. **Idempotency table growth.** If the cleanup job is broken, `idempotency_keys` grows unbounded. Alert on row count >threshold or expired-not-deleted count.

**(e) Synthetic monitoring.** A canary that does a real end-to-end test every minute: `POST /payments` for $1 from a synthetic sender to a synthetic recipient, simulate the webhook completion, verify the ledger entries land, then reverse to clean up. If the canary fails twice in a row, page someone.

---

## 5. Secrets management and rotation

### Why it matters

Production credentials currently live in `.env` files:

- `WEBHOOK_SECRET` — if leaked, an attacker can forge webhooks (every fix in [`learn/code-review-junior-webhook.md`](code-review-junior-webhook.md) becomes moot).
- `DATABASE_URL` includes the database password — if leaked, full database access.
- Future API keys (from #2) — if any leak, an attacker authenticates as the holder.

`.env` files are bad for production because:

- They're easy to accidentally commit (we have it in `.gitignore`, but the discipline is fragile across many engineers and environments).
- They're hard to rotate — every rotation requires a deploy.
- They have no audit trail — no record of who read the secret, or when.
- They tend to leak into logs, error messages, and crash dumps.

### What failure mode it prevents

1. **Secret exfiltration → full database compromise / webhook forging.**
2. **Inability to respond to a breach** — if you suspect a secret has leaked but can't rotate cleanly, you have to choose between downtime and continued exposure.
3. **Long-lived credentials** — same secret in use for years, increasing the blast radius if it ever does leak.

### Implementation

**(a) Move to a secrets manager.** AWS Secrets Manager, HashiCorp Vault, GCP Secret Manager — pick based on infrastructure. The backend reads secrets at boot via the SDK, authenticating with workload identity (IAM role, Kubernetes service account, Vault token). The `.env` file in production holds only the secrets-manager *path* (`SECRET_PATH=/payway/prod/webhook_secret`), never the secret itself.

**(b) Type-level redaction.** Wrap all secrets in [`secrecy::Secret<String>`](https://docs.rs/secrecy) or a homegrown wrapper:

```rust
pub struct WebhookSecret(secrecy::Secret<String>);

impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookSecret([REDACTED])")
    }
}
```

Now `tracing::debug!(?state)` cannot accidentally print the secret. The compiler enforces the discipline; engineers don't have to remember.

Migration from current code:

```rust
// In state.rs
pub webhook_secret: WebhookSecret,  // was: Arc<String>

// At use site
verify_hmac_sha256(state.webhook_secret.expose_secret().as_bytes(), ...)
//                                     ^^^^^^^^^^^^^^ explicit "I want the secret here"
```

The `expose_secret()` call is a deliberate point of attention — code review can grep for it.

**(c) Rotation grace period for the webhook secret.** During rotation, both old and new secrets are valid for some overlap window (24h typical). Schema:

```sql
CREATE TABLE webhook_secrets (
  id          UUID PRIMARY KEY,
  secret_hash TEXT NOT NULL,  -- never store plaintext; provider has the cleartext, we have the verifier
  valid_from  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
  valid_until TIMESTAMPTZ,
  rotated_in_by TEXT  -- who did the rotation
);
```

Verifier tries each active secret in order; success on any = valid. Old secret is set `valid_until = NOW() + 24h` at rotation time; after that, expired entries are auto-removed by the cleanup job.

**(d) Rotation schedule.** Webhook secret every 90 days; database password every 30 days; API keys every 90 days OR on user request. Automated via the secrets manager's rotation features (most have built-in support for database password rotation; webhook secrets need a custom rotation Lambda or equivalent).

**(e) Audit log of secret reads.** Every read of a secret from the secrets manager is logged with: caller identity, source IP, secret path, timestamp. Reviewable for "did this secret get read from somewhere it shouldn't have." Most secrets managers do this natively (CloudTrail for AWS Secrets Manager, Vault audit logs).

---

## What's NOT on the top-5

The list above is the things I'd insist on before going live. There's a longer list of "before the year is out" items:

- **Encryption at rest for PII** (`recipients.bank_account_number`). Important but the data is fundamentally non-secret for the operator; needed for compliance more than security.
- **Multi-tenant isolation.** Required when we have multiple businesses on shared infrastructure; not strictly required for a single-tenant launch.
- **Cleanup jobs** for `idempotency_keys`, `webhook_events`, expired `fx_quotes`. Mechanical; without them, tables grow unboundedly but slowly — a few months' grace.
- **Balance materialization** as discussed in [`learn/schema-design.md`](schema-design.md) §1.4. The derived `account_balances` view is fine at thousands of payments; needs materialization at millions.
- **Connection pool tuning.** `max_connections=20` is a guess. Real tuning requires load testing.
- **Read replica routing** for `GET /payments` queries. Latency optimization; not correctness.
- **FX P&L tracking** as a separate account class. Important for treasury accounting but not for transactional integrity.
- **GDPR / data retention.** Required for European customers; can be designed in but not blocker for launch in a single jurisdiction.
- **Stuck-webhook replay worker.** Mentioned in [`learn/failure-scenarios.md`](failure-scenarios.md) 4B.2 — important but built on top of the outbox foundation in #1.

The five I chose are the ones whose absence makes a production launch *unsafe* rather than *suboptimal*. Each is the difference between "we can launch and iterate" and "we'll have an incident in the first month that costs trust we can't rebuild."
