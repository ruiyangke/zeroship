//! Per-request Ed25519 verification (`auth.ed25519-v1`).
//!
//! The agent's wire-protocol-v1 auth scheme. Defeats:
//!
//!   - **Replay** of a captured request beyond a 5-second window
//!   - **In-flight body / path / method tampering**
//!   - **Forgery from inside a sandbox VM** — the agent holds
//!     **only the public key**; the private (signing) key never
//!     enters any sandbox at any point in the lifecycle. Even
//!     full root inside the libkrun VM (or PID-1 memory dump)
//!     yields zero forgery capability against any sandbox in the
//!     fleet.
//!
//! ## Wire format
//!
//! Three request headers:
//!
//! ```text
//! X-Sbx-Timestamp: <unix_seconds>
//! X-Sbx-Nonce:     <ascii-alnum + - / _, ≤ 64 chars>
//! X-Sbx-Signature: <base64 64-byte Ed25519 signature>
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
//! Body is hashed (not inlined) so signers and verifiers can stream
//! large bodies without buffering the full content in the
//! to-be-signed message.
//!
//! ## Verification rules (in order — all must pass)
//!
//! 1. **Timestamp present + parseable** as decimal `u64`.
//! 2. **Clock skew** — `|now - ts| <= SKEW_S` (5 s).
//! 3. **Nonce well-formed** — non-empty, length-bounded charset.
//! 4. **Nonce not seen recently** — bounded LRU, `NONCE_TTL_S` (30 s).
//! 5. **Signature verifies** under the controller's public key (via
//!    `ed25519_dalek::VerifyingKey::verify_strict` — rejects S
//!    out-of-range and other non-canonical forms).
//!
//! Only after all five pass do we record the nonce as "seen", so a
//! garbage signature can't pollute the cache.
//!
//! ## Why Ed25519 (and not HMAC)
//!
//! With HMAC, agent and controller share the same secret. A fully
//! compromised agent (e.g. memory disclosed via a future bug) hands
//! the attacker forgery capability against itself. With Ed25519:
//!
//!   - Agent holds the **public** key only — non-secret material.
//!     Mounted from a `ConfigMap` (read-only fs), no `Secret`,
//!     no encryption-at-rest concerns.
//!   - Private key lives only on the controller. Forgery requires
//!     compromising the controller, not any individual sandbox.
//!   - The agent process **cannot generate a key the controller
//!     would trust** because the controller's pubkey is the trust
//!     anchor; arbitrary new keypairs are not in the trust set.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, VerifyingKey, SIGNATURE_LENGTH};
use lru::LruCache;
use sha2::{Digest, Sha256};

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
    /// `X-Sbx-Signature` missing, not valid base64, or wrong length.
    BadSignatureEncoding,
    /// Ed25519 verification failed — wrong key, tampered
    /// body/path/method, malformed signature scalar, etc.
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

/// Verifier holds the controller's Ed25519 public key and a bounded
/// replay cache.
///
/// Cloned cheaply via `Arc<Verifier>` in the application state.
/// Contains **no secret material**; safe to log, dump, or include
/// in core files (the public key is public by design).
pub struct Verifier {
    pubkey: VerifyingKey,
    nonces: Mutex<LruCache<String, u64>>,
}

impl Verifier {
    /// Construct from a public key. Caller is responsible for
    /// supplying the correct controller pubkey — typically loaded
    /// via [`crate::auth::load_pubkey_from_path`] from a read-only
    /// ConfigMap mount.
    pub fn new(pubkey: VerifyingKey) -> Self {
        let cap = std::num::NonZeroUsize::new(NONCE_CACHE_CAPACITY)
            .expect("NONCE_CACHE_CAPACITY > 0");
        Self {
            pubkey,
            nonces: Mutex::new(LruCache::new(cap)),
        }
    }

    /// The controller's public key. Exposed for `/version` debug
    /// output (key-id reporting); the value is non-secret.
    pub fn pubkey(&self) -> &VerifyingKey {
        &self.pubkey
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

        // 3. Decode the signature header. Ed25519 signatures are
        //    exactly 64 bytes — anything else is malformed.
        let sig_bytes = B64
            .decode(sig_hdr.as_bytes())
            .map_err(|_| AuthFail::BadSignatureEncoding)?;
        if sig_bytes.len() != SIGNATURE_LENGTH {
            return Err(AuthFail::BadSignatureEncoding);
        }
        let sig = Signature::from_slice(&sig_bytes)
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

        // 5. Verify under the controller's public key.
        //    `verify_strict` rejects malleable / non-canonical sigs
        //    (e.g. S not reduced mod l). This is what the docs
        //    recommend for fresh-design protocols like ours.
        if self.pubkey.verify_strict(canonical.as_bytes(), &sig).is_err() {
            return Err(AuthFail::BadSignature);
        }

        // 6. Nonce replay check. We do this LAST so that an
        //    attacker with a wrong key (signature mismatch) cannot
        //    pollute the cache by spamming valid-looking nonces.
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
        // Pubkey is non-secret; emit a short fingerprint so logs
        // can identify which trust anchor is in use.
        let fp = pubkey_fingerprint(&self.pubkey);
        f.debug_struct("Verifier")
            .field("pubkey_fp", &fp)
            .field("nonces", &"<lru>")
            .finish()
    }
}

