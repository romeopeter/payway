// Integration tests for the create_payment service.
//
// `#[sqlx::test]` gives each test a fresh database with all migrations
// (including the seed) applied. Tests run in parallel, each on its own DB.
//
// Run with:
//   cd backend && cargo test --test payments_create

use payway_backend::domain::payments::{create_payment, CreatePaymentRequest};
use payway_backend::error::AppError;
use payway_backend::fx::SimulatedFxProvider;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use sqlx::PgPool;
use std::str::FromStr;
use uuid::Uuid;

// Seeded UUIDs from migrations/0002_seed.sql.
const LAGOS_NGN_ACCOUNT: &str = "00000000-0000-0000-0000-000000000300";
const ACME_RECIPIENT: &str = "00000000-0000-0000-0000-000000000020";

fn req() -> CreatePaymentRequest {
    CreatePaymentRequest {
        sender_account_id: Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap(),
        recipient_id: Uuid::from_str(ACME_RECIPIENT).unwrap(),
        source_amount: dec!(1000000),
        destination_currency: "USD".into(),
    }
}

#[sqlx::test(migrations = "../migrations")]
async fn happy_path_debits_sender_and_credits_clearing(pool: PgPool) {
    let fx = SimulatedFxProvider::new();

    let resp = create_payment(&pool, &fx, "key-happy", req()).await.unwrap();

    assert_eq!(resp.status, "processing");
    assert_eq!(resp.source_currency, "NGN");
    assert_eq!(resp.destination_currency, "USD");
    assert_eq!(resp.source_amount, dec!(1000000));
    assert!(resp.destination_amount > Decimal::ZERO);
    assert_eq!(resp.fx_rate, dec!(0.000625));
    assert!(resp.provider_reference.unwrap().starts_with("SIM-"));

    // Sender balance: 100M (seed) - 1M = 99M
    let sender_balance: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric FROM ledger_entries WHERE account_id = $1",
    )
    .bind(Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sender_balance, dec!(99000000));

    // NGN clearing: it had a 0 net before this payment (seed only credited
    // the user account from system source). Now it holds +1M.
    let clearing_balance: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric
         FROM ledger_entries le
         JOIN accounts a ON a.id = le.account_id
         WHERE a.account_type = 'clearing' AND a.currency = 'NGN'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(clearing_balance, dec!(1000000));
}

#[sqlx::test(migrations = "../migrations")]
async fn idempotent_replay_returns_same_response(pool: PgPool) {
    let fx = SimulatedFxProvider::new();

    let resp1 = create_payment(&pool, &fx, "key-replay", req()).await.unwrap();
    let resp2 = create_payment(&pool, &fx, "key-replay", req()).await.unwrap();

    assert_eq!(resp1.id, resp2.id);
    assert_eq!(resp1.reference, resp2.reference);
    assert_eq!(resp1.destination_amount, resp2.destination_amount);
    assert_eq!(resp1.provider_reference, resp2.provider_reference);

    // Only one transaction created.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);

    // Sender debited only once.
    let sender_balance: Decimal = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount), 0)::numeric FROM ledger_entries WHERE account_id = $1",
    )
    .bind(Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(sender_balance, dec!(99000000));
}

#[sqlx::test(migrations = "../migrations")]
async fn same_key_different_body_is_a_conflict(pool: PgPool) {
    let fx = SimulatedFxProvider::new();

    create_payment(&pool, &fx, "key-conflict", req()).await.unwrap();

    let mut other = req();
    other.source_amount = dec!(500000); // different amount, same key

    let err = create_payment(&pool, &fx, "key-conflict", other)
        .await
        .unwrap_err();

    assert!(matches!(err, AppError::IdempotencyConflict));
}

#[sqlx::test(migrations = "../migrations")]
async fn insufficient_balance_rejects_and_writes_no_ledger(pool: PgPool) {
    let fx = SimulatedFxProvider::new();

    let mut body = req();
    body.source_amount = dec!(999_999_999); // Lagos has 100M

    let err = create_payment(&pool, &fx, "key-broke", body).await.unwrap_err();

    match err {
        AppError::InsufficientBalance {
            balance,
            requested,
            currency,
        } => {
            assert_eq!(balance, dec!(100000000));
            assert_eq!(requested, dec!(999999999));
            assert_eq!(currency, "NGN");
        }
        other => panic!("expected InsufficientBalance, got {other:?}"),
    }

    // No transaction created.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM transactions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);

    // Sender ledger untouched: only the seed deposit entry exists.
    let entries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ledger_entries WHERE account_id = $1",
    )
    .bind(Uuid::from_str(LAGOS_NGN_ACCOUNT).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(entries, 1);
}

#[sqlx::test(migrations = "../migrations")]
async fn same_currency_pair_rejected(pool: PgPool) {
    let fx = SimulatedFxProvider::new();

    let mut body = req();
    body.destination_currency = "NGN".into();

    let err = create_payment(&pool, &fx, "key-same-ccy", body)
        .await
        .unwrap_err();

    assert!(matches!(err, AppError::BadRequest(_)));
}
