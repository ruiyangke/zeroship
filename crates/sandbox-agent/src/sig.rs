//! Per-request HMAC-SHA256 verification (`auth.hmac-v1`).
//!
//! The agent's wire-protocol-v1 auth scheme. Defeats:
//!
//!   - **Replay** of a captured request beyond a 5-second window
//!   - **In-flight body / path / method tampering**
//!   - **Static-secret leak via wire capture** — the secret never
//!     travels, only HMACs of canonical strings do
//!
//! ## Wire format
//!
//! Three new request headers:
//!
//! ```text
//! X-Sbx-Timestamp: <unix_seconds>
//! X-Sbx-Nonce:     <hex 16 random bytes = 32 chars>
//! X-Sbx-Signature: <base64 HMAC-SHA256 over canonical>
//! ```
//!
//! `canonical` is a fixed-format string the controller and agent
//! BOTH compute identically:
//!
//! ```text
//! canonical = method  + "\n"
//!           + path    + "\n"
//!           + ts      + "\n"
//!           + nonce   + "\n"
//!           + sha256_hex(body)
//! ```
//!
//! Empty bodies use `sha256_hex(&[])` =
//! `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`.
//!
//! ## Verification rules (in order — all must pass)
//!
//! 1. **Timestamp present + parseable** as decimal `u64`.
//! 2. **Clock skew** — `|now - ts| <= SKEW_S` (5 s).
//! 3. **Nonce well-formed** — non-empty, length-bounded so an
//!    adversary can't pollute the cache with arbitrarily long keys.
//! 4. **Nonce not seen recently** — checked against a bounded LRU.
//!    The cache is keyed by the nonce string; entries TTL out at
//!    `NONCE_TTL_S` (30 s) so memory is bounded even under flood.
//! 5. **Signature constant-time-matches** the recomputed HMAC.
//!
//! Only after all five pass do we record the nonce as "seen", so a
//! garbage signature can't pollute the cache.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use lru::LruCache;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;

/// Maximum allowed clock skew between controller and agent.
pub const SKEW_S: u64 = 5;

/// How long a nonce stays in the replay cache. Together with
/// [`SKEW_S`], a request is reject-able for `NONCE_TTL_S - SKEW_S`
/// seconds after first use, and the timestamp window blocks anything
/// older than that anyway.
pub const NONCE_TTL_S: u64 = 30;

/// Bound on the LRU cache. ~10k requests/30s = ~333 RPS sustained
/// before older nonces age out — fine for an interactive agent.
pub const NONCE_CACHE_CAPACITY: usize = 10_000;

/// Bound on the nonce string we'll accept. Prevents cache pollution
/// via huge-key attacks even before the LRU eviction kicks in.
const MAX_NONCE_LEN: usize = 64;

/// SHA-256 of the empty byte string. Cached because every body-less
/// request hits this value and recomputing is wasteful.
const EMPTY_BODY_SHA256_HEX: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Reasons a signature fails. Each maps to a distinct audit event so
/// alerting can distinguish a clock-skew operator mistake from an
/// actual replay attack from a wrong-key controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthFail {
    /// `X-Sbx-Timestamp` missing or not a decimal integer.
    BadTimestamp,
    /// `|now - ts| > SKEW_S`.
    SkewTooLarge,
    /// `X-Sbx-Nonce` missing, empty, or longer than `MAX_NONCE_LEN`.
    BadNonce,
    /// Nonce already seen in the LRU within `NONCE_TTL_S`.
    ReplayedNonce,
    /// `X-Sbx-Signature` missing or not valid base64.
    BadSignatureEncoding,
    /// HMAC didn't match — wrong key, tampered body/path/method, etc.
    BadSignature,
}

