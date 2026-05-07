# Schema Design

> **What this document is for:** The spec asks four questions about Part 1. This doc answers them, but more importantly it walks you through *why* each decision was made and *what alternative we rejected*. Read [migrations/0001_initial_schema.sql](../migrations/0001_initial_schema.sql) alongside it — the SQL is short on prose, this file is where the reasoning lives.

---

## TL;DR

1. **Money lives in `ledger_entries` only.** Everything else (account balances, transaction status) is an interpretation of the ledger.
2. **The ledger is append-only and zero-sum.** You can't update or delete entries; every group of entries sums to zero per currency. Enforced by triggers, not application code.
3. **State machines and idempotency are enforced at the database**, because the database is the only thing that sees concurrent requests.

---

## 1. Why this structure (and what we rejected)

### 1.1 One account per `(owner, currency, account_type)` — not "an account with multi-currency balances"

The intuitive model is: an account has *balances* in different currencies, like a bank app showing "$1,200 USD / ₦450,000 NGN" on one screen. We rejected it.

- **Why we rejected it:** the ledger has to record *which currency* each entry is in. If "Lagos Imports' account" can hold any currency, then `ledger_entries.account_id` has to be paired with a currency for every row, *and* you need cross-currency queries to do anything ("how much NGN do they have?" becomes a filter). The "balance" concept also gets weird — is `accounts.balance` a JSONB? A child table?
- **What we did instead:** every account has exactly one currency. Lagos Imports has *separate accounts* for NGN, USD, etc. The UI can group them by owner; the data model doesn't care.
- **Tradeoff:** more rows in `accounts`. That's fine — we're talking dozens to thousands per business client, not millions.

### 1.2 `NUMERIC(20, 4)` for money, separate `currency` column — not BIGINT minor units

The other defensible choice is BIGINT minor units (kobo, cents) — it's what Stripe and most modern fintech does. We did **not** pick it.

- **Why BIGINT minor units is appealing:** integer arithmetic is exact, fast, and impossible to round wrong. You never accidentally multiply a USD amount by an integer and get `1.9999999998`.
- **Why `NUMERIC` won here:** currencies have different exponents. JPY has 0 decimal places (¥150 is the smallest unit). USD has 2 (cents). BHD (Bahraini Dinar) has 3. If you store everything as "minor units" you also need a `currency_exponent` column or hardcoded lookup, and you have to apply it on every read. FX math (rate × amount) introduces fractional minor units anyway, so you end up needing a "rounding strategy" column. `NUMERIC(20, 4)` sidesteps this — 4 decimal places is more than any real currency uses, and Postgres handles the arithmetic correctly.
- **What you give up:** a tiny amount of performance, and you have to be careful never to mix `NUMERIC` with `FLOAT` (Postgres lets you, the result is `DOUBLE PRECISION`, and silent precision loss). Solution: never declare `FLOAT` columns for money, and Rust's `sqlx` will yell at you if you try to bind an `f64` to a `NUMERIC` column.
- **When BIGINT minor units would be the right call:** higher volume (millions of payments/day where the perf gap matters), single currency or very few currencies, or shipping a system that integrates with another system that uses minor units (banking core systems often do).

### 1.3 Append-only, signed-amount ledger — not "debit / credit columns"

Two common shapes for a ledger row:

1. `(account_id, debit NUMERIC, credit NUMERIC)` — one of the two is zero
2. `(account_id, amount NUMERIC)` — signed, negative = debit, positive = credit

We picked **2 (signed)**. Reasons:

- Summing a single column to get a balance is straightforward; with separate debit/credit you write `SUM(credit) - SUM(debit)` everywhere and one mistake reverses the sign.
- The zero-sum invariant becomes elegant: `SUM(amount) = 0` per group per currency. With debit/credit it's `SUM(debit) = SUM(credit)` per group per currency — equivalent but less direct.
- Most modern ledger systems (Square's TigerBeetle, Stripe's docs on their internal ledger) use signed amounts.

