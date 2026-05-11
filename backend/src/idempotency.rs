use serde_json::Value;
use sqlx::PgConnection;

// Outcome of attempting to claim an idempotency key.
//
// The DB unique index on (scope, key) is what actually serializes
// concurrent dupes — application-level "check then insert" loses to races.
// See learn/concepts/idempotency.md.
pub enum IdempotencyOutcome {
    // First time we've seen this key. Caller should do the work.
    Proceed,
    // Key already exists with the same request hash AND a stored response.
    // Return the cached response byte-for-byte.
    Replay(Value),
    // Key exists with a DIFFERENT request hash — client bug. Return 422.
    Conflict,
}

// `INSERT ... ON CONFLICT DO NOTHING` is the canonical pattern. Two
// concurrent requests with the same key both reach the INSERT; the unique
// index forces them to serialize, then exactly one row exists. The "loser"
// reads the existing row and either replays or conflicts.
//
// Rows live for 24h. `idempotency_keys.expires_at` is set on insert; a
// periodic cleanup job (not built yet — production-readiness) would prune
// expired rows.
pub async fn check(
    conn: &mut PgConnection,
    scope: &str,
    key: &str,
    request_hash: &str,
) -> Result<IdempotencyOutcome, sqlx::Error> {
    let inserted = sqlx::query(
        "INSERT INTO idempotency_keys (scope, key, request_hash, expires_at)
         VALUES ($1, $2, $3, NOW() + INTERVAL '24 hours')
         ON CONFLICT (scope, key) DO NOTHING",
    )
    .bind(scope)
    .bind(key)
    .bind(request_hash)
    .execute(&mut *conn)
    .await?
    .rows_affected();

    if inserted > 0 {
        return Ok(IdempotencyOutcome::Proceed);
    }

    // Conflict path: read the existing row and decide replay vs. 422.
    let row: (String, Option<Value>) = sqlx::query_as(
        "SELECT request_hash, response_body
         FROM idempotency_keys
         WHERE scope = $1 AND key = $2",
    )
    .bind(scope)
    .bind(key)
    .fetch_one(&mut *conn)
    .await?;

    if row.0 != request_hash {
        return Ok(IdempotencyOutcome::Conflict);
    }

    match row.1 {
        Some(body) => Ok(IdempotencyOutcome::Replay(body)),
        // Same hash, no stored response. With single-DB-transaction handlers
        // this can only happen if the original handler crashed mid-flight,
        // in which case its INSERT was rolled back — so we wouldn't reach
        // this branch. Treat it defensively as a conflict so we don't double-act.
        None => Ok(IdempotencyOutcome::Conflict),
    }
}
