# Double-Spend Prevention

> **What this is for:** the locking story behind [`backend/src/domain/payments.rs`](../../backend/src/domain/payments.rs)'s `fetch_sender_for_update`. After this you'll be able to explain (a) the classic double-spend race, (b) why `SELECT FOR UPDATE` prevents it, (c) why we lock `accounts` and not `ledger_entries`, (d) why we don't reach for `SERIALIZABLE` isolation. This is also the prepared answer to spec Part 4B.1.

---

## The classic race

Lagos Imports has 1,000,000 NGN. Two concurrent requests, each for 800,000 NGN, with *different* idempotency keys (so idempotency doesn't save us — they're two distinct logical payments).

Without locking:

```
T+0   Request A                       | Request B
T+1   BEGIN                           |
T+2   read balance: 1,000,000         | BEGIN
T+3   1,000,000 >= 800,000 ? yes      | read balance: 1,000,000
T+4                                   | 1,000,000 >= 800,000 ? yes
T+5   debit ledger -800,000           |
T+6                                   | debit ledger -800,000
T+7   COMMIT                          |
T+8                                   | COMMIT
```

Final balance: -600,000. Both transactions succeeded; the customer is now overdrawn by 600,000 NGN. Money was created from nothing.

The bug: Request B read the balance *before* Request A wrote its debit. Postgres' default isolation level (`READ COMMITTED`) doesn't protect against this — each *statement* sees the latest committed data, but two reads in different transactions are independent. There's no rule that says "if you read X and then someone else writes X, your transaction must abort."

---

## The fix: `SELECT ... FOR UPDATE`

Our `fetch_sender_for_update` runs:

```sql
SELECT id, currency, account_type::text
FROM accounts
WHERE id = $1
FOR UPDATE
```

`FOR UPDATE` acquires a row-level *write* lock on the matching `accounts` row. It is held until the transaction commits or rolls back. While Transaction A holds it, any other `SELECT FOR UPDATE` (or `UPDATE`/`DELETE`) on the same row blocks until A finishes.

Same race, with the lock:

```
T+0   Request A                       | Request B
T+1   BEGIN                           |
T+2   SELECT FOR UPDATE accounts row  | BEGIN
T+3     - acquires lock               | SELECT FOR UPDATE accounts row
T+4   read balance: 1,000,000         |   - blocks waiting for A's lock
T+5   debit ledger -800,000           |
T+6   COMMIT                          |   - lock released
T+7                                   |   - acquires lock
T+8                                   | read balance: 200,000
T+9                                   | 200,000 >= 800,000 ? no
T+10                                  | return InsufficientBalance
T+11                                  | ROLLBACK
```

Request B sees the post-commit state of A. The overdraft is impossible.

Importantly: the second SELECT in Request B reads the *current* committed state (200,000), not the snapshot it might have started with at T+2. This is `READ COMMITTED` doing what it says — each statement reads the latest committed version, and `FOR UPDATE` happens to also see the latest version because it's a write-lock probe.

---

## Why we lock `accounts` and not `ledger_entries`

You might think: "we're computing balance from the ledger — shouldn't we lock the ledger rows we read?" Two reasons we don't:

1. **You can't `FOR UPDATE` on an aggregate.** `SELECT SUM(amount) FROM ledger_entries ... FOR UPDATE` doesn't lock the result of the aggregate; it doesn't really make sense. `FOR UPDATE` locks rows. There's no specific "balance row" to lock.
2. **The `accounts` row is the natural mutex.** There's exactly one row per account. Locking it serializes *all* writers for that account, by definition. While I hold the accounts lock, no other code path can read-then-write the ledger for this account — they're all queued behind me on `SELECT FOR UPDATE`.

This is the standard pattern for ledger systems: identify the entity-level row (the account), lock it as a mutex, do whatever read+write work you need on related tables. The related tables (`ledger_entries`) don't themselves need locking because the accounts lock funnels all access through one writer at a time.

---

## What about deadlocks?