The append-only invariant is **enforced by trigger** — `UPDATE` and `DELETE` on `ledger_entries` raise an exception. To "reverse" a payment, we insert *new* compensating entries. The original entries stay forever. This means every audit question ("what was the state on March 12?") is answerable by querying `ledger_entries WHERE created_at < '2026-03-12'`.

### 1.4 Balances are *derived*, not stored

`account_balances` is a `VIEW`, not a table. There is no `balance` column anywhere.

- **Why:** if balance is stored, it can drift from the ledger — say, because a deploy partially fails, or a bug forgets to update one side, or someone runs `UPDATE accounts SET balance = ...` in production. With a derived view, drift is impossible by construction. The ledger is the only writable surface.
- **The performance question** (because this is where someone will push back): summing potentially millions of ledger rows per balance query is bad. True. The fix is **materialized balances with reconciliation**, not stored balances trusted blindly:
  1. Add `account_balances_cache (account_id, currency, balance, last_entry_id)` table.
  2. Update it transactionally with each `ledger_entries` insert (trigger or application).
  3. Run a **periodic reconciliation job** that recomputes from scratch and alerts on any divergence.
  4. The view stays the source of truth; the cache is an optimization.
- For prototype: derived is fine. The view will perform adequately at thousands of payments. Document the upgrade path; don't ship the optimization yet.

### 1.5 Idempotency in its own table — not a column on `transactions`

`idempotency_keys` stores `(scope, key, request_hash, response_status, response_body, transaction_id, expires_at)`.

- **Why a separate table:** the spec says "The same `Idempotency-Key` must return the same response without creating a duplicate transaction." Returning the *same response* means storing the response. Idempotency isn't deduplication — it's replay. A column on `transactions` couldn't replay errors (e.g. a 400 from a validation failure, where no transaction was created) and couldn't store the response body.
- **Why `request_hash`:** detects "same key, different body." That's a client bug — they reused a key for a different request. We surface it with **HTTP 422** instead of either (a) silently returning the cached response (wrong; client thinks their new request succeeded) or (b) creating a duplicate (wrong; violates idempotency). This is a small thing that real payment processors get right and most prototypes get wrong.
- **The unique index is the actual concurrency guarantee.** If two concurrent requests both try to insert with the same `(scope, key)`, one wins, one fails with a unique-violation. The application catches the violation and returns the cached response. *Application-level checks lose to concurrency every time*; the database constraint is the real lock.
- `expires_at` lets us purge old keys (typically 24h after creation). Without expiry, the table grows forever.

### 1.6 Webhooks logged raw — and the table has its own life

Two pitfalls people hit with webhooks:

1. They parse the JSON before logging, then crash on malformed input — no record of what happened.
2. They process and log in the same transaction, so a processing crash rolls back the log too — no record of what happened.

`webhook_events` fixes both:

- **Stores `raw_payload BYTEA`** (raw bytes, not parsed JSON). This matters for HMAC signature verification — the signature is computed over the *bytes the provider sent*, not your parsed-and-reserialized version. JSON whitespace, key ordering, and number formatting all change between encoders.
- **Logged in its own transaction**, *before* business logic runs. Even if processing fails, the raw record persists.
- `processed_at` is `NULL` until processing completes. A scan for "unprocessed webhooks older than 5 minutes" gives you a queue of stuck work to investigate.
- The unique index on `(provider, provider_event_id)` deduplicates retries from the provider. The provider sends the same event twice (very common — they retry on any non-2xx, and even on 2xx if their ack timeout fires); we INSERT, hit the unique violation, return 200 OK, do nothing.

The spec asks why `POST /webhooks/provider` should return **200 OK even for unknown references**. Two reasons, both worth understanding:

