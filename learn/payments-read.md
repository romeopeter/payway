# `GET /payments/:id` + `GET /payments` — walkthrough

> **What this is for:** the read side of the API from Part 2d. Much shorter than the create/webhook docs because the read side is mostly SQL — the interesting decisions are about response shape, pagination, and how to query without dynamic SQL injection.

---

## `GET /payments/:id` — detail view

**Purpose:** give the ops dashboard everything it needs to display one payment, including:
- the transaction record (status, amounts, currencies, timestamps, failure reason if any)
- linked names (sender business, recipient details) — without forcing the frontend to join
- the FX rate from the linked `fx_quotes` row
- **all ledger entries** for the transaction (so the operator can see the debit/credit pair, the settlement entries on completion, the reversal entries on failure)
- **the status history timeline** with timestamps and reasons

**Why embed ledger + history in the same response?** The dashboard always needs all three together. Splitting into separate endpoints adds round-trips and lets the frontend display partial state (transaction loaded but timeline still spinning). The cost of embedding is one extra SQL query each — well worth it.

**Implementation** ([`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs) `get_payment_detail`):

1. **Main row** — one SELECT with joins to `fx_quotes`, `accounts` (sender), `business_entities` (sender business), and `recipients`. Returns `None` if no transaction with that id → handler maps to `404`.
2. **Ledger entries** — second SELECT joining `ledger_entries` and `accounts` (so we can include `account_type` for context). Ordered by id (insertion order — append-only ledger means insertion order is also chronological).
3. **Status history** — third SELECT from `transaction_status_history`, ordered by `(changed_at, id)`.

The `enum::text` cast pattern is used to project Postgres enums into `String` for serialization — same trick as elsewhere.

**404 handling:** `AppError::NotFound` → 404 via the IntoResponse impl. The handler is a one-liner.

---

## `GET /payments` — list view

**Purpose:** the dashboard's transaction table. Each row carries enough to render without per-row API calls.

**Filters supported:**
- `status` — exact match against `transaction_status` enum
- `from_date` / `to_date` — inclusive lower, exclusive upper bound on `initiated_at`
- `limit` — 1..=100, default 20
- `offset` — non-negative, default 0

**Pagination: offset + limit, not cursor.** Two reasons:
1. The dashboard naturally wants "page 3 of 27" style UI. Offset matches that mental model.
2. Volume is low. Offset-based pagination only becomes a problem at large `OFFSET` values where Postgres has to count past rows to find the page; we're nowhere near that.

If the data volume ever grew enough to make offset slow (typically 10K+ rows in the result), we'd switch to cursor pagination encoding `(initiated_at, id)` for stable ordering across page boundaries. The schema's `transactions_status_initiated_at` index supports both patterns equally.

**Dynamic filters without dynamic SQL:**

A naive implementation would build the SQL string conditionally:
```rust
let mut sql = "SELECT ... WHERE 1=1".to_string();
if let Some(s) = status { sql += " AND status = '" + s + "'" }  // INJECTION
```

That's how SQL injection happens. Don't do this.

Our approach uses the **`($1::type IS NULL OR column = $1::type)`** pattern. Bind every filter as `Option<T>` and rely on Postgres' short-circuit evaluation to skip the comparison when the parameter is `NULL`:

```sql
WHERE ($1::transaction_status IS NULL OR t.status = $1::transaction_status)
  AND ($2::timestamptz       IS NULL OR t.initiated_at >= $2)
  AND ($3::timestamptz       IS NULL OR t.initiated_at <  $3)
```

When `$1` is `None`, the OR short-circuits and the filter is no-op. When it's `Some("processing")`, the comparison applies. Single SQL string, no dynamic concatenation, no injection surface.

There's also the more elegant `sqlx::QueryBuilder` API for building dynamic SQL safely, but for three optional filters the `(NULL OR match)` pattern is more readable and produces a plan the optimizer handles well.

**Total count:** a second query with the same WHERE clauses but `SELECT COUNT(*)`. We could combine into one query using `COUNT(*) OVER ()` as a window function on every row, but the two-query approach is clearer and the cost difference is negligible at our scale.

---

## What's tested

[`backend/tests/payments_read.rs`](../backend/tests/payments_read.rs):

| Test | What it verifies |
|------|------------------|
| `detail_returns_full_structure` | All fields populated, ledger has 2 entries (debit+credit), history has 2 transitions |
| `detail_404_for_unknown_id` | Returns `AppError::NotFound` (→ HTTP 404) |
| `list_returns_payments_newest_first_with_pagination` | `ORDER BY initiated_at DESC`, offset+limit correctly slices |
| `list_filters_by_status` | Two payments in different states → status filter returns each correctly |
| `list_filters_by_date_range` | Range covering payment includes it; range excluding it returns empty |
| `list_empty_db_returns_zero` | No data → total=0, items empty, sensible defaults applied |

Run:
```bash
cd backend
DATABASE_URL=postgres://payway:payway_local_dev@localhost:5432/payway \
  cargo test --test payments_read
```

---

## Things deliberately not done

- **No projection-aware queries.** We always SELECT the full row even when the consumer might want a subset. For our scale that's fine; if list-view bandwidth became a problem we could add a `fields=` query param.
- **No request-level caching.** Reads are cheap on this data; the dashboard isn't refetching aggressively. A CDN cache header on `GET /payments/:id` would be wrong anyway because the status changes over time.
- **No CORS configuration.** The frontend in Part 3 lives in the same docker-compose stack and hits the backend at the same origin (via the same docker network or via a dev proxy). If the dashboard ever moved to a separate origin, we'd add `tower_http::cors`.
- **No auth.** Same reason as the rest of the API — out of spec scope.

---

## Cross-references

- [`backend/src/domain/payments.rs`](../backend/src/domain/payments.rs) — read API: `get_payment_detail`, `list_payments`, the row types
- [`backend/src/routes/payments.rs`](../backend/src/routes/payments.rs) — GET routes
- [`learn/payments-create.md`](payments-create.md) — companion doc for the write side
- [`learn/schema-design.md`](schema-design.md) §1.7 — `transaction_status_history` and how it's populated by trigger