impl AuthFail {
    /// Stable string used in audit log + tracing fields.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthFail::BadTimestamp => "bad-timestamp",
            AuthFail::SkewTooLarge => "skew-too-large",
            AuthFail::BadNonce => "bad-nonce",
            AuthFail::ReplayedNonce => "replayed-nonce",
            AuthFail::BadSignatureEncoding => "bad-signature-encoding",
            AuthFail::BadSignature => "bad-signature",
        }
    }
}

/// Verifier holds the HMAC key and a bounded replay cache.
///
/// Cloned cheaply via `Arc<Verifier>` in the application state.
pub struct Verifier {
    key: Zeroizing<Vec<u8>>,
    nonces: Mutex<LruCache<String, u64>>,
}

impl Verifier {
    /// Construct from raw key bytes. Caller is responsible for the
    /// secrecy of the input slice; Verifier copies into a Zeroizing
    /// buffer that scrubs on drop.
    pub fn new(key: Zeroizing<Vec<u8>>) -> Self {
        let cap = std::num::NonZeroUsize::new(NONCE_CACHE_CAPACITY)
            .expect("NONCE_CACHE_CAPACITY > 0");
        Self {
            key,
            nonces: Mutex::new(LruCache::new(cap)),
        }
    }

    /// Verify a request. Returns `Ok(())` on a valid signature, an
    /// [`AuthFail`] otherwise. Side effect on success: the nonce is
    /// recorded so a subsequent identical request is rejected as
    /// `ReplayedNonce`.
    ///
    /// Caller passes the request method (`"GET"`, `"POST"`, ...),
    /// the URL path (no query string), the **raw** body bytes (or
    /// empty slice for body-less requests), and the three header
    /// values: timestamp, nonce, signature.
    pub fn verify(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        ts_hdr: &str,
        nonce_hdr: &str,
        sig_hdr: &str,
    ) -> Result<(), AuthFail> {
        // 1. Timestamp parse + skew check.
        let ts: u64 = ts_hdr.parse().map_err(|_| AuthFail::BadTimestamp)?;
        let now = unix_now();
        if abs_diff(now, ts) > SKEW_S {
            return Err(AuthFail::SkewTooLarge);
        }

        // 2. Nonce shape check (cheap; rejects pollution attempts
        //    before we touch the LRU).
        if nonce_hdr.is_empty() || nonce_hdr.len() > MAX_NONCE_LEN {
            return Err(AuthFail::BadNonce);
        }
        if !nonce_hdr.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(AuthFail::BadNonce);
        }

        // 3. Decode the signature header.
        let presented = B64
            .decode(sig_hdr.as_bytes())
            .map_err(|_| AuthFail::BadSignatureEncoding)?;

        // 4. Compute the body hash and the canonical string. Hash
        //    the empty body as a constant rather than re-running
        //    SHA-256 on `&[]`.
        let body_hash_hex: String = if body.is_empty() {
            EMPTY_BODY_SHA256_HEX.to_string()
        } else {
            hex::encode(Sha256::digest(body))
        };
        let canonical =
            format!("{method}\n{path}\n{ts}\n{nonce_hdr}\n{body_hash_hex}");

        // 5. Recompute the HMAC.
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .expect("HMAC accepts any key length");
        mac.update(canonical.as_bytes());
        let expected = mac.finalize().into_bytes();

        // 6. Constant-time compare.
        if !bool::from(presented.ct_eq(expected.as_slice())) {
            return Err(AuthFail::BadSignature);
        }

        // 7. Nonce replay check. We do this LAST so that an
        //    attacker with a wrong key (signature mismatch) cannot
        //    pollute the cache by spamming valid-looking nonces.
        //    Insert under lock; another thread may have just won the
        //    race and inserted the same nonce between our test and
        //    our insert — `put` returns the old value, and we treat
        //    a present old value as a replay.
        let mut cache = self.nonces.lock().unwrap_or_else(|p| p.into_inner());
        evict_expired(&mut cache, now);
        if cache.contains(nonce_hdr) {
            return Err(AuthFail::ReplayedNonce);
        }
        cache.put(nonce_hdr.to_string(), ts);

