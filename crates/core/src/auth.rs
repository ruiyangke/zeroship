//! Auth utilities: control key validation, API key hashing, bearer extraction,
//! HMAC signing for cross-service identity propagation.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

pub const ZEROSHIP_USER_MAX_AGE_SECS: u64 = 60;
pub const ZEROSHIP_USER_FUTURE_SKEW_SECS: u64 = 5;

/// Constant-time comparison using XOR fold to prevent timing attacks.
/// Returns true if `provided` and `expected` are equal.
pub fn validate_control_key(provided: &str, expected: &str) -> bool {
    let a = provided.as_bytes();
    let b = expected.as_bytes();

    // If lengths differ, we still do a comparison on the shorter slice
    // but the length mismatch itself sets the result to false — without
    // branching early so the timing is uniform for a fixed `expected` length.
    let len_ok = a.len() == b.len();

    // XOR every byte of the shorter of the two slices.  Using the expected
    // length as the iteration bound leaks the expected length (acceptable —
    // the expected key length is not secret), but does NOT leak whether the
    // provided key is longer or shorter.
    let min_len = a.len().min(b.len());
    let diff: u8 = a[..min_len]
        .iter()
        .zip(b[..min_len].iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));

    len_ok && diff == 0
}

/// SHA-256 hash of `key`, returned as a lowercase hex string.
/// Used to store API key hashes in the routing table instead of plaintext.
pub fn hash_api_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Constant-time validation of a provided API key against its stored SHA-256 hash.
pub fn validate_api_key(provided: &str, stored_hash: &str) -> bool {
    let computed = hash_api_key(provided);
    validate_control_key(&computed, stored_hash)
}

/// Strip the `Bearer ` prefix from an Authorization header value.
/// Returns `None` if the header does not start with `"Bearer "`.
pub fn extract_bearer(header: &str) -> Option<&str> {
    header.strip_prefix("Bearer ")
}

// ---------------------------------------------------------------------------
// HMAC-SHA256 signing — used to sign forwarded identity across trust boundaries
// ---------------------------------------------------------------------------

/// Compute an HMAC-SHA256 over `payload` with `key`, returned as the
/// raw 32-byte tag.
///
/// Used by the federation stash cookie (auth: `ui::oauth_stash`),
/// the pending-link token (auth: `identity::linker`), and the
/// gateway-side RP-callback stash (gateway: `oidc_rp`). Each
/// previously inlined the same three lines — keep it in one place so
/// the constant-time-compare wrappers above stay co-located with the
/// MAC primitive.
#[must_use]
pub fn hmac_sha256(key: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(payload);
    mac.finalize().into_bytes().into()
}

/// Compute an HMAC-SHA256 over `payload` with `key`, returned as lowercase hex.
#[must_use]
pub fn hmac_sha256_hex(key: &[u8], payload: &[u8]) -> String {
    hex::encode(hmac_sha256(key, payload))
}

/// Constant-time verify of `expected_hex` against `payload` HMAC-signed with `key`.
#[must_use]
pub fn verify_hmac_sha256_hex(key: &[u8], payload: &[u8], expected_hex: &str) -> bool {
    let computed = hmac_sha256_hex(key, payload);
    validate_control_key(&computed, expected_hex)
}

/// Sign a `ZeroShip-User` JSON payload as:
/// `<base64(json)>.<request_id>.<issued_at_unix_secs>.<hex-hmac>`.
///
/// The HMAC covers the first three segments, binding the identity to one
/// gateway dispatch request and a short issuance window.
#[must_use]
pub fn sign_zeroship_user_header_at(
    key: &[u8],
    user_json: &[u8],
    request_id: Uuid,
    issued_at_secs: u64,
) -> String {
    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(user_json);
    let signed = format!("{payload_b64}.{request_id}.{issued_at_secs}");
    let mac = hmac_sha256_hex(key, signed.as_bytes());
    format!("{signed}.{mac}")
}

#[must_use]
pub fn sign_zeroship_user_header(key: &[u8], user_json: &[u8], request_id: Uuid) -> String {
    let issued_at_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before Unix epoch")
        .as_secs();
    sign_zeroship_user_header_at(key, user_json, request_id, issued_at_secs)
}

/// Verify and decode a request-bound `ZeroShip-User` header.
#[must_use]
pub fn verify_zeroship_user_header(key: &[u8], header: &str) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    verify_zeroship_user_header_at(key, header, now)
}

