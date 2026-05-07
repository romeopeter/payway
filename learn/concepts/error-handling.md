# Error Handling in Rust (for someone coming from JS / Python)

> **What this is for:** Rust's error story is the first thing you'll meet that has *no clean JS or Python analogue*. Spending 15 minutes here will save you hours of staring at compiler messages later. We'll work from the JS pattern you know, show what Rust does instead, and walk through every error pattern used in our [`backend/src/error.rs`](../../backend/src/error.rs).

---

## Step 1: Rust does not have exceptions

In JS:
```js
try {
  const user = await getUser(id);
  return user.name;
} catch (e) {
  return null;
}
```

The `catch` is invisible from the function's signature. `getUser` could throw anything: a `TypeError`, a `NetworkError`, a string, `42`. You don't know until it happens. JS programmers have learned to live with this; some consider it a feature.

In Rust, **functions that can fail say so in their return type.** There is no hidden exception channel. If a function returns `Result<T, E>`, you must handle the error; if it returns `T`, it cannot fail (in the "returned an error" sense — it can still panic on a logic bug).

This is the single biggest mental shift. Once it clicks, you stop missing exceptions.

---

## Step 2: `Result<T, E>` and `Option<T>`

Two enums in the standard library carry essentially all of Rust's "this might not work" signaling.

```rust
enum Result<T, E> {
    Ok(T),
    Err(E),
}

enum Option<T> {
    Some(T),
    None,
}
```

- `Result<T, E>` is for fallible operations. Reading a file: `Result<String, io::Error>`. Parsing a number: `Result<i32, ParseIntError>`. Inserting a row: `Result<PgQueryResult, sqlx::Error>`.
- `Option<T>` is for "this value might not exist." Looking up a key in a HashMap: `Option<&V>`. The first character of a string: `Option<char>`. JS's `undefined` and Python's `None` are both "everywhere by default"; `Option` is "explicit on the type."

You handle them with `match`, or with helpers like `.unwrap_or(default)`, `.map(|x| ...)`, `.and_then(|x| ...)`. You'll see all of these in our codebase.

---

## Step 3: The `?` operator

This is the single most important piece of Rust syntax for day-to-day work.

In JS, propagating errors looks like:
```js
const a = await callA();          // throws on error — propagates "for free"
const b = await callB(a);
return b;
```

In Rust without `?`:
```rust
let a = match call_a().await {
    Ok(v) => v,
    Err(e) => return Err(e),
};
let b = match call_b(a).await {
    Ok(v) => v,
    Err(e) => return Err(e),
};
Ok(b)
```

That's unbearable. So Rust has `?`:
```rust
let a = call_a().await?;
let b = call_b(a).await?;
Ok(b)
```

`?` desugars to "if `Ok(v)`, give me `v`; if `Err(e)`, return `Err(e.into())` from this function." The `.into()` is the trick: `?` will *convert* the error type if there's a `From` impl available. Which brings us to:

---

## Step 4: `thiserror` and the conversion pattern

In our [`error.rs`](../../backend/src/error.rs):
```rust
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,

    #[error("invalid input: {0}")]
    BadRequest(String),

    #[error("idempotency key reused with a different request body")]
    IdempotencyConflict,

    #[error("database error")]
    Database(#[from] sqlx::Error),

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}
```

What's happening here:

- **`#[derive(thiserror::Error)]`** — the `thiserror` crate's macro. It reads our enum + the `#[error("...")]` annotations and generates two trait impls for free:
  - `impl Display for AppError` — printing the error gives the `#[error("...")]` string.
  - `impl Error for AppError` — makes it usable wherever a generic error is needed.
- **`#[from] sqlx::Error`** on the `Database` variant — generates `impl From<sqlx::Error> for AppError`. So when our handler does `pool.execute(...).await?` and gets back an `Err(sqlx::Error)`, the `?` operator silently converts it to `AppError::Database(sqlx_err)` and returns that.
- **`#[from] anyhow::Error`** on `Internal` — same trick for the catch-all type, used when we wrap an error with extra context via `anyhow`.

The payoff: handlers stay clean.
```rust
async fn create_payment(...) -> Result<Json<...>, AppError> {
    let row = sqlx::query!("INSERT ...").fetch_one(&pool).await?;
    //                                                       ^ converts sqlx::Error -> AppError::Database
    Ok(Json(...))
}
```

---

## Step 5: `IntoResponse` — turning errors into HTTP responses

Axum needs a way to say "this handler returned an error; render it as an HTTP response." The mechanism is the `IntoResponse` trait:

```rust
impl IntoResponse for AppError {
    fn into_response(self) -> Response { /* ... */ }
}
```

In our impl, the pattern match is doing real work:

```rust
match &self {
    AppError::NotFound       => (404, self.to_string()),
    AppError::BadRequest(_)  => (400, self.to_string()),
    AppError::IdempotencyConflict => (422, self.to_string()),
    AppError::Database(_) | AppError::Internal(_) => {
        tracing::error!(error = ?self, "internal error");
        (500, "internal server error".to_string())
    }
}
```