If two transactions take locks in different orders, you can deadlock. Example (hypothetical, not in our current code):

- Request A: pays from account X to account Y. Locks X first, then tries to lock Y.
- Request B: pays from account Y to account X. Locks Y first, then tries to lock X.

If both reach step 1 before either reaches step 2, deadlock. Postgres detects this and aborts one of them with `40P01 deadlock_detected`.

Our code doesn't have this exposure because we only lock the **sender**. The recipient's account is updated in the webhook handler (2c), in a separate transaction. There's no second lock to acquire during payment creation.

If we ever needed to lock two accounts at once (e.g. an internal transfer between two user accounts), the discipline is **always lock in a deterministic order** — typically by ascending UUID. Then no two concurrent transactions can grab them in different orders. Document the rule, code-review for it.

---

## Could we just use `SERIALIZABLE` isolation instead?

Postgres supports `SERIALIZABLE` isolation, where transactions execute *as if* one at a time. No `FOR UPDATE` needed; the database guarantees serializability and aborts transactions that would violate it.

We don't, because:

- `SERIALIZABLE` retries on conflict. The application has to wrap every transaction in a retry loop, with care about which errors are retryable (`40001 serialization_failure` is the one).
- It's more expensive at scale. Postgres tracks read/write sets to detect serialization anomalies; high-contention workloads can see significant retry rates.
- The failure mode changes from "block then proceed" to "fail then retry," which is harder to reason about and harder to debug under load.
- For our access patterns (one entity-level row to lock, then writes to related tables), `READ COMMITTED + SELECT FOR UPDATE` does the same job with less ceremony.

Most production databases I've seen run `READ COMMITTED` with explicit locks. `SERIALIZABLE` shows up when access patterns are too tangled to reason about with manual locks — e.g. complex multi-table invariants where you can't easily identify a single entity-level row to use as a mutex. Not our situation.

---

## What if you forgot the `FOR UPDATE`?

You'd have a double-spend bug — the very one in §1. Without the lock, two concurrent requests can both pass the balance check and both write debits. Sender ends up overdrawn.

The bug would be hard to find in testing because it requires concurrent requests with specific timing. It would slip through unit tests and integration tests, and only surface in production — and only sometimes. The customer would notice when their balance went negative.

The schema can't prevent it from the writer side. `ledger_entries.amount` has no "balance can't go negative" CHECK constraint, and shouldn't, because legitimate flows do briefly flip balance signs (reversals, fee bookings). The application-level lock is the only thing preventing the race.

This is why the schema design ([`learn/schema-design.md`](../schema-design.md) §4.3) flagged the lock as load-bearing: the integrity of the entire ledger depends on the application acquiring it correctly. Code review for it. Test it under concurrency if possible (a tokio test that fires two `create_payment` calls in parallel and asserts only one succeeds).

---

## The prepared answer to Part 4B.1

The spec asks: *"Two concurrent `POST /payments` requests arrive with different idempotency keys but for the same sender account whose balance only covers one payment. How do you prevent overdraft?"*

The answer is everything above:

1. Both requests begin a DB transaction.
2. Both call `SELECT FOR UPDATE` on the sender's `accounts` row.
3. The first acquires the lock; the second blocks.
4. The first reads the balance, validates against `source_amount`, inserts ledger entries, commits, releases the lock.
5. The second unblocks, reads the *post-commit* balance, validates — and now finds insufficient funds, returns `InsufficientBalance`, rolls back.

No overdraft, no money created. The mechanism is the per-account row lock. We'll formalize this answer in `learn/failure-scenarios.md` when we get to Part 4.

---

## Cross-references

- [`backend/src/domain/payments.rs`](../../backend/src/domain/payments.rs) — `fetch_sender_for_update` is where the lock is acquired
- [`learn/payments-create.md`](../payments-create.md) §5 — where this fits in the create_payment flow
- [`learn/schema-design.md`](../schema-design.md) §4.3 — the original mention of the pattern from Part 1
- [`requirement.md`](../../requirement.md) Part 4B.1 — the failure scenario this answers
