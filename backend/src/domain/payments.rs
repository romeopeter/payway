use chrono::{DateTime, Datelike, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppError;
use crate::fx::SimulatedFxProvider;
use crate::idempotency::{self, IdempotencyOutcome};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreatePaymentRequest {
    pub sender_account_id: Uuid,
    pub recipient_id: Uuid,
    pub source_amount: Decimal,
    pub destination_currency: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreatePaymentResponse {
    pub id: Uuid,
    pub reference: String,
    pub status: String,
    pub sender_account_id: Uuid,
    pub recipient_id: Uuid,
    pub source_currency: String,
    pub source_amount: Decimal,
    pub destination_currency: String,
    pub destination_amount: Decimal,
    pub fx_rate: Decimal,
    pub provider_reference: Option<String>,
    pub provider_name: Option<String>,
    pub initiated_at: DateTime<Utc>,
    pub submitted_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

const IDEMPOTENCY_SCOPE: &str = "POST /payments";

pub async fn create_payment(
    pool: &PgPool,
    fx: &SimulatedFxProvider,
    idempotency_key: &str,
    body: CreatePaymentRequest,
) -> Result<CreatePaymentResponse, AppError> {
    let body = validate_and_normalize(body)?;
    let request_hash = compute_request_hash(&body)?;

    // Single DB transaction holds: idempotency claim, sender lock,
    // balance check, FX quote, transaction insert, ledger pair, status
    // transition, and the cached-response write. All-or-nothing.
    let mut tx = pool.begin().await?;

    match idempotency::check(&mut tx, IDEMPOTENCY_SCOPE, idempotency_key, &request_hash).await? {
        IdempotencyOutcome::Replay(cached) => {
            // No work to do; commit the empty transaction so the connection
            // returns to the pool cleanly.
            tx.commit().await?;
            return serde_json::from_value(cached).map_err(|e| {
                AppError::Internal(anyhow::anyhow!("malformed cached idempotency response: {e}"))
            });
        }
        IdempotencyOutcome::Conflict => return Err(AppError::IdempotencyConflict),
        IdempotencyOutcome::Proceed => {}
    }

    // SELECT ... FOR UPDATE on the sender's accounts row. This is the
    // per-account mutex that prevents double-spend (see learn/concepts/double-spend.md).
    let sender = fetch_sender_for_update(&mut tx, body.sender_account_id).await?;

    if sender.account_type != "user" {
        return Err(AppError::BadRequest(
            "sender must be a user account".into(),
        ));
    }

    if sender.currency == body.destination_currency {
        return Err(AppError::BadRequest(
            "source and destination currency must differ".into(),
        ));
    }

    require_active_currency(&mut tx, &sender.currency).await?;
    require_active_currency(&mut tx, &body.destination_currency).await?;
    require_recipient_exists(&mut tx, body.recipient_id).await?;

    // Balance derived from the ledger; we hold the accounts lock so no other
    // transaction can write entries for this account before we commit.
    let balance = current_balance(&mut tx, sender.id).await?;
    if balance < body.source_amount {
        return Err(AppError::InsufficientBalance {
            balance,
            requested: body.source_amount,
            currency: sender.currency.clone(),
        });
    }

    // FX quote.
    let rate = fx.quote(&sender.currency, &body.destination_currency)?;
    let destination_amount = (body.source_amount * rate).round_dp(4);
    let quote_id = insert_fx_quote(
        &mut tx,
        &sender.currency,
        &body.destination_currency,
        rate,
        body.source_amount,
        destination_amount,
    )
    .await?;

    let src_clearing_id = clearing_account_id(&mut tx, &sender.currency).await?;

    // Transaction row + ledger pair + quote claim + provider submission, all
    // committed together. The deferred ledger zero-sum trigger fires at
    // COMMIT and will refuse to commit if our two entries don't net to zero.
    let reference = generate_reference();
    let initiated_at = Utc::now();
    let tx_id = insert_transaction(
        &mut tx,
        &reference,
        sender.id,
        body.recipient_id,
        &sender.currency,
        body.source_amount,
        &body.destination_currency,
        destination_amount,
        quote_id,
        initiated_at,
    )
    .await?;

    sqlx::query("UPDATE fx_quotes SET locked_by_transaction_id = $1 WHERE id = $2")
        .bind(tx_id)
        .bind(quote_id)
        .execute(&mut *tx)
        .await?;

    insert_ledger_pair(
        &mut tx,
        tx_id,
        sender.id,
        src_clearing_id,
        body.source_amount,
        &sender.currency,
    )
    .await?;

    // Simulated provider call. In production this is the network IO that
    // motivates the outbox pattern (see learn/payments-create.md "What
    // production would do differently"). For the prototype it never fails.
    let provider_reference = format!("SIM-{}", Uuid::new_v4());
    let submitted_at = Utc::now();

    sqlx::query(
        "UPDATE transactions
         SET status = 'processing',
             provider_reference = $1,
             provider_name = 'sim',
             submitted_at = $2
         WHERE id = $3",
    )
    .bind(&provider_reference)
    .bind(submitted_at)
    .bind(tx_id)
    .execute(&mut *tx)
    .await?;

    let response = CreatePaymentResponse {
        id: tx_id,
        reference,
        status: "processing".into(),
        sender_account_id: sender.id,
        recipient_id: body.recipient_id,
        source_currency: sender.currency.clone(),
        source_amount: body.source_amount,
        destination_currency: body.destination_currency.clone(),
        destination_amount,
        fx_rate: rate,
        provider_reference: Some(provider_reference),
        provider_name: Some("sim".into()),
        initiated_at,
        submitted_at: Some(submitted_at),
    };

    // Cache the response so retries with the same key replay byte-for-byte.
    let response_json = serde_json::to_value(&response)
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;

    sqlx::query(
        "UPDATE idempotency_keys
         SET response_status = $1,
             response_body   = $2,
             transaction_id  = $3
         WHERE scope = $4 AND key = $5",
    )
    .bind(202_i32)
    .bind(&response_json)
    .bind(tx_id)
    .bind(IDEMPOTENCY_SCOPE)
    .bind(idempotency_key)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(response)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

struct SenderAccount {
    id: Uuid,
    currency: String,
    account_type: String,
}

fn validate_and_normalize(mut body: CreatePaymentRequest) -> Result<CreatePaymentRequest, AppError> {
    if body.source_amount <= Decimal::ZERO {
        return Err(AppError::BadRequest(
            "source_amount must be positive".into(),
        ));
    }
    body.destination_currency = body.destination_currency.to_uppercase();
    if body.destination_currency.len() != 3 {
        return Err(AppError::BadRequest(
            "destination_currency must be a 3-letter ISO 4217 code".into(),
        ));
    }
    Ok(body)
}

// Hash of the deserialised request body. Stable across retries because we
// re-serialise via serde_json with deterministic field order.
fn compute_request_hash(body: &CreatePaymentRequest) -> Result<String, AppError> {
    let canonical =
        serde_json::to_vec(body).map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?;
    Ok(hex::encode(Sha256::digest(&canonical)))
}

// Reference format: PWY-<year>-<12 uppercase hex chars from a UUID v4>.
// 16^12 ≈ 2.8e14 possibilities; collisions effectively impossible at our
// scale. UNIQUE constraint on transactions.reference catches any anyway.
fn generate_reference() -> String {
    let year = Utc::now().year();
    let suffix: String = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect::<String>()
        .to_uppercase();
    format!("PWY-{year}-{suffix}")
}

async fn fetch_sender_for_update(
    conn: &mut PgConnection,
    sender_id: Uuid,
) -> Result<SenderAccount, AppError> {
    let row: Option<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, currency, account_type::text
         FROM accounts
         WHERE id = $1
         FOR UPDATE",
    )
    .bind(sender_id)
    .fetch_optional(&mut *conn)
    .await?;

    let (id, currency, account_type) = row.ok_or_else(|| {
        AppError::BadRequest(format!("sender_account_id {sender_id} not found"))
    })?;

    Ok(SenderAccount {
        id,
        currency,
        account_type,
    })
}

async fn require_active_currency(conn: &mut PgConnection, code: &str) -> Result<(), AppError> {
    let active: Option<bool> = sqlx::query_scalar("SELECT is_active FROM currencies WHERE code = $1")
        .bind(code)
        .fetch_optional(&mut *conn)
        .await?;

    match active {
        Some(true) => Ok(()),
        Some(false) => Err(AppError::BadRequest(format!(
            "currency {code} is not currently active"
        ))),
        None => Err(AppError::BadRequest(format!(
            "currency {code} is not supported"
        ))),
    }
}

async fn require_recipient_exists(conn: &mut PgConnection, id: Uuid) -> Result<(), AppError> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM recipients WHERE id = $1)")
        .bind(id)
        .fetch_one(&mut *conn)
        .await?;

    if !exists {
        return Err(AppError::BadRequest(format!("recipient_id {id} not found")));
    }
    Ok(())
}

async fn current_balance(conn: &mut PgConnection, account_id: Uuid) -> Result<Decimal, AppError> {
    let bal: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric FROM ledger_entries WHERE account_id = $1",
    )
    .bind(account_id)
    .fetch_one(&mut *conn)
    .await?;
    Ok(bal)
}