- **Client errors** (`NotFound`, `BadRequest`, `IdempotencyConflict`) leak their message to the response body. The user is supposed to see it.
- **Server errors** (`Database`, `Internal`) get logged with full detail and return a generic message. The user sees `500 internal server error`; engineers see the stack trace in logs.

In a payments context this matters a lot. `FOREIGN KEY constraint violated on idempotency_keys_pkey` is a useful message for me; for the user it's a leak that reveals schema details and is also not actionable. The `IntoResponse` impl is the one place that line is drawn.

---

## Step 6: `anyhow` vs. `thiserror` — when to use which

Rust has two error-handling crates that look similar but have different jobs.

- **`thiserror`** — for **defined error types** with **structured variants** that other code will pattern-match on. We use it for `AppError` because the `IntoResponse` impl pattern-matches on variants.
- **`anyhow`** — for "I just want to bubble this up with a message; I don't need anyone downstream to introspect it." It's a wrapper that holds any `Error` type and adds context.

In `main.rs`:
```rust
let pool = db::pool(&config.database_url)
    .await
    .context("connecting to postgres")?;
```

`Context::context` is from `anyhow` — it wraps the `sqlx::Error` with the message "connecting to postgres" and returns an `anyhow::Error`. If pool creation fails, the user-facing message becomes "connecting to postgres: <underlying sqlx error>." For startup code where nothing pattern-matches the result, this is exactly right.

**Rule of thumb:**
- Library / domain code → `thiserror` enums. Variants are part of the API.
- Application / glue code → `anyhow::Result`. You just want errors to flow up with context.

Our `AppError::Internal` variant uses `#[from] anyhow::Error`, which is the bridge: any `anyhow::Error` from glue code becomes an `Internal` variant in `AppError`.

---

## Step 7: When to `panic!` (almost never)

Rust has two ways out of a function: return, or panic. Panic is the "this isn't a recoverable error, this is a bug" exit. In production, panics in async tasks are caught by tokio, the task dies, and the rest of the system continues. But in a request handler, a panic = a 500 error and the request is gone.

**Don't panic in a payments handler ever, including from `unwrap()` and `expect()`.** Those are panic shortcuts for "I'm certain this can't fail" — and in payments, *nothing* falls under that. If you find yourself reaching for `.unwrap()`, change the function signature to return `Result` instead.

The exception is startup code: if `DATABASE_URL` isn't set, panicking (or `?`-bubbling to `main`'s error return, which prints and exits) is correct. The system can't function; failing fast is the right behavior.

In `main.rs` we use `?` which bubbles to `anyhow::Result<()>` and prints. We don't `unwrap`. Even at startup, structured errors are friendlier than "thread 'main' panicked at..."

---

## Step 8: Where this all comes together

When `POST /payments` fails because the idempotency key was reused with a different body:

1. The handler does some pre-flight checks and detects the conflict.
2. It returns `Err(AppError::IdempotencyConflict)`.
3. The `?` operator (or explicit return) sends the `AppError` up.
4. Axum sees the `Result<_, AppError>` from the handler.
5. Axum calls `AppError::into_response()`.
6. The pattern matches `IdempotencyConflict` → `(422, "idempotency key reused...")`.
7. The response goes out as JSON: `{"error":"idempotency key reused with a different request body"}` with status 422.
8. `tracing` middleware logs the request + response, including our `x-request-id`.

No exceptions, no `try`/`catch`, no leaking internals. The whole error path is visible in the type signatures.

---

## Cheat sheet

| You want to...                                  | You write...                                   |
|--------------------------------------------------|------------------------------------------------|
| Mark a function as fallible                      | `fn foo() -> Result<T, MyError>`               |
| Propagate an error up                            | `let x = thing()?;`                            |
| Convert error type along the way                 | `?` + a `From` impl (via `thiserror` `#[from]`)|
| Add a message to an error                        | `.context("doing the thing")?`                  |
| Define a custom error type with variants         | `#[derive(thiserror::Error)] enum`              |
| Just bubble *some* error up with no introspection| `-> anyhow::Result<T>`                          |
| Map error to HTTP response                       | `impl IntoResponse for MyError`                 |
| Handle error inline                              | `match result { Ok(v) => ..., Err(e) => ... }`  |
| Provide a default on error                       | `result.unwrap_or(default)`                     |
| Crash the program because the bug is unrecoverable | `panic!("...")` — but almost never in payments|

---

## What to read next

- The cheat sheet above is enough to read 90% of [`backend/src/error.rs`](../../backend/src/error.rs) and the handlers we'll write in 2b.
- Official deeper reading: [Rust by Example — Error Handling](https://doc.rust-lang.org/rust-by-example/error.html). Solid; ignore anything that recommends string-based errors.
- When something doesn't compile and the error mentions trait bounds: 80% of the time it's "this type doesn't implement `Send` or `Sync` because of a non-`Send` lock guard held across an `.await`." We'll cover that one when it bites us.
