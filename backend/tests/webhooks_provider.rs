// Integration tests for the webhook processor.
//
// Each test runs in a fresh DB via #[sqlx::test(migrations = ...)].
// We test the service function `webhooks::process` directly — same pattern
// as tests/payments_create.rs. No HTTP server needed.

use hmac::{Hmac, Mac};
use payway_backend::domain::payments::{create_payment, CreatePaymentRequest, CreatePaymentResponse};
use payway_backend::domain::webhooks::{process, ProcessingResult};
use payway_backend::fx::SimulatedFxProvider;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use sha2::Sha256;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

const SECRET: &str = "test_webhook_secret";
const PROVIDER: &str = "sim";
const LAGOS_NGN_ACCOUNT: &str = "00000000-0000-0000-0000-000000000300";
const ACME_RECIPIENT: &str = "00000000-0000-0000-0000-000000000020";

// Sign body bytes with the same algorithm the real provider would use.
fn sign(body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

async fn seed_payment(pool: &PgPool) -> CreatePaymentResponse {
    let fx = SimulatedFxProvider::new();
    create_payment(
        pool,
        &fx,
        "seed-key",
        CreatePaymentRequest {
            sender_account_id: Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap(),
            recipient_id: Uuid::from_str(ACME_RECIPIENT).unwrap(),
            source_amount: dec!(1000000),
            destination_currency: "USD".into(),
        },
    )
    .await
    .unwrap()
}

#[sqlx::test(migrations = "../migrations")]
async fn completion_credits_recipient_and_marks_completed(pool: PgPool) {
    let payment = seed_payment(&pool).await;
    let provider_ref = payment.provider_reference.clone().unwrap();

    let body = serde_json::to_vec(&json!({
        "event_id": "evt_complete",
        "provider_reference": provider_ref,
        "status": "completed",
    }))
    .unwrap();
    let sig = sign(&body);

    let result = process(&pool, SECRET, PROVIDER, &body, Some(&sig))
        .await
        .unwrap();
    assert!(matches!(result, ProcessingResult::Processed));

    let status: String =
        sqlx::query_scalar("SELECT status::text FROM transactions WHERE id = $1")
            .bind(payment.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "completed");

    // Recipient credited the destination amount.
    let recipient_balance: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric
         FROM ledger_entries le
         JOIN accounts a ON a.id = le.account_id
         WHERE a.owner_recipient_id = $1 AND a.currency = 'USD'",
    )
    .bind(Uuid::from_str(ACME_RECIPIENT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recipient_balance, payment.destination_amount);

    // Destination-currency clearing went down by the same amount (we delivered
    // USD inventory). Seed credited the clearing with 500,000 USD.
    let dest_clearing_balance: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric
         FROM ledger_entries le
         JOIN accounts a ON a.id = le.account_id
         WHERE a.account_type = 'clearing' AND a.currency = 'USD'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(dest_clearing_balance, dec!(500000) - payment.destination_amount);
}

#[sqlx::test(migrations = "../migrations")]
async fn failure_reverses_sender_debit(pool: PgPool) {
    let payment = seed_payment(&pool).await;
    let provider_ref = payment.provider_reference.clone().unwrap();

    // After create_payment, sender is 100M - 1M = 99M.
    let pre: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric FROM ledger_entries WHERE account_id = $1",
    )
    .bind(Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(pre, dec!(99000000));

    let body = serde_json::to_vec(&json!({
        "event_id": "evt_fail",
        "provider_reference": provider_ref,
        "status": "failed",
        "failure_reason": "provider declined",
    }))
    .unwrap();
    let sig = sign(&body);

    let result = process(&pool, SECRET, PROVIDER, &body, Some(&sig))
        .await
        .unwrap();
    assert!(matches!(result, ProcessingResult::Processed));

    let status: String =
        sqlx::query_scalar("SELECT status::text FROM transactions WHERE id = $1")
            .bind(payment.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");

    let reason: Option<String> =
        sqlx::query_scalar("SELECT failure_reason FROM transactions WHERE id = $1")
            .bind(payment.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason.as_deref(), Some("provider declined"));

    // Sender balance restored to 100M via append-only reversal entries.
    let post: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric FROM ledger_entries WHERE account_id = $1",
    )
    .bind(Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(post, dec!(100000000));

    // No recipient ledger entries (failure path skips the destination side).
    let recipient_entries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries le
         JOIN accounts a ON a.id = le.account_id
         WHERE a.owner_recipient_id = $1",
    )
    .bind(Uuid::from_str(ACME_RECIPIENT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recipient_entries, 0);
}

#[sqlx::test(migrations = "../migrations")]
async fn invalid_signature_logs_and_returns_ignored(pool: PgPool) {
    let payment = seed_payment(&pool).await;
    let body = serde_json::to_vec(&json!({
        "event_id": "evt_bad_sig",
        "provider_reference": payment.provider_reference.clone().unwrap(),
        "status": "completed",
    }))
    .unwrap();

    let result = process(&pool, SECRET, PROVIDER, &body, Some("deadbeef"))
        .await
        .unwrap();
    assert!(matches!(result, ProcessingResult::Ignored));

    // Webhook event logged with signature_valid=false.
    let row: (bool, String, Option<String>) = sqlx::query_as(
        "SELECT signature_valid, processing_status::text, processing_error
         FROM webhook_events
         ORDER BY received_at DESC
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!row.0);
    assert_eq!(row.1, "ignored");
    assert!(row.2.unwrap().contains("signature"));

    // Transaction status untouched.
    let status: String =
        sqlx::query_scalar("SELECT status::text FROM transactions WHERE id = $1")
            .bind(payment.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status, "processing");
}

#[sqlx::test(migrations = "../migrations")]
async fn duplicate_event_id_does_not_double_credit(pool: PgPool) {
    let payment = seed_payment(&pool).await;
    let provider_ref = payment.provider_reference.clone().unwrap();

    let body = serde_json::to_vec(&json!({
        "event_id": "evt_dup",
        "provider_reference": provider_ref,
        "status": "completed",
    }))
    .unwrap();
    let sig = sign(&body);

    let r1 = process(&pool, SECRET, PROVIDER, &body, Some(&sig)).await.unwrap();
    let r2 = process(&pool, SECRET, PROVIDER, &body, Some(&sig)).await.unwrap();

    assert!(matches!(r1, ProcessingResult::Processed));
    assert!(matches!(r2, ProcessingResult::Duplicate));

    let recipient_balance: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric
         FROM ledger_entries le
         JOIN accounts a ON a.id = le.account_id
         WHERE a.owner_recipient_id = $1 AND a.currency = 'USD'",
    )
    .bind(Uuid::from_str(ACME_RECIPIENT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recipient_balance, payment.destination_amount);

    // Only one webhook_events row for this event_id.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM webhook_events WHERE provider_event_id = 'evt_dup'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn unknown_provider_reference_is_logged_and_ignored(pool: PgPool) {
    let body = serde_json::to_vec(&json!({
        "event_id": "evt_orphan",
        "provider_reference": "SIM-does-not-exist",
        "status": "completed",
    }))
    .unwrap();
    let sig = sign(&body);

    let result = process(&pool, SECRET, PROVIDER, &body, Some(&sig))
        .await
        .unwrap();
    assert!(matches!(result, ProcessingResult::Ignored));

    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT processing_status::text, processing_error
         FROM webhook_events
         WHERE provider_event_id = 'evt_orphan'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "ignored");
    assert!(row.1.unwrap().contains("unknown provider_reference"));
}

#[sqlx::test(migrations = "../migrations")]
async fn second_completion_for_completed_payment_is_ignored(pool: PgPool) {
    let payment = seed_payment(&pool).await;
    let provider_ref = payment.provider_reference.clone().unwrap();

    let body1 = serde_json::to_vec(&json!({
        "event_id": "evt_first",
        "provider_reference": provider_ref.clone(),
        "status": "completed",
    }))
    .unwrap();
    let sig1 = sign(&body1);

    // Second event has a different event_id (so dedup wouldn't catch it)
    // but targets the same already-completed transaction.
    let body2 = serde_json::to_vec(&json!({
        "event_id": "evt_second",
        "provider_reference": provider_ref,
        "status": "completed",
    }))
    .unwrap();
    let sig2 = sign(&body2);

    let r1 = process(&pool, SECRET, PROVIDER, &body1, Some(&sig1)).await.unwrap();
    let r2 = process(&pool, SECRET, PROVIDER, &body2, Some(&sig2)).await.unwrap();

    assert!(matches!(r1, ProcessingResult::Processed));
    assert!(matches!(r2, ProcessingResult::Ignored));

    // Only one credit_recipient entry exists.
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries
         WHERE transaction_id = $1 AND entry_type = 'credit_recipient'",
    )
    .bind(payment.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn malformed_json_is_logged_with_failed_status(pool: PgPool) {
    let body = b"this is not JSON";
    let sig = sign(body);

    let result = process(&pool, SECRET, PROVIDER, body, Some(&sig))
        .await
        .unwrap();
    assert!(matches!(result, ProcessingResult::Ignored));

    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT processing_status::text, processing_error
         FROM webhook_events
         ORDER BY received_at DESC
         LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "failed");
    assert!(row.1.unwrap().to_lowercase().contains("malformed"));
}