1. **Operational:** providers retry indefinitely on non-2xx. If you 404 an unknown reference, the provider hammers you forever, and a single misrouted webhook can DoS the endpoint.
2. **Security:** an attacker can probe `/webhooks/provider` with arbitrary references to enumerate which transactions exist on your system (response codes leak existence). Always-200 closes that side channel.

The mechanism: log the webhook, mark it `processing_status = 'ignored'` with a reason like "unknown provider_reference," return 200. We have a record; the provider stops retrying; nothing leaked.

### 1.7 State machine in the database, not just the application

The transition `initiated → processing → completed | failed` and `completed → reversed` is enforced by a trigger.

- **Why not just enforce it in Rust:** because two concurrent requests could both see status `processing` and both try to set it to `completed` — application-level checks miss this if the read and write aren't in the same locked transaction. The trigger fires *inside* the database, on every UPDATE, so concurrency can't bypass it.
- **What about the application?** It still validates transitions before issuing the UPDATE — that's where you produce a clean error message ("payment already failed, can't mark complete") instead of a generic constraint violation. Belt and suspenders. App for UX, database for correctness.
- A `transaction_status_history` row is auto-inserted on every status change, by another trigger. That's where the dashboard's timeline view reads from.

### 1.8 The `journal_id` column on `ledger_entries`

`ledger_entries` has *both* `transaction_id` and `journal_id`, with a CHECK that exactly one is set.

- **Why:** not every ledger movement is a payment. Opening balances, deposits, internal adjustments, FX P&L bookings — these are real ledger events that don't have a customer-facing transaction. Trying to model them as fake transactions distorts the `transactions` table (you'd see "transactions" that aren't payments at all).
- A `journal_id` is just "any group of balanced ledger entries that aren't a payment." The seed deposits in [migrations/0002_seed.sql](../migrations/0002_seed.sql) use this.
- The zero-sum trigger is written to handle either: it groups by `COALESCE(transaction_id, journal_id)`. The invariant is the same — every group nets to zero per currency.
- **Alternative we rejected:** a separate `journal_entries` table parallel to `ledger_entries`. It would mean two places to query for an account's history, and joins everywhere. Not worth the conceptual purity.

### 1.9 `currencies` table with FK references everywhere

Every currency-bearing column (`accounts.currency`, `transactions.source_currency` / `destination_currency`, `fx_quotes.base_currency` / `quote_currency`, `ledger_entries.currency`) has a foreign key to `currencies.code`.

- **Why:** without this, a typo'd or unsupported currency code (`'XYZ'`, `'usd'`, `'$$$'`) gets accepted by the schema and only fails much later — usually at a confusing FK error against `accounts` when you try to find a clearing account that doesn't exist. The currencies table is the single source of "what currencies does Payway support."
- **`is_active` flag**, not row deletion. Stopping support for a currency means flipping `is_active=false` so new payments are rejected by application code, while historical transactions and ledger entries (which still FK against the row) remain queryable. Deleting the row would either break those FKs or require a destructive migration. The flag is the correct soft-disable mechanism.
- **`exponent` field**: ISO 4217 minor-unit decimal places (NGN=2, USD=2, JPY=0, BHD=3). Our amount columns are `NUMERIC` so this isn't needed for arithmetic, but it's correct UI metadata: "format ¥150 as 150, not 150.00." Store it once here rather than hardcoding it in the frontend.
- **CHECK constraints** enforce that codes are uppercase 3-character strings — `'usd'` and `'us'` are rejected at insert time. Cheap belt-and-suspenders.
- **Alternative we rejected:** an enum (`CREATE TYPE currency_code AS ENUM ('NGN', 'USD', ...)`). Postgres enums can't be modified transactionally — `ALTER TYPE ... ADD VALUE` works, but reordering or removing values is painful, and you can't store metadata (name, exponent, active flag) alongside an enum value. A lookup table is more flexible and the FK overhead is negligible.

---

## 2. How balance integrity is guaranteed