/// Short hex fingerprint of a public key — first 8 bytes of
/// SHA-256(pubkey). Useful in logs / `/version` for operators to
/// confirm "the agent is verifying with the expected controller key".
pub fn pubkey_fingerprint(pk: &VerifyingKey) -> String {
    let digest = Sha256::digest(pk.as_bytes());
    hex::encode(&digest[..8])
}

/// Sign a request, producing the `X-Sbx-Signature` header value.
///
/// **Controller-side helper.** The agent never imports
/// `SigningKey` and never calls this function — it's compiled in
/// for unit tests of the verifier and for the controller / e2e
/// harness. The function is kept here so signer and verifier share
/// the canonical-string definition and can never drift.
///
/// Returns base64(64-byte Ed25519 signature) over the canonical
/// string. Caller is responsible for emitting `X-Sbx-Timestamp`
/// and `X-Sbx-Nonce` alongside the returned signature.
pub fn sign(
    signing_key: &ed25519_dalek::SigningKey,
    method: &str,
    path: &str,
    body: &[u8],
    ts: u64,
    nonce: &str,
) -> String {
    use ed25519_dalek::Signer;
    let body_hash_hex: String = if body.is_empty() {
        EMPTY_BODY_SHA256_HEX.to_string()
    } else {
        hex::encode(Sha256::digest(body))
    };
    let canonical = format!("{method}\n{path}\n{ts}\n{nonce}\n{body_hash_hex}");
    let sig: Signature = signing_key.sign(canonical.as_bytes());
    B64.encode(sig.to_bytes())
}

/// Drop entries older than `NONCE_TTL_S`. Called inside the lock,
/// best-effort: walks until it finds a still-valid entry from the
/// LRU's least-recent end.
fn evict_expired(cache: &mut LruCache<String, u64>, now: u64) {
    while let Some((_, ts)) = cache.peek_lru() {
        if now.saturating_sub(*ts) > NONCE_TTL_S {
            cache.pop_lru();
        } else {
            break;
        }
    }
}

/// Wall-clock seconds since UNIX_EPOCH.
///
/// Crash-loud on clock-before-epoch instead of silently returning 0:
/// inside the agent, `ts=0` would make every reply's nonce-LRU key
/// behave bizarrely + the skew check would 401 every controller
/// request forever. A panicking agent surfaces a broken VM clock
/// (the controller's `wait_for_agent_livez` timeout), which is the
/// right place to act on it. Mirror of `controller::unix_now` in
/// both backends.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX_EPOCH")
        .as_secs()
}

