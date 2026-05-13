use rust_decimal::Decimal;
use serde::Deserialize;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::error::AppError;
use crate::webhook_signature::verify_hmac_sha256;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Outcome of processing one inbound webhook delivery.
///
/// The HTTP handler maps all three variants to `200 OK` — they're "expected"
/// outcomes that should not cause provider retries. Only an unexpected
/// `AppError` (e.g. DB down) bubbles up as 5xx.
#[derive(Debug)]
pub enum ProcessingResult {
    /// Status update applied; ledger entries (if any) written.
    Processed,
    /// Recognized but not actionable: invalid signature, malformed body,
    /// unknown provider_reference, transaction in a non-`processing` state,
    /// or an unrecognised `status` value.
    Ignored,
    /// Same (provider, provider_event_id) seen before. No-op.
    Duplicate,
}

#[derive(Debug, Deserialize)]
struct WebhookPayload {
    event_id: String,
    provider_reference: String,
    status: String,
    failure_reason: Option<String>,
}

/// Process one inbound webhook delivery. See `learn/webhooks.md` for the
/// full walkthrough.
pub async fn process(
    pool: &PgPool,
    secret: &str,
    provider: &str,
    raw_body: &[u8],
    signature: Option<&str>,
) -> Result<ProcessingResult, AppError> {
    let mut tx = pool.begin().await?;

    // Step 1: signature check. We log invalid signatures so the audit trail
    // is complete; we return Ignored (HTTP will translate to 200).
    let signature_valid = match signature {
        Some(sig) => verify_hmac_sha256(secret.as_bytes(), raw_body, sig).is_ok(),
        None => false,
    };
    if !signature_valid {
        log_failed_webhook(
            &mut tx,
            provider,
            None,
            raw_body,
            signature,
            false,
            "ignored",
            Some("signature missing or invalid"),
            None,
        )
        .await?;
        tx.commit().await?;
        return Ok(ProcessingResult::Ignored);
    }

    // Step 2: parse JSON. If this fails, the raw bytes are STILL the source
    // of truth — log them verbatim with the parse error.
    let payload: WebhookPayload = match serde_json::from_slice(raw_body) {
        Ok(p) => p,
        Err(e) => {
            log_failed_webhook(
                &mut tx,
                provider,
                None,
                raw_body,
                signature,
                true,
                "failed",
                Some(&format!("malformed JSON: {e}")),
                None,
            )
            .await?;
            tx.commit().await?;
            return Ok(ProcessingResult::Ignored);
        }
    };

    // Step 3: dedup via the unique index on (provider, provider_event_id).
    // Two simultaneous deliveries of the same event: one inserts and proceeds;
    // the other gets None from the RETURNING clause and bails out.
    let webhook_event_id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO webhook_events
            (provider, provider_event_id, raw_payload, headers, signature, signature_valid, processing_status)
         VALUES ($1, $2, $3, '{}'::jsonb, $4, true, 'pending')
         ON CONFLICT (provider, provider_event_id) WHERE provider_event_id IS NOT NULL DO NOTHING
         RETURNING id",
    )
    .bind(provider)
    .bind(&payload.event_id)
    .bind(raw_body)
    .bind(signature)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(webhook_event_id) = webhook_event_id else {
        tx.commit().await?;
        return Ok(ProcessingResult::Duplicate);
    };

    // Step 4: look up the transaction by provider_reference, and take a row
    // lock. The lock prevents a concurrent transaction (e.g. a manual reversal
    // in 4B.4) from racing the status update.
    let tx_row: Option<TransactionRow> = sqlx::query_as(
        "SELECT id,
                status::text         AS status,
                sender_account_id,
                source_currency,
                source_amount,
                destination_currency,
                destination_amount,
                recipient_id
         FROM transactions
         WHERE provider_reference = $1
         FOR UPDATE",
    )
    .bind(&payload.provider_reference)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(txn) = tx_row else {
        // Unknown provider_reference. Always 200, always logged — see
        // learn/concepts/webhook-security.md "Why always 200".
        mark_webhook(
            &mut tx,
            webhook_event_id,
            "ignored",
            Some("unknown provider_reference"),
            None,
        )
        .await?;
        tx.commit().await?;
        return Ok(ProcessingResult::Ignored);
    };

    // Step 5: state machine guard. We only act on transactions in `processing`.
    // Anything else (completed/failed/reversed) is a late or duplicate webhook;
    // mark it ignored so we don't double-credit/double-reverse.
    if txn.status != "processing" {
        mark_webhook(
            &mut tx,
            webhook_event_id,
            "ignored",
            Some(&format!(
                "transaction not in processing state (currently: {})",
                txn.status
            )),
            Some(txn.id),
        )
        .await?;
        tx.commit().await?;
        return Ok(ProcessingResult::Ignored);
    }

    // Step 6: dispatch.
    match payload.status.as_str() {
        "completed" => {
            handle_completion(
                &mut tx,
                txn.id,
                txn.recipient_id,
                &txn.destination_currency,
                txn.destination_amount,
            )
            .await?;
        }
        "failed" => {
            handle_failure(
                &mut tx,
                txn.id,
                txn.sender_account_id,
                &txn.source_currency,
                txn.source_amount,
                payload.failure_reason.as_deref(),
            )
            .await?;
        }
        other => {
            mark_webhook(
                &mut tx,
                webhook_event_id,
                "ignored",
                Some(&format!("unknown status: {other}")),
                Some(txn.id),
            )
            .await?;
            tx.commit().await?;
            return Ok(ProcessingResult::Ignored);
        }
    }

    mark_webhook(&mut tx, webhook_event_id, "processed", None, Some(txn.id)).await?;
    tx.commit().await?;
    Ok(ProcessingResult::Processed)
}

