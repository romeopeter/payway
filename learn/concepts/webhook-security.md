# Webhook Security

> **What this is for:** the security primitives behind [`backend/src/webhook_signature.rs`](../../backend/src/webhook_signature.rs) and the surrounding handler. After this you should be able to explain (a) why HMAC over raw bytes, not parsed JSON, (b) why constant-time compare matters, (c) the "always 200" rule and the side channels it closes, (d) what replay attacks look like and why our dedup mitigates them.

---

## Why HMAC at all?

A webhook endpoint is a public URL. Anything on the internet can send it a POST request. Without authentication, anyone can claim to be the payment provider and tell us "payment completed" for a transaction reference they guessed.

HMAC (Hash-based Message Authentication Code) is the simplest authentication mechanism for this:

- We share a secret with the provider once, out of band (during integration setup).
- When the provider sends a webhook, it computes `HMAC-SHA256(secret, body)` and includes the result in a header.
- We compute the same MAC on our side and compare. If they match, we know the message came from someone who has the secret.

The provider doesn't need to manage TLS client certificates or rotate API keys per request. The secret is symmetric — both sides hold the same value — which is why it must never be logged, never leak into error messages, and never be transmitted in cleartext outside the initial handshake.

---

## Why HMAC over **raw bytes**, not parsed JSON

This is the single subtlest mistake people make.

The provider computes `HMAC-SHA256(secret, body_bytes)` where `body_bytes` is the exact byte sequence they put on the wire — including every space, every key order, every numeric format.

If we receive the request, parse the JSON into a struct, and then re-serialize that struct to compute our HMAC, we'll almost certainly get **different bytes** than the provider did. Reasons:

- **Key ordering.** JSON objects have no defined order. Our serializer might emit `{"a":1,"b":2}`; theirs might emit `{"b":2,"a":1}`. Different bytes; different MAC.
- **Whitespace.** Pretty-printed JSON vs compact. Different bytes; different MAC.
- **Number formatting.** `1.0` vs `1`. `1e2` vs `100`. Different bytes; different MAC.
- **Unicode escaping.** `"é"` vs `"é"`. Different bytes; different MAC.

The MAC is over the exact bytes the provider signed. To verify, we must hold onto those exact bytes until verification completes.

In our handler:
```rust
async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,        // <-- raw bytes, not Json<T>
) -> Result<...>
```

`Bytes` is Axum's body extractor that gives us the raw request bytes. We **don't** use `Json<WebhookPayload>` here, because that would parse first and we'd lose the original bytes.

Parsing happens *after* signature verification, inside [`backend/src/domain/webhooks.rs`](../../backend/src/domain/webhooks.rs):
```rust
let payload: WebhookPayload = serde_json::from_slice(raw_body)?;
```

By the time we parse, we've already verified the integrity of the bytes. Tampering would have been caught.

---

## Why **constant-time** comparison

Naive HMAC check:
```rust
let expected_mac = compute_hmac(secret, body);
if expected_mac == provided_mac {  // <-- BUG
    // proceed
}
```

`==` on byte slices (or strings) short-circuits on the first different byte. The comparison is faster for inputs that differ early than for inputs that differ late. An attacker measuring response time can guess one byte of the MAC at a time — once they find a byte that makes the comparison "slower," they know that byte is correct, and move to the next. With sufficient samples (network jitter is the limiting factor; can be averaged out), they recover the entire MAC and can forge messages.

This is a real attack. The CVE history of OAuth and webhook libraries has more than a few "timing leak in HMAC compare" entries.

The fix is **constant-time compare** — always inspect every byte, regardless of where mismatches occur. The `hmac` crate gives us this:

```rust
mac.verify_slice(&expected)  // constant-time inside
```

Our [`webhook_signature.rs`](../../backend/src/webhook_signature.rs) uses `verify_slice` and never falls back to `==` on the MAC bytes. The unit tests don't directly verify the timing property (you can't reliably test that in unit-test scale), but they verify correctness, and using the standard `verify_slice` is the way to inherit the timing guarantee.

---

## Why **always 200**, in detail

The spec explicitly calls this out for unknown transaction references. The reasoning generalizes to everything our handler accepts.

### Operational: provider retry storms

Payment providers retry **indefinitely** on any non-2xx response. Their assumption is "the webhook didn't get through; eventually it will." This is the right design for delivery reliability.