        Ok(())
    }
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't expose the key bytes via Debug.
        f.debug_struct("Verifier")
            .field("key_bytes", &self.key.len())
            .field("nonces", &"<lru>")
            .finish()
    }
}

/// Sign a canonical message. **Test-only** — the agent is the
/// verifier; the controller is the signer in production. We expose
/// this here so handler tests don't have to reimplement HMAC-SHA256.
#[cfg(test)]
pub fn sign_for_test(
    key: &[u8],
    method: &str,
    path: &str,
    body: &[u8],
    ts: u64,
    nonce: &str,
) -> String {
    let body_hash_hex: String = if body.is_empty() {
        EMPTY_BODY_SHA256_HEX.to_string()
    } else {
        hex::encode(Sha256::digest(body))
    };
    let canonical = format!("{method}\n{path}\n{ts}\n{nonce}\n{body_hash_hex}");
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(canonical.as_bytes());
    B64.encode(mac.finalize().into_bytes())
}

/// Drop entries older than `NONCE_TTL_S`. Called inside the lock,
/// best-effort: walks until it finds a still-valid entry from the
/// LRU's least-recent end. (LRU entries are sorted by access order,
/// not insert order, so this may not catch the oldest by timestamp;
/// the LRU capacity bound is the real protection. TTL eviction is
/// just a memory-hygiene knob.)
fn evict_expired(cache: &mut LruCache<String, u64>, now: u64) {
    while let Some((_, ts)) = cache.peek_lru() {
        if now.saturating_sub(*ts) > NONCE_TTL_S {
            cache.pop_lru();
        } else {
            break;
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[inline]
fn abs_diff(a: u64, b: u64) -> u64 {
    if a >= b { a - b } else { b - a }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> Zeroizing<Vec<u8>> {
        Zeroizing::new(b"abcdefghijklmnopqrstuvwxyz012345".to_vec())
    }

    fn ts_now() -> u64 { unix_now() }

    #[test]
    fn valid_request_passes() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let nonce = "abc123";
        let sig = sign_for_test(&key(), "GET", "/files/x", b"", ts, nonce);
        assert!(v.verify("GET", "/files/x", b"", &ts.to_string(), nonce, &sig).is_ok());
    }

    #[test]
    fn body_hash_covers_payload() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let nonce = "n1";
        // Sign for body "hello"
        let sig = sign_for_test(&key(), "PUT", "/files/x", b"hello", ts, nonce);
        // Verify with a TAMPERED body.
        assert_eq!(
            v.verify("PUT", "/files/x", b"world", &ts.to_string(), nonce, &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn empty_body_uses_constant_hash() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let sig = sign_for_test(&key(), "DELETE", "/files/y", b"", ts, "n2");
        assert!(v.verify("DELETE", "/files/y", b"", &ts.to_string(), "n2", &sig).is_ok());
    }

    #[test]
    fn rejects_skew_too_old() {
        let v = Verifier::new(key());
        let ts = ts_now() - SKEW_S - 1;
        let sig = sign_for_test(&key(), "GET", "/x", b"", ts, "n3");
        assert_eq!(
            v.verify("GET", "/x", b"", &ts.to_string(), "n3", &sig),
            Err(AuthFail::SkewTooLarge)
        );
    }

    #[test]
    fn rejects_skew_too_new() {
        let v = Verifier::new(key());
        let ts = ts_now() + SKEW_S + 5;
        let sig = sign_for_test(&key(), "GET", "/x", b"", ts, "n4");
        assert_eq!(
            v.verify("GET", "/x", b"", &ts.to_string(), "n4", &sig),
            Err(AuthFail::SkewTooLarge)
        );
    }

    #[test]
    fn rejects_replayed_nonce() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let sig = sign_for_test(&key(), "POST", "/exec", b"{}", ts, "n5");
        assert!(v.verify("POST", "/exec", b"{}", &ts.to_string(), "n5", &sig).is_ok());
        // Same nonce, same request → replay rejected.
        assert_eq!(
            v.verify("POST", "/exec", b"{}", &ts.to_string(), "n5", &sig),
            Err(AuthFail::ReplayedNonce)
        );
    }

    #[test]
    fn rejects_path_tampering() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let sig = sign_for_test(&key(), "GET", "/files/safe", b"", ts, "n6");
        assert_eq!(
            v.verify("GET", "/files/secret", b"", &ts.to_string(), "n6", &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn rejects_method_tampering() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let sig = sign_for_test(&key(), "GET", "/x", b"", ts, "n7");
        assert_eq!(
            v.verify("DELETE", "/x", b"", &ts.to_string(), "n7", &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn rejects_bad_timestamp_format() {
        let v = Verifier::new(key());
        let r = v.verify("GET", "/x", b"", "not-a-number", "n8", "AAAA");
        assert_eq!(r, Err(AuthFail::BadTimestamp));
    }

    #[test]
    fn rejects_empty_nonce() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "", "AAAA");
        assert_eq!(r, Err(AuthFail::BadNonce));
    }

    #[test]
    fn rejects_oversize_nonce() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let huge = "a".repeat(MAX_NONCE_LEN + 1);
        let r = v.verify("GET", "/x", b"", &ts.to_string(), &huge, "AAAA");
        assert_eq!(r, Err(AuthFail::BadNonce));
    }

    #[test]
    fn rejects_nonce_with_bad_chars() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "abc def", "AAAA");
        assert_eq!(r, Err(AuthFail::BadNonce));
    }

    #[test]
    fn rejects_bad_base64_signature() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "n9", "%not base64!");
        assert_eq!(r, Err(AuthFail::BadSignatureEncoding));
    }

    #[test]
    fn rejects_wrong_key() {
        let v = Verifier::new(key());
        let ts = ts_now();
        let other_key = Zeroizing::new(b"OOOOOOOOOOOOOOOOOOOOOOOOOOOOOOOO".to_vec());
        let sig = sign_for_test(&other_key, "GET", "/x", b"", ts, "n10");
        assert_eq!(
            v.verify("GET", "/x", b"", &ts.to_string(), "n10", &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn bad_signature_does_not_pollute_nonce_cache() {
        let v = Verifier::new(key());
        let ts = ts_now();
        // Wrong signature → BadSignature, nonce NOT recorded.
        let r1 = v.verify("GET", "/x", b"", &ts.to_string(), "n11", "AAAAAAAAAAAA");
        assert_eq!(r1, Err(AuthFail::BadSignature));
        // Now a legitimate sign with the same nonce should still work.
        let sig = sign_for_test(&key(), "GET", "/x", b"", ts, "n11");
        assert!(v.verify("GET", "/x", b"", &ts.to_string(), "n11", &sig).is_ok());
    }

    #[test]
    fn debug_does_not_leak_key() {
        let v = Verifier::new(key());
        let s = format!("{:?}", v);
        assert!(!s.contains("abcdefgh"), "Debug must redact: {s}");
    }

    #[test]
    fn auth_fail_strings_stable() {
        // Wire-stable identifiers; renaming any breaks audit pipelines.
        assert_eq!(AuthFail::BadTimestamp.as_str(), "bad-timestamp");
        assert_eq!(AuthFail::SkewTooLarge.as_str(), "skew-too-large");
        assert_eq!(AuthFail::BadNonce.as_str(), "bad-nonce");
        assert_eq!(AuthFail::ReplayedNonce.as_str(), "replayed-nonce");
        assert_eq!(AuthFail::BadSignatureEncoding.as_str(), "bad-signature-encoding");
        assert_eq!(AuthFail::BadSignature.as_str(), "bad-signature");
    }
}
