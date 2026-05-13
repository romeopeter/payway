// Integration tests for GET /payments/:id and GET /payments.
//
// Same pattern as the other test files: drive the service functions directly,
// each test gets a fresh DB via #[sqlx::test].

use chrono::{Duration, Utc};
use payway_backend::domain::payments::{
    create_payment, get_payment_detail, list_payments, CreatePaymentRequest, CreatePaymentResponse,
    ListPaymentsQuery,
};
use payway_backend::error::AppError;
use payway_backend::fx::SimulatedFxProvider;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

const LAGOS_NGN_ACCOUNT: &str = "00000000-0000-0000-0000-000000000300";
const ACME_RECIPIENT: &str = "00000000-0000-0000-0000-000000000020";

async fn seed_payment(pool: &PgPool, key: &str) -> CreatePaymentResponse {
    let fx = SimulatedFxProvider::new();
    create_payment(
        pool,
        &fx,
        key,
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
async fn detail_returns_full_structure(pool: PgPool) {
    let payment = seed_payment(&pool, "detail-1").await;

    let detail = get_payment_detail(&pool, payment.id).await.unwrap();

    assert_eq!(detail.id, payment.id);
    assert_eq!(detail.reference, payment.reference);
    assert_eq!(detail.status, "processing");
    assert_eq!(detail.source_currency, "NGN");
    assert_eq!(detail.source_amount, dec!(1000000));
    assert_eq!(detail.destination_currency, "USD");
    assert_eq!(detail.fx_rate, dec!(0.000625));
    assert_eq!(detail.sender_name.as_deref(), Some("Lagos Imports Ltd"));
    assert_eq!(detail.recipient_name, "Acme Components Inc");
    assert_eq!(detail.recipient_country, "US");

    // Two ledger entries from create_payment (debit sender + credit clearing).
    assert_eq!(detail.ledger_entries.len(), 2);
    let amounts: Vec<_> = detail.ledger_entries.iter().map(|e| e.amount).collect();
    assert!(amounts.contains(&dec!(-1000000)));
    assert!(amounts.contains(&dec!(1000000)));

    // Status history: initial 'initiated' + transition to 'processing'.
    assert_eq!(detail.status_history.len(), 2);
    assert_eq!(detail.status_history[0].from_status, None);
    assert_eq!(detail.status_history[0].to_status, "initiated");
    assert_eq!(detail.status_history[1].from_status.as_deref(), Some("initiated"));
    assert_eq!(detail.status_history[1].to_status, "processing");
}

#[sqlx::test(migrations = "../migrations")]
async fn detail_404_for_unknown_id(pool: PgPool) {
    let result = get_payment_detail(&pool, Uuid::new_v4()).await;
    assert!(matches!(result, Err(AppError::NotFound)));
}

#[sqlx::test(migrations = "../migrations")]
async fn list_returns_payments_newest_first_with_pagination(pool: PgPool) {
    // Seed two payments with distinct idempotency keys.
    let p1 = seed_payment(&pool, "list-1").await;
    // Tiny gap so initiated_at is monotonically increasing.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let p2 = seed_payment(&pool, "list-2").await;

    // First page (limit 1): newest first → p2.
    let page1 = list_payments(
        &pool,
        ListPaymentsQuery {
            status: None,
            from_date: None,
            to_date: None,
            limit: Some(1),
            offset: Some(0),
        },
    )
    .await
    .unwrap();
    assert_eq!(page1.total, 2);
    assert_eq!(page1.items.len(), 1);
    assert_eq!(page1.items[0].id, p2.id);

    // Second page (limit 1, offset 1): p1.
    let page2 = list_payments(
        &pool,
        ListPaymentsQuery {
            status: None,
            from_date: None,
            to_date: None,
            limit: Some(1),
            offset: Some(1),
        },
    )
    .await
    .unwrap();
    assert_eq!(page2.total, 2);
    assert_eq!(page2.items.len(), 1);
    assert_eq!(page2.items[0].id, p1.id);
}

#[sqlx::test(migrations = "../migrations")]
async fn list_filters_by_status(pool: PgPool) {
    // After create_payment the transaction is `processing`. We mutate one
    // to `failed` via a manufactured webhook-equivalent UPDATE so we have
    // two distinct statuses in the DB. (Going through the actual webhook
    // handler is overkill for this filter test.)
    let p_processing = seed_payment(&pool, "filter-proc").await;
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let p_to_fail = seed_payment(&pool, "filter-fail").await;

    // Flip one to 'failed' directly via the state machine.
    sqlx::query("UPDATE transactions SET status = 'failed', failed_at = NOW() WHERE id = $1")
        .bind(p_to_fail.id)
        .execute(&pool)
        .await
        .unwrap();

    let processing_only = list_payments(
        &pool,
        ListPaymentsQuery {
            status: Some("processing".into()),
            from_date: None,
            to_date: None,
            limit: None,
            offset: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(processing_only.total, 1);
    assert_eq!(processing_only.items[0].id, p_processing.id);

    let failed_only = list_payments(
        &pool,
        ListPaymentsQuery {
            status: Some("failed".into()),
            from_date: None,
            to_date: None,
            limit: None,
            offset: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(failed_only.total, 1);
    assert_eq!(failed_only.items[0].id, p_to_fail.id);
}

#[sqlx::test(migrations = "../migrations")]
async fn list_filters_by_date_range(pool: PgPool) {
    let _p = seed_payment(&pool, "date-range").await;

    // Range covering "now" should include it.
    let yesterday = Utc::now() - Duration::days(1);
    let tomorrow = Utc::now() + Duration::days(1);

    let in_range = list_payments(
        &pool,
        ListPaymentsQuery {
            status: None,
            from_date: Some(yesterday),
            to_date: Some(tomorrow),
            limit: None,
            offset: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(in_range.total, 1);

    // Range fully in the past should exclude it.
    let week_ago = Utc::now() - Duration::days(7);
    let two_days_ago = Utc::now() - Duration::days(2);
    let out_of_range = list_payments(
        &pool,
        ListPaymentsQuery {
            status: None,
            from_date: Some(week_ago),
            to_date: Some(two_days_ago),
            limit: None,
            offset: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(out_of_range.total, 0);
    assert!(out_of_range.items.is_empty());
}

#[sqlx::test(migrations = "../migrations")]
async fn list_empty_db_returns_zero(pool: PgPool) {
    let response = list_payments(
        &pool,
        ListPaymentsQuery {
            status: None,
            from_date: None,
            to_date: None,
            limit: None,
            offset: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(response.total, 0);
    assert!(response.items.is_empty());
    assert_eq!(response.limit, 20);
    assert_eq!(response.offset, 0);
}