But if our handler returns 404 for an unknown reference, the provider retries forever. A single misrouted webhook becomes a sustained DDoS on `/webhooks/provider`. Multiply by every misroute across the lifetime of the integration and the endpoint is buried under retries that will never succeed.

Returning 200 + logging is the correct ack: "we received it, we have it on file, stop retrying." Even if the content was useless to us, the *receipt* was successful.

### Security: enumeration oracle

Response codes leak existence. If our handler returns:
- 200 for a known reference,
- 404 for an unknown reference,

then an attacker who can hit the endpoint can enumerate which transaction references exist on our system. They just send `POST /webhooks/provider` with arbitrary references and watch the response codes. Over time, they harvest the set of valid references — useful for follow-on attacks (replay, social engineering, internal mapping).

Always-200 closes the side channel. From the attacker's perspective, every request looks the same.

### What we return 200 for

- Valid signature + valid body + processed → 200
- Valid + duplicate event_id → 200
- Valid + transaction already settled → 200
- Valid + unknown `provider_reference` → 200
- Valid signature + malformed JSON → 200
- Invalid signature → 200
- Missing signature → 200

### What still returns 5xx

- DB unreachable → 500 (provider retries — appropriate, this is transient)
- Some other unexpected panic in the handler → Axum returns 500 (provider retries)

The provider's retry mechanism is correct *for* transient infrastructure failures. We just don't want them retrying on permanent "this will never succeed" outcomes.

---

## Replay attacks and what mitigates them

An attacker who captures a valid webhook (e.g. from a man-in-the-middle position before HTTPS, or from a log file leak) could replay it later. The signature would still verify — same bytes, same secret. Without further protection, we'd process the same event again and double-credit or double-reverse.

Two mechanisms in our design mitigate this:

1. **The unique index on `(provider, provider_event_id)`.** Same event_id twice = same event. The second delivery hits the unique constraint, `ON CONFLICT DO NOTHING` makes the INSERT a no-op, we return `Duplicate`. Even with a perfectly-replayed signature, nothing happens.
2. **The state machine guard.** Even if an attacker replayed with a *different* event_id (somehow), our `if txn.status != "processing"` check rejects it. A transaction can transition `processing → completed` only once.

What's NOT mitigated:
- **Replays delivered before the original.** If the attacker gets the bytes and races ahead of the provider's actual delivery, they'd successfully process the event. This requires capturing the bytes in transit, which TLS prevents in normal operation.
- **Indefinitely-old replays.** We don't enforce a `timestamp` window on the webhook payload. Production-grade webhook handlers usually reject events with timestamps more than ~5 minutes from the current clock (Stripe does this). This is a small enhancement we could add — note for production readiness.

---

## Things that are NOT in our model but matter in production

- **Webhook secret rotation.** Real systems rotate secrets periodically. The endpoint typically accepts signatures from the *current* and *previous* secret during a rotation window. We have one secret.
- **Per-event timestamp window.** Per above.
- **Per-IP rate limiting** at the network layer (nginx, Cloudflare). Without this, an attacker can spam the endpoint with bogus signed requests; each one creates a `webhook_events` row. With rate limiting they can't.
- **TLS mutual auth or IP allowlisting** in addition to HMAC. Belt and suspenders. The provider gives us an allowlist of their egress IPs; the load balancer enforces it. HMAC remains the primary auth.
- **Logging the secret accidentally.** We use `Arc<String>` for the secret in `AppState`. In production this should be `secrecy::Secret<String>` which has a `Debug` impl that prints `[REDACTED]`. The discipline of "never log AppState" is fragile; the type system can enforce it. Production-readiness item.

---

## Cross-references

- [`backend/src/webhook_signature.rs`](../../backend/src/webhook_signature.rs) — HMAC verifier with unit tests
- [`backend/src/domain/webhooks.rs`](../../backend/src/domain/webhooks.rs) — usage in the processor
- [`backend/src/routes/webhooks.rs`](../../backend/src/routes/webhooks.rs) — the `Bytes` extractor that preserves raw bytes
- [`learn/webhooks.md`](../webhooks.md) — the end-to-end flow that builds on these primitives
- [`learn/code-review-junior-webhook.md`](../code-review-junior-webhook.md) — concrete examples of getting these wrong