#[inline]
fn abs_diff(a: u64, b: u64) -> u64 {
    if a >= b { a - b } else { b - a }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn keypair() -> (SigningKey, VerifyingKey) {
        // Deterministic test key — never used in production.
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let pk = sk.verifying_key();
        (sk, pk)
    }

    fn ts_now() -> u64 { unix_now() }

    #[test]
    fn valid_request_passes() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "abc123";
        let sig = sign(&sk, "GET", "/files/x", b"", ts, nonce);
        assert!(v.verify("GET", "/files/x", b"", &ts.to_string(), nonce, &sig).is_ok());
    }

    #[test]
    fn body_hash_covers_payload() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "n1";
        let sig = sign(&sk, "PUT", "/files/x", b"hello", ts, nonce);
        assert_eq!(
            v.verify("PUT", "/files/x", b"world", &ts.to_string(), nonce, &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn empty_body_uses_constant_hash() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let sig = sign(&sk, "DELETE", "/files/y", b"", ts, "n2");
        assert!(v.verify("DELETE", "/files/y", b"", &ts.to_string(), "n2", &sig).is_ok());
    }

    #[test]
    fn rejects_skew_too_old() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now() - SKEW_S - 1;
        let sig = sign(&sk, "GET", "/x", b"", ts, "n3");
        assert_eq!(
            v.verify("GET", "/x", b"", &ts.to_string(), "n3", &sig),
            Err(AuthFail::SkewTooLarge)
        );
    }

    #[test]
    fn rejects_skew_too_new() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now() + SKEW_S + 5;
        let sig = sign(&sk, "GET", "/x", b"", ts, "n4");
        assert_eq!(
            v.verify("GET", "/x", b"", &ts.to_string(), "n4", &sig),
            Err(AuthFail::SkewTooLarge)
        );
    }

    #[test]
    fn rejects_replayed_nonce() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let sig = sign(&sk, "POST", "/exec", b"{}", ts, "n5");
        assert!(v.verify("POST", "/exec", b"{}", &ts.to_string(), "n5", &sig).is_ok());
        assert_eq!(
            v.verify("POST", "/exec", b"{}", &ts.to_string(), "n5", &sig),
            Err(AuthFail::ReplayedNonce)
        );
    }

    #[test]
    fn rejects_path_tampering() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let sig = sign(&sk, "GET", "/files/safe", b"", ts, "n6");
        assert_eq!(
            v.verify("GET", "/files/secret", b"", &ts.to_string(), "n6", &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn rejects_method_tampering() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let sig = sign(&sk, "GET", "/x", b"", ts, "n7");
        assert_eq!(
            v.verify("DELETE", "/x", b"", &ts.to_string(), "n7", &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn rejects_bad_timestamp_format() {
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let r = v.verify("GET", "/x", b"", "not-a-number", "n8", "AAAA");
        assert_eq!(r, Err(AuthFail::BadTimestamp));
    }

    #[test]
    fn rejects_empty_nonce() {
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "", "AAAA");
        assert_eq!(r, Err(AuthFail::BadNonce));
    }

    #[test]
    fn rejects_oversize_nonce() {
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let huge = "a".repeat(MAX_NONCE_LEN + 1);
        let r = v.verify("GET", "/x", b"", &ts.to_string(), &huge, "AAAA");
        assert_eq!(r, Err(AuthFail::BadNonce));
    }

    #[test]
    fn rejects_nonce_with_bad_chars() {
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "abc def", "AAAA");
        assert_eq!(r, Err(AuthFail::BadNonce));
    }

    #[test]
    fn rejects_bad_base64_signature() {
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "n9", "%not base64!");
        assert_eq!(r, Err(AuthFail::BadSignatureEncoding));
    }

    #[test]
    fn rejects_wrong_length_signature() {
        // base64("AAAA") = 3 bytes, not 64 — must fall through to
        // BadSignatureEncoding rather than BadSignature.
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let r = v.verify("GET", "/x", b"", &ts.to_string(), "n9b", "AAAA");
        assert_eq!(r, Err(AuthFail::BadSignatureEncoding));
    }

    #[test]
    fn rejects_wrong_key() {
        // Sign with a different key, verify with our pubkey →
        // BadSignature (not Encoding — sig is well-formed).
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let other_sk = SigningKey::from_bytes(&[7u8; 32]);
        let sig = sign(&other_sk, "GET", "/x", b"", ts, "n10");
        assert_eq!(
            v.verify("GET", "/x", b"", &ts.to_string(), "n10", &sig),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn bad_signature_does_not_pollute_nonce_cache() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        // Wrong-length sig → BadSignatureEncoding, nonce NOT recorded.
        let r1 = v.verify("GET", "/x", b"", &ts.to_string(), "n11", "AAAAAAAAAAAA");
        assert_eq!(r1, Err(AuthFail::BadSignatureEncoding));
        // Now a legitimate sign with the same nonce should still work.
        let sig = sign(&sk, "GET", "/x", b"", ts, "n11");
        assert!(v.verify("GET", "/x", b"", &ts.to_string(), "n11", &sig).is_ok());
    }

    /// Tampered-but-well-formed sig (correct length, bogus contents):
    /// also must NOT pollute the nonce cache.
    #[test]
    fn invalid_well_formed_sig_does_not_pollute_cache() {
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        // 64 bytes of zeros, base64-encoded.
        let bogus = B64.encode([0u8; 64]);
        let r1 = v.verify("GET", "/x", b"", &ts.to_string(), "n11b", &bogus);
        assert_eq!(r1, Err(AuthFail::BadSignature));
        let sig = sign(&sk, "GET", "/x", b"", ts, "n11b");
        assert!(v.verify("GET", "/x", b"", &ts.to_string(), "n11b", &sig).is_ok());
    }

    #[test]
    fn debug_does_not_leak_pubkey_bytes() {
        // Pubkey isn't secret, but Debug output should still be
        // short and operator-friendly (fingerprint, not full bytes).
        let (_, pk) = keypair();
        let v = Verifier::new(pk);
        let s = format!("{v:?}");
        // The fingerprint is 16 hex chars; the full pubkey is 64
        // hex chars. Make sure we emit the short form.
        let full_hex = hex::encode(pk.as_bytes());
        assert!(!s.contains(&full_hex), "Debug must use fingerprint: {s}");
        assert!(s.contains("pubkey_fp"), "Debug must show fingerprint: {s}");
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

    #[test]
    fn pubkey_fingerprint_is_stable_and_short() {
        let (_, pk) = keypair();
        let fp = pubkey_fingerprint(&pk);
        assert_eq!(fp.len(), 16, "fingerprint should be 8 bytes hex");
        // Stable for the deterministic test key.
        assert_eq!(fp, pubkey_fingerprint(&keypair().1));
    }
}