/// Verify and decode a request-bound `ZeroShip-User` header at a fixed clock.
#[must_use]
pub fn verify_zeroship_user_header_at(key: &[u8], header: &str, now_secs: u64) -> Option<String> {
    verify_zeroship_user_header_parts_at(key, header, now_secs).map(|verified| verified.user_json)
}

/// Verify and decode a `ZeroShip-User` header for one expected dispatch request.
#[must_use]
pub fn verify_zeroship_user_header_for_request(
    key: &[u8],
    header: &str,
    expected_request_id: Uuid,
) -> Option<String> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    verify_zeroship_user_header_for_request_at(key, header, expected_request_id, now)
}

/// Verify and decode a `ZeroShip-User` header for one expected dispatch request
/// at a fixed clock.
#[must_use]
pub fn verify_zeroship_user_header_for_request_at(
    key: &[u8],
    header: &str,
    expected_request_id: Uuid,
    now_secs: u64,
) -> Option<String> {
    let verified = verify_zeroship_user_header_parts_at(key, header, now_secs)?;
    if verified.request_id != expected_request_id {
        return None;
    }
    Some(verified.user_json)
}

struct VerifiedZeroShipUserHeader {
    user_json: String,
    request_id: Uuid,
}

fn verify_zeroship_user_header_parts_at(
    key: &[u8],
    header: &str,
    now_secs: u64,
) -> Option<VerifiedZeroShipUserHeader> {
    let mut parts = header.split('.');
    let payload_b64 = parts.next()?;
    let request_id = parts.next()?;
    let issued_at = parts.next()?;
    let mac = parts.next()?;
    if parts.next().is_some()
        || payload_b64.is_empty()
        || request_id.is_empty()
        || issued_at.is_empty()
        || mac.is_empty()
    {
        return None;
    }
    let issued_at_secs = issued_at.parse::<u64>().ok()?;
    let request_id = Uuid::parse_str(request_id).ok()?;
    if now_secs.saturating_sub(issued_at_secs) > ZEROSHIP_USER_MAX_AGE_SECS {
        return None;
    }
    if issued_at_secs.saturating_sub(now_secs) > ZEROSHIP_USER_FUTURE_SKEW_SECS {
        return None;
    }

    let signed = format!("{payload_b64}.{request_id}.{issued_at}");
    if !verify_hmac_sha256_hex(key, signed.as_bytes(), mac) {
        return None;
    }

    let json = base64::engine::general_purpose::STANDARD
        .decode(payload_b64)
        .ok()?;
    Some(VerifiedZeroShipUserHeader {
        user_json: String::from_utf8(json).ok()?,
        request_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_JSON: &[u8] =
        br#"{"id":"usr_123","email":"a@example.com","name":"A","email_verified":true}"#;

    fn legacy_payload_only_user_header(key: &[u8], user_json: &[u8]) -> String {
        let payload_b64 = base64::engine::general_purpose::STANDARD.encode(user_json);
        let mac = hmac_sha256_hex(key, payload_b64.as_bytes());
        format!("{payload_b64}.{mac}")
    }

    fn legacy_payload_only_user_header_verifies(key: &[u8], header: &str) -> bool {
        let Some((payload_b64, mac)) = header.split_once('.') else {
            return false;
        };
        verify_hmac_sha256_hex(key, payload_b64.as_bytes(), mac)
    }

    #[test]
    fn control_key_equal() {
        assert!(validate_control_key("secret", "secret"));
    }

    #[test]
    fn control_key_different() {
        assert!(!validate_control_key("wrong", "secret"));
    }

    #[test]
    fn control_key_length_mismatch() {
        assert!(!validate_control_key("sec", "secret"));
        assert!(!validate_control_key("secretextra", "secret"));
    }

    #[test]
    fn hash_is_hex_sha256() {
        let h = hash_api_key("test");
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn api_key_roundtrip() {
        let key = "my-api-key";
        let stored = hash_api_key(key);
        assert!(validate_api_key(key, &stored));
        assert!(!validate_api_key("wrong-key", &stored));
    }

    #[test]
    fn bearer_extraction() {
        assert_eq!(extract_bearer("Bearer abc123"), Some("abc123"));
        assert_eq!(extract_bearer("Basic abc123"), None);
        assert_eq!(extract_bearer("Bearer "), Some(""));
        assert_eq!(extract_bearer(""), None);
    }

    #[test]
    fn hmac_roundtrip() {
        let key = b"shared-secret";
        let payload = b"user-payload";
        let mac = hmac_sha256_hex(key, payload);
        assert!(verify_hmac_sha256_hex(key, payload, &mac));
    }

    #[test]
    fn hmac_rejects_tampered_payload() {
        let key = b"shared-secret";
        let mac = hmac_sha256_hex(key, b"original");
        assert!(!verify_hmac_sha256_hex(key, b"tampered", &mac));
    }

    #[test]
    fn hmac_rejects_wrong_key() {
        let mac = hmac_sha256_hex(b"key-a", b"payload");
        assert!(!verify_hmac_sha256_hex(b"key-b", b"payload", &mac));
    }

    /// Raw 32-byte HMAC tag matches the hex-encoded form bit-for-bit.
    /// Regression test for the dedupe in
    /// `auth: ui::oauth_stash`/`identity::linker` and `gateway: oidc_rp`
    /// — if `hmac_sha256` ever drifts from `hmac_sha256_hex`, every
    /// federation cookie + pending-link token signed under one and
    /// verified under the other would silently reject. The fixture is a
    /// known-answer test from RFC 4231 §4.2 (HMAC-SHA-256, key 20×0x0b,
    /// data "Hi There").
    #[test]
    fn hmac_raw_matches_hex_and_rfc4231_kat() {
        let key = [0x0b_u8; 20];
        let data = b"Hi There";
        let raw = hmac_sha256(&key, data);
        let hex_form = hmac_sha256_hex(&key, data);
        assert_eq!(hex::encode(raw), hex_form);
        // RFC 4231 §4.2 expected output.
        assert_eq!(
            hex_form,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn zeroship_user_header_accepts_fresh_request_bound_header() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let now = 1_900_000_000;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, now);

        assert_eq!(
            verify_zeroship_user_header_at(key, &header, now),
            Some(String::from_utf8(USER_JSON.to_vec()).unwrap())
        );
    }

    #[test]
    fn zeroship_user_header_rejects_old_issued_at() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let now = 1_900_000_000;
        let stale = now - 120;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, stale);

        assert_eq!(verify_zeroship_user_header_at(key, &header, now), None);

        let legacy_header = legacy_payload_only_user_header(key, USER_JSON);
        assert!(
            legacy_payload_only_user_header_verifies(key, &legacy_header),
            "payload-only validation had no timestamp to reject a replay"
        );
        assert_eq!(verify_zeroship_user_header_at(key, &legacy_header, now), None);
    }

    #[test]
    fn zeroship_user_header_rejects_future_issued_at() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let now = 1_900_000_000;
        let future = now + 60;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, future);

        assert_eq!(verify_zeroship_user_header_at(key, &header, now), None);
    }

    #[test]
    fn zeroship_user_header_rejects_rebound_request_id() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let other_request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0022").unwrap();
        let now = 1_900_000_000;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, now);
        let mut parts: Vec<&str> = header.split('.').collect();
        parts[1] = "018f6df3-43f7-7f68-84e0-4f1f9f5f0022";
        let rebound = parts.join(".");

        assert_eq!(verify_zeroship_user_header_at(key, &rebound, now), None);
        assert_eq!(
            verify_zeroship_user_header_at(
                key,
                &sign_zeroship_user_header_at(key, USER_JSON, other_request_id, now),
                now,
            ),
            Some(String::from_utf8(USER_JSON.to_vec()).unwrap())
        );
    }

    #[test]
    fn zeroship_user_header_rejects_replay_on_different_request() {
        let key = b"worker-secret";
        let request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0021").unwrap();
        let other_request_id = Uuid::parse_str("018f6df3-43f7-7f68-84e0-4f1f9f5f0022").unwrap();
        let now = 1_900_000_000;
        let header = sign_zeroship_user_header_at(key, USER_JSON, request_id, now);

        assert_eq!(
            verify_zeroship_user_header_for_request_at(key, &header, request_id, now),
            Some(String::from_utf8(USER_JSON.to_vec()).unwrap())
        );
        assert_eq!(
            verify_zeroship_user_header_for_request_at(key, &header, other_request_id, now),
            None
        );
    }
}