This is the spec's second question. The answer is: **five layers**, each independent, each catching a different failure mode.

| Layer | What it catches |
|-------|----------------|
| `NUMERIC(20, 4)` instead of `FLOAT` | Floating-point rounding errors compounding into "missing pennies" |
| Append-only ledger (UPDATE/DELETE rejected by trigger) | An adjustment hiding the original record; manual `UPDATE` in production |
| Zero-sum check trigger (`SUM(amount) = 0` per group per currency) | A debit without a matching credit; off-by-one in compensating entries |
| Derived `account_balances` view (no stored balance) | Cached/stored balance drifting from the ledger |
| Transaction state machine trigger | Status updates that bypass the lifecycle (e.g. reviving a `failed` payment) |

A bug that violates *one* layer is caught by another. To create or destroy money, you'd have to defeat all five — and at that point, the bug is loud (Postgres exceptions, not silent corruption).

What this does **not** prevent:

- **Wrong-currency entries.** If a payment is in NGN but a developer mis-types and creates a USD entry, the zero-sum check still passes (each currency sums to zero independently). Mitigation: the application enforces that ledger entries for a transaction match `transactions.source_currency` / `destination_currency`. This isn't enforced in the schema; it could be added as another trigger if we wanted defense-in-depth.
- **Wrong-account entries.** Crediting the wrong recipient is a logic bug. The schema can't catch "you sent money to the wrong person." That's what testing, code review, and the timeline audit log exist for.

---

## 3. Adding a new currency pair in production

The spec asks how this would work. The honest answer involves both schema work *and* operational/business work — I'll give both.

**Schema-side (5 minutes, all data inserts — no schema migration):**

1. Register the currency in `currencies`:
   ```sql
   INSERT INTO currencies (code, name, exponent)
   VALUES ('XAF', 'Central African CFA franc', 0);
   ```
2. Insert a `clearing` account in the new currency:
   ```sql
   INSERT INTO accounts (account_type, owner_business_id, currency, display_name)
   VALUES ('clearing', '00000000-0000-0000-0000-000000000001', 'XAF', 'Payway XAF clearing');
   ```
3. Insert a matching `system` external-source account:
   ```sql
   INSERT INTO accounts (account_type, currency, display_name)
   VALUES ('system', 'XAF', 'External deposits source — XAF');
   ```
4. Update the FX rate provider configuration to fetch the new pair (e.g. `NGN/XAF`).

That's it for the database. No table changes; no migration.

**Operational side (the actually-hard part):**

1. **Source of FX rates** for the new pair. Major pairs (USD, EUR, GBP) have deep liquid markets and many rate providers. Exotic pairs may have wide spreads, fewer providers, and less reliable real-time data.
2. **A funded clearing position** in the new currency. Before you can pay anyone in XAF, you need XAF inventory — sourced via FX trade, settlement bank, or correspondent.
3. **A downstream provider** that can deliver to that currency's banking system.
4. **Compliance:** know-your-business rules for the destination country, sanctions screening, currency control regulations (CFA franc has specific FCFA rules, etc.).
5. **FX risk policy:** how long are you willing to hold an unhedged XAF position? At what size do you hedge? This is a treasury/finance call, not an engineering one.

**The schema is designed so that step 1 is trivial and steps 2–5 dominate**, which is the right shape — the costly work is the operational/compliance work, not schema migration. If adding a currency required a schema migration, that's a sign the schema is wrong.

