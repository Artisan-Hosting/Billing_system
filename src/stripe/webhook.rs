//! Stripe webhook signature verification.
//!
//! HMAC-SHA256 over `"{timestamp}.{raw_body}"`, keyed by the webhook
//! signing secret, compared via the `hmac` crate's own constant-time
//! `Mac::verify_slice` rather than a hand-rolled byte comparison. A
//! timestamp outside `tolerance_secs` is refused as a possible replay --
//! both checks match Stripe's own documented verification procedure.
//!
//! Never touches [`super::StripeClient`] or the database: this is pure,
//! and its only job is "is this really Stripe." What happens once a
//! webhook is trusted (looking up and updating a `payment_intents` row)
//! belongs to the gRPC handler that calls this, not here.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::{Error, Result};

/// Verifies a `Stripe-Signature` header against `payload` and
/// `webhook_secret`. Checks every `v1=` signature present in the header
/// (Stripe sends more than one during a signing-secret rotation window)
/// and accepts if any one matches, per Stripe's own recommendation.
pub fn verify_signature(payload: &[u8], signature_header: &str, webhook_secret: &str, tolerance_secs: i64) -> Result<()> {
    let mut timestamp: Option<i64> = None;
    let mut signatures: Vec<&str> = Vec::new();

    for part in signature_header.split(',') {
        match part.split_once('=') {
            Some(("t", t)) => timestamp = t.parse().ok(),
            Some(("v1", sig)) => signatures.push(sig),
            _ => {}
        }
    }

    let timestamp =
        timestamp.ok_or_else(|| Error::Invalid("stripe signature header has no timestamp".to_owned()))?;
    if signatures.is_empty() {
        return Err(Error::Invalid("stripe signature header has no v1 signature".to_owned()));
    }

    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > tolerance_secs {
        return Err(Error::Invalid("stripe webhook timestamp outside tolerance (possible replay)".to_owned()));
    }

    // Built as bytes, not through a `String`: a lossy UTF-8 conversion of
    // `payload` would silently substitute invalid sequences and change
    // what gets signed, turning a legitimate signature into a mismatch.
    // JSON from Stripe is always valid UTF-8 in practice, but there is no
    // reason to route through a representation that could get this wrong.
    let mut signed_payload = format!("{timestamp}.").into_bytes();
    signed_payload.extend_from_slice(payload);

    let mac = Hmac::<Sha256>::new_from_slice(webhook_secret.as_bytes())
        .map_err(|e| Error::Stripe(format!("invalid webhook secret: {e}")))?;

    for signature in &signatures {
        let Ok(bytes) = hex::decode(signature) else { continue };
        if mac.clone().chain_update(&signed_payload).verify_slice(&bytes).is_ok() {
            return Ok(());
        }
    }

    Err(Error::Invalid("stripe webhook signature did not match".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "whsec_test_secret";

    fn sign(payload: &[u8], timestamp: i64, secret: &str) -> String {
        let mut signed_payload = format!("{timestamp}.").into_bytes();
        signed_payload.extend_from_slice(payload);
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&signed_payload);
        format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn a_correctly_signed_payload_verifies() {
        let payload = br#"{"id":"evt_1","type":"payment_intent.succeeded"}"#;
        let now = chrono::Utc::now().timestamp();
        let header = sign(payload, now, SECRET);
        assert!(verify_signature(payload, &header, SECRET, 300).is_ok());
    }

    #[test]
    fn a_tampered_payload_is_rejected() {
        let payload = br#"{"id":"evt_1","type":"payment_intent.succeeded"}"#;
        let now = chrono::Utc::now().timestamp();
        let header = sign(payload, now, SECRET);
        let tampered = br#"{"id":"evt_1","type":"payment_intent.payment_failed"}"#;
        assert!(verify_signature(tampered, &header, SECRET, 300).is_err());
    }

    #[test]
    fn the_wrong_secret_is_rejected() {
        let payload = br#"{"id":"evt_1"}"#;
        let now = chrono::Utc::now().timestamp();
        let header = sign(payload, now, "whsec_a_different_secret");
        assert!(verify_signature(payload, &header, SECRET, 300).is_err());
    }

    #[test]
    fn a_stale_timestamp_is_rejected_as_a_possible_replay() {
        let payload = br#"{"id":"evt_1"}"#;
        let old = chrono::Utc::now().timestamp() - 3600;
        let header = sign(payload, old, SECRET);
        assert!(verify_signature(payload, &header, SECRET, 300).is_err());
    }

    #[test]
    fn a_malformed_header_is_rejected_not_panicked_on() {
        let payload = b"{}";
        assert!(verify_signature(payload, "not,a,valid,header", SECRET, 300).is_err());
        assert!(verify_signature(payload, "", SECRET, 300).is_err());
        assert!(verify_signature(payload, "t=notanumber,v1=abc", SECRET, 300).is_err());
    }

    #[test]
    fn a_second_v1_signature_during_secret_rotation_still_verifies() {
        let payload = br#"{"id":"evt_1"}"#;
        let now = chrono::Utc::now().timestamp();
        // Stripe sends both the old and new secret's signature during a
        // rotation window; verification must accept either.
        let old_sig = sign(payload, now, "whsec_old_secret");
        let old_sig_only = old_sig.split(',').nth(1).unwrap();
        let header = format!("t={now},{old_sig_only},v1=deadbeef");
        assert!(verify_signature(payload, &header, "whsec_old_secret", 300).is_ok());
    }
}