// ---------------------------------------------------------------------------
// Internal types + helpers
// ---------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct TransactionRow {
    id: Uuid,
    status: String,
    sender_account_id: Uuid,
    source_currency: String,
    source_amount: Decimal,
    destination_currency: String,
    destination_amount: Decimal,
    recipient_id: Uuid,
}

// Settlement entries (destination currency). The original source-currency
// entries from create_payment stay where they are; the sender's debit is
// permanent. This is the FX trade: we received NGN, we deliver USD.
async fn handle_completion(
    conn: &mut PgConnection,
    transaction_id: Uuid,
    recipient_id: Uuid,
    destination_currency: &str,
    destination_amount: Decimal,
) -> Result<(), AppError> {
    let dest_clearing: Uuid =
        sqlx::query_scalar("SELECT id FROM accounts WHERE account_type = 'clearing' AND currency = $1")
            .bind(destination_currency)
            .fetch_one(&mut *conn)
            .await?;

    let recipient_account =
        ensure_recipient_external_account(&mut *conn, recipient_id, destination_currency).await?;

    sqlx::query(
        "INSERT INTO ledger_entries
            (transaction_id, account_id, amount, currency, entry_type)
         VALUES
            ($1, $2, $3, $4, 'debit_dest_clearing'),
            ($1, $5, $6, $4, 'credit_recipient')",
    )
    .bind(transaction_id)
    .bind(dest_clearing)
    .bind(-destination_amount)
    .bind(destination_currency)
    .bind(recipient_account)
    .bind(destination_amount)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "UPDATE transactions
         SET status = 'completed', completed_at = NOW()
         WHERE id = $1",
    )
    .bind(transaction_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

// Compensating entries (source currency). Append-only ledger: we don't UPDATE
// the original debit; we add new entries that net the original out. The two
// groups (original + reversal) each sum to zero per currency, and their union
// also sums to zero — the deferred trigger validates both.
async fn handle_failure(
    conn: &mut PgConnection,
    transaction_id: Uuid,
    sender_account_id: Uuid,
    source_currency: &str,
    source_amount: Decimal,
    failure_reason: Option<&str>,
) -> Result<(), AppError> {
    let src_clearing: Uuid =
        sqlx::query_scalar("SELECT id FROM accounts WHERE account_type = 'clearing' AND currency = $1")
            .bind(source_currency)
            .fetch_one(&mut *conn)
            .await?;

    sqlx::query(
        "INSERT INTO ledger_entries
            (transaction_id, account_id, amount, currency, entry_type)
         VALUES
            ($1, $2, $3, $4, 'reverse_debit_sender'),
            ($1, $5, $6, $4, 'reverse_credit_clearing')",
    )
    .bind(transaction_id)
    .bind(sender_account_id)
    .bind(source_amount) // re-credit sender (positive)
    .bind(source_currency)
    .bind(src_clearing)
    .bind(-source_amount) // debit clearing (negative)
    .execute(&mut *conn)
    .await?;

    sqlx::query(
        "UPDATE transactions
         SET status = 'failed',
             failed_at = NOW(),
             failure_reason = $1
         WHERE id = $2",
    )
    .bind(failure_reason)
    .bind(transaction_id)
    .execute(&mut *conn)
    .await?;

    Ok(())
}

// Get-or-create the recipient's external account in `currency`. The "no-op
// UPDATE" trick (DO UPDATE SET <existing-value>) ensures RETURNING fires on
// both insert and conflict — `fetch_one` always yields the id without a
// second roundtrip.
async fn ensure_recipient_external_account(
    conn: &mut PgConnection,
    recipient_id: Uuid,
    currency: &str,
) -> Result<Uuid, AppError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO accounts (account_type, owner_recipient_id, currency, display_name)
         VALUES ('external_recipient', $1, $2, $3)
         ON CONFLICT (owner_recipient_id, currency) WHERE owner_recipient_id IS NOT NULL
         DO UPDATE SET display_name = accounts.display_name
         RETURNING id",
    )
    .bind(recipient_id)
    .bind(currency)
    .bind(format!("recipient-external-{currency}"))
    .fetch_one(&mut *conn)
    .await?;
    Ok(id)
}