#[allow(clippy::too_many_arguments)]
async fn insert_fx_quote(
    conn: &mut PgConnection,
    base: &str,
    quote: &str,
    rate: Decimal,
    base_amount: Decimal,
    quote_amount: Decimal,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO fx_quotes
            (base_currency, quote_currency, rate, base_amount, quote_amount, expires_at)
         VALUES ($1, $2, $3, $4, $5, NOW() + INTERVAL '60 seconds')
         RETURNING id",
    )
    .bind(base)
    .bind(quote)
    .bind(rate)
    .bind(base_amount)
    .bind(quote_amount)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

async fn clearing_account_id(conn: &mut PgConnection, currency: &str) -> Result<Uuid, AppError> {
    sqlx::query_scalar(
        "SELECT id FROM accounts WHERE account_type = 'clearing' AND currency = $1",
    )
    .bind(currency)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "no clearing account for currency {currency}; seed migration is wrong"
        ))
    })
}

#[allow(clippy::too_many_arguments)]
async fn insert_transaction(
    conn: &mut PgConnection,
    reference: &str,
    sender_account_id: Uuid,
    recipient_id: Uuid,
    source_currency: &str,
    source_amount: Decimal,
    destination_currency: &str,
    destination_amount: Decimal,
    fx_quote_id: Uuid,
    initiated_at: DateTime<Utc>,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO transactions (
            reference, status, sender_account_id, recipient_id,
            source_currency, source_amount, destination_currency, destination_amount,
            fx_quote_id, initiated_at
         )
         VALUES ($1, 'initiated', $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING id",
    )
    .bind(reference)
    .bind(sender_account_id)
    .bind(recipient_id)
    .bind(source_currency)
    .bind(source_amount)
    .bind(destination_currency)
    .bind(destination_amount)
    .bind(fx_quote_id)
    .bind(initiated_at)
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

async fn insert_ledger_pair(
    conn: &mut PgConnection,
    transaction_id: Uuid,
    sender_account_id: Uuid,
    clearing_account_id: Uuid,
    amount: Decimal,
    currency: &str,
) -> Result<(), AppError> {
    // Two entries: -amount on the sender, +amount on the source-currency
    // clearing. The deferred zero-sum trigger validates this nets to 0
    // per (transaction_id, currency) at COMMIT.
    sqlx::query(
        "INSERT INTO ledger_entries
            (transaction_id, account_id, amount, currency, entry_type)
         VALUES
            ($1, $2, $3, $4, 'debit_sender'),
            ($1, $5, $6, $4, 'credit_clearing')",
    )
    .bind(transaction_id)
    .bind(sender_account_id)
    .bind(-amount)
    .bind(currency)
    .bind(clearing_account_id)
    .bind(amount)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

// ===========================================================================
// Read API
// ===========================================================================

#[derive(Debug, Serialize)]
pub struct PaymentDetail {
    pub id: Uuid,
    pub reference: String,
    pub status: String,
    pub sender_account_id: Uuid,
    pub sender_name: Option<String>,
    pub recipient_id: Uuid,
    pub recipient_name: String,
    pub recipient_country: String,
    pub source_currency: String,
    pub source_amount: Decimal,
    pub destination_currency: String,
    pub destination_amount: Decimal,
    pub fx_rate: Decimal,
    pub provider_name: Option<String>,
    pub provider_reference: Option<String>,
    pub initiated_at: DateTime<Utc>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub failure_reason: Option<String>,
    pub ledger_entries: Vec<LedgerEntryView>,
    pub status_history: Vec<StatusHistoryView>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LedgerEntryView {
    pub id: i64,
    pub account_id: Uuid,
    pub account_type: String,
    pub amount: Decimal,
    pub currency: String,
    pub entry_type: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StatusHistoryView {
    pub from_status: Option<String>,
    pub to_status: String,
    pub changed_at: DateTime<Utc>,
    pub reason: Option<String>,
}

#[derive(sqlx::FromRow)]
struct PaymentDetailRow {
    id: Uuid,
    reference: String,
    status: String,
    sender_account_id: Uuid,
    sender_name: Option<String>,
    recipient_id: Uuid,
    recipient_name: String,
    recipient_country: String,
    source_currency: String,
    source_amount: Decimal,
    destination_currency: String,
    destination_amount: Decimal,
    fx_rate: Decimal,
    provider_name: Option<String>,
    provider_reference: Option<String>,
    initiated_at: DateTime<Utc>,
    submitted_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    failed_at: Option<DateTime<Utc>>,
    failure_reason: Option<String>,
}

pub async fn get_payment_detail(pool: &PgPool, id: Uuid) -> Result<PaymentDetail, AppError> {
    // Three reads — one for the transaction + linked names + FX rate, then
    // two for the embedded collections. We could fold these into one query
    // with array_agg of jsonb, but the readability cost isn't worth the
    // single-roundtrip win at our scale.
    let row: Option<PaymentDetailRow> = sqlx::query_as(
        "SELECT
            t.id,
            t.reference,
            t.status::text                AS status,
            t.sender_account_id,
            sb.name                       AS sender_name,
            t.recipient_id,
            r.name                        AS recipient_name,
            r.country_code                AS recipient_country,
            t.source_currency,
            t.source_amount,
            t.destination_currency,
            t.destination_amount,
            q.rate                        AS fx_rate,
            t.provider_name,
            t.provider_reference,
            t.initiated_at,
            t.submitted_at,
            t.completed_at,
            t.failed_at,
            t.failure_reason
         FROM transactions t
         JOIN fx_quotes q       ON q.id  = t.fx_quote_id
         JOIN accounts sa       ON sa.id = t.sender_account_id
         LEFT JOIN business_entities sb ON sb.id = sa.owner_business_id
         JOIN recipients r      ON r.id  = t.recipient_id
         WHERE t.id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Err(AppError::NotFound);
    };

    let ledger_entries: Vec<LedgerEntryView> = sqlx::query_as(
        "SELECT le.id,
                le.account_id,
                a.account_type::text  AS account_type,
                le.amount,
                le.currency,
                le.entry_type,
                le.created_at
         FROM ledger_entries le
         JOIN accounts a ON a.id = le.account_id
         WHERE le.transaction_id = $1
         ORDER BY le.id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    let status_history: Vec<StatusHistoryView> = sqlx::query_as(
        "SELECT from_status::text  AS from_status,
                to_status::text    AS to_status,
                changed_at,
                reason
         FROM transaction_status_history
         WHERE transaction_id = $1
         ORDER BY changed_at, id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;

    Ok(PaymentDetail {
        id: row.id,
        reference: row.reference,
        status: row.status,
        sender_account_id: row.sender_account_id,
        sender_name: row.sender_name,
        recipient_id: row.recipient_id,
        recipient_name: row.recipient_name,
        recipient_country: row.recipient_country,
        source_currency: row.source_currency,
        source_amount: row.source_amount,
        destination_currency: row.destination_currency,
        destination_amount: row.destination_amount,
        fx_rate: row.fx_rate,
        provider_name: row.provider_name,
        provider_reference: row.provider_reference,
        initiated_at: row.initiated_at,
        submitted_at: row.submitted_at,
        completed_at: row.completed_at,
        failed_at: row.failed_at,
        failure_reason: row.failure_reason,
        ledger_entries,
        status_history,
    })
}

#[derive(Debug, Deserialize)]
pub struct ListPaymentsQuery {
    pub status: Option<String>,
    pub from_date: Option<DateTime<Utc>>,
    pub to_date: Option<DateTime<Utc>>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct ListPaymentsResponse {
    pub items: Vec<PaymentListItem>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PaymentListItem {
    pub id: Uuid,
    pub reference: String,
    pub status: String,
    pub source_currency: String,
    pub source_amount: Decimal,
    pub destination_currency: String,
    pub destination_amount: Decimal,
    pub fx_rate: Decimal,
    pub initiated_at: DateTime<Utc>,
    pub sender_name: Option<String>,
    pub recipient_name: String,
    pub recipient_country: String,
}

pub async fn list_payments(
    pool: &PgPool,
    query: ListPaymentsQuery,
) -> Result<ListPaymentsResponse, AppError> {
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let offset = query.offset.unwrap_or(0);

    // Optional filters via the (NULL OR match) pattern. Postgres short-circuits
    // when the parameter is NULL, so the filter doesn't restrict — equivalent
    // to "no filter applied". One SQL statement instead of dynamic SQL.
    let items: Vec<PaymentListItem> = sqlx::query_as(
        "SELECT
            t.id,
            t.reference,
            t.status::text       AS status,
            t.source_currency,
            t.source_amount,
            t.destination_currency,
            t.destination_amount,
            q.rate               AS fx_rate,
            t.initiated_at,
            sb.name              AS sender_name,
            r.name               AS recipient_name,
            r.country_code       AS recipient_country
         FROM transactions t
         JOIN fx_quotes q       ON q.id  = t.fx_quote_id
         JOIN accounts sa       ON sa.id = t.sender_account_id
         LEFT JOIN business_entities sb ON sb.id = sa.owner_business_id
         JOIN recipients r      ON r.id  = t.recipient_id
         WHERE ($1::transaction_status IS NULL OR t.status = $1::transaction_status)
           AND ($2::timestamptz       IS NULL OR t.initiated_at >= $2)
           AND ($3::timestamptz       IS NULL OR t.initiated_at <  $3)
         ORDER BY t.initiated_at DESC, t.id DESC
         LIMIT $4 OFFSET $5",
    )
    .bind(query.status.as_deref())
    .bind(query.from_date)
    .bind(query.to_date)
    .bind(limit as i64)
    .bind(offset as i64)
    .fetch_all(pool)
    .await?;

    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)
         FROM transactions t
         WHERE ($1::transaction_status IS NULL OR t.status = $1::transaction_status)
           AND ($2::timestamptz       IS NULL OR t.initiated_at >= $2)
           AND ($3::timestamptz       IS NULL OR t.initiated_at <  $3)",
    )
    .bind(query.status.as_deref())
    .bind(query.from_date)
    .bind(query.to_date)
    .fetch_one(pool)
    .await?;

    Ok(ListPaymentsResponse {
        items,
        total,
        limit,
        offset,
    })
}
