use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum SignatureError {
    // Single variant on purpose. Distinguishing "bad hex" from "wrong MAC"
    // would give an attacker a signal to refine probes. Caller doesn't need
    // to know which kind of invalid it was — just that it failed.
    #[error("invalid webhook signature")]
    Invalid,
}

/// Verify an HMAC-SHA256 signature in constant time.
///
/// * `secret`     — the shared HMAC key (UTF-8 bytes of the configured secret)
/// * `body`       — the raw request bytes the provider signed
/// * `signature_hex` — hex-encoded MAC from the `X-Webhook-Signature` header
///
/// Returns `Ok(())` on a valid signature, `Err(SignatureError::Invalid)` otherwise.
///
/// Uses `Mac::verify_slice`, which compares in constant time — crucial because
/// `==` on byte slices short-circuits on first difference and leaks timing
/// information that lets an attacker recover a MAC byte by byte. See
/// learn/concepts/webhook-security.md.
pub fn verify_hmac_sha256(
    secret: &[u8],
    body: &[u8],
    signature_hex: &str,
) -> Result<(), SignatureError> {
    let expected = hex::decode(signature_hex.trim()).map_err(|_| SignatureError::Invalid)?;

    // new_from_slice accepts any-length keys (HMAC pads/truncates as needed).
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| SignatureError::Invalid)?;
    mac.update(body);
    mac.verify_slice(&expected).map_err(|_| SignatureError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    // We compute a known-good MAC, then verify. If verify_slice ever stops being
    // constant-time we won't catch that here (you can't test timing in a unit
    // test reliably), but we do catch correctness regressions.
    fn make_sig(secret: &[u8], body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    #[test]
    fn accepts_a_correct_signature() {
        let secret = b"shared-secret";
        let body = br#"{"event_id":"evt_1"}"#;
        let sig = make_sig(secret, body);
        assert!(verify_hmac_sha256(secret, body, &sig).is_ok());
    }

    #[test]
    fn rejects_wrong_signature() {
        let secret = b"shared-secret";
        let body = br#"{"event_id":"evt_1"}"#;
        let mut sig = make_sig(secret, body);
        // Flip one hex digit.
        sig.replace_range(0..1, "f");
        assert!(verify_hmac_sha256(secret, body, &sig).is_err());
    }

    #[test]
    fn rejects_signature_for_different_body() {
        let secret = b"shared-secret";
        let sig = make_sig(secret, b"original");
        assert!(verify_hmac_sha256(secret, b"tampered", &sig).is_err());
    }

    #[test]
    fn rejects_signature_with_different_secret() {
        let body = br#"{"event_id":"evt_1"}"#;
        let sig = make_sig(b"secret-a", body);
        assert!(verify_hmac_sha256(b"secret-b", body, &sig).is_err());
    }

    #[test]
    fn rejects_malformed_hex() {
        let secret = b"shared-secret";
        let body = b"x";
        assert!(verify_hmac_sha256(secret, body, "not-hex-at-all").is_err());
    }
}