// One-shot insert into webhook_events for the cases where we don't reach the
// deduped INSERT above (invalid signature, malformed JSON).
//
// provider_event_id stays NULL, which means these rows don't participate in
// dedup. A future enhancement is to hash raw_payload for deduping malformed
// requests, but for the prototype we accept the unbounded growth and rely on
// network-layer rate limiting in production.
#[allow(clippy::too_many_arguments)]
async fn log_failed_webhook(
    conn: &mut PgConnection,
    provider: &str,
    event_id: Option<&str>,
    raw_payload: &[u8],
    signature: Option<&str>,
    signature_valid: bool,
    processing_status: &str,
    processing_error: Option<&str>,
    related_transaction_id: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO webhook_events
            (provider, provider_event_id, raw_payload, headers, signature,
             signature_valid, processing_status, processed_at, processing_error,
             related_transaction_id)
         VALUES
            ($1, $2, $3, '{}'::jsonb, $4, $5,
             $6::webhook_processing_status, NOW(), $7, $8)",
    )
    .bind(provider)
    .bind(event_id)
    .bind(raw_payload)
    .bind(signature)
    .bind(signature_valid)
    .bind(processing_status)
    .bind(processing_error)
    .bind(related_transaction_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn mark_webhook(
    conn: &mut PgConnection,
    webhook_event_id: Uuid,
    status: &str,
    processing_error: Option<&str>,
    related_transaction_id: Option<Uuid>,
) -> Result<(), AppError> {
    sqlx::query(
        "UPDATE webhook_events
         SET processing_status     = $1::webhook_processing_status,
             processed_at          = NOW(),
             processing_error      = $2,
             related_transaction_id = COALESCE(related_transaction_id, $3)
         WHERE id = $4",
    )
    .bind(status)
    .bind(processing_error)
    .bind(related_transaction_id)
    .bind(webhook_event_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