**Soft-disabling a currency:** the `currencies.is_active` flag stops *new* payments in a currency without affecting history. Application code at `POST /payments` rejects inactive currencies with a clean error before any ledger work. Closing out a currency in production typically means: flip `is_active=false`, drain remaining clearing inventory via FX trades or settlement, then leave the row forever (deleting it would break FKs from historical transactions and ledger entries — that's the point).

---

## 4. Other things you should know

### 4.1 What this schema does NOT model

These are deliberate omissions for the prototype. Each would be a real production concern.

- **Multi-leg / split payments.** One transaction = one sender, one recipient, one currency conversion. Some real flows (e.g. paying multiple suppliers from one bulk transfer) need a parent/child transaction model. Not modeled.
- **FX P&L tracking.** When Payway quotes a customer 1 NGN = 0.0006 USD but actually trades at 0.00059 USD on the FX market, the difference is Payway's revenue (or loss). The schema doesn't track this — there's no separate "FX trade" ledger event. A real system has a `system` account per currency for FX P&L, with entries inserted when the rate-quote-vs-actual gap is realized. Mention worth making in Part 4 (production readiness).
- **Reconciliation against bank statements.** Real treasuries match every ledger entry against an external statement (the actual money movement at the settlement bank). Schema would need a `bank_statement_lines` table and a reconciliation join. Not modeled.
- **Encryption of recipient PII.** `recipients.bank_account_number` is plaintext in the prototype. In production, this would be encrypted at rest (column-level encryption, KMS-managed keys). Mention in production readiness.
- **Multi-tenancy.** No `tenant_id` on anything. Single-tenant prototype.
- **Soft-delete / GDPR right-to-erasure.** No tombstoning of recipients; no purge process for old data. Real product would need both, balanced against the audit-log retention requirement.

### 4.2 Why the seed file uses journal entries

[migrations/0002_seed.sql](../migrations/0002_seed.sql) seeds opening balances. It uses `journal_id` (not `transaction_id`) for the ledger entries because:

1. There's no real "payment" happening — it's an opening balance.
2. We don't want fake rows in the `transactions` table polluting the dashboard list.
3. The double-entry invariant still applies (credit user account, debit external source) and is still checked by the same trigger.

This is the principled use of the `journal_id` mechanism described in §1.8.

### 4.3 Concurrency: what's actually preventing the double-spend

The spec's Part 4B.1 asks about double-spend. The full answer comes in the API code, but the schema sets up the foundations:

1. `idempotency_keys` unique index on `(scope, key)` makes duplicate-request retries safe.
2. The `POST /payments` handler will use `SELECT ... FOR UPDATE` on the sender's accounts row when checking-then-debiting the balance. This serializes concurrent payments from the same sender.
3. `fx_quotes.locked_by_transaction_id` partial unique index ensures a quote is claimed by exactly one transaction (a `UPDATE fx_quotes SET locked_by_transaction_id = $1 WHERE id = $2 AND locked_by_transaction_id IS NULL` either updates 1 row or 0 — atomic claim).
4. Postgres `READ COMMITTED` isolation (the default) is sufficient for the patterns above. We don't need `SERIALIZABLE`. If we did need it, we'd have a different design.

### 4.4 Where this schema breaks under scale

Honest assessment of where this falls over:

- **`account_balances` view.** At ~100K ledger entries it's fine. At ~10M it's slow. Fix: materialize, as discussed in §1.4.
- **`ledger_entries` table size.** Append-only means it grows forever. At hundreds of millions of rows you'd partition by `created_at` (monthly partitions) and have a cold-storage strategy. Out of scope here.
- **Webhook replay queue.** The "unprocessed webhooks" index assumes the queue stays small. If a downstream processing bug causes thousands to back up, the index is fine but the processing pipeline isn't. A dedicated worker / queue (e.g. PostgreSQL `LISTEN/NOTIFY` or a real message queue) would be the next step.

None of these are problems for a prototype, but knowing where the limits are is part of the design.

---

## Cross-references

- SQL: [migrations/0001_initial_schema.sql](../migrations/0001_initial_schema.sql) (schema), [migrations/0002_seed.sql](../migrations/0002_seed.sql) (seed)
- The teaching contract and non-negotiables: [.claude/skills/payway-guide/SKILL.md](../.claude/skills/payway-guide/SKILL.md)
- Spec: [requirement.md](../requirement.md)
