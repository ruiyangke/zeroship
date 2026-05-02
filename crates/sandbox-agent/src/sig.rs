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

/// Domain-separator tag for canonical v1.1. Prepended to every v1.1
/// canonical-string so a captured v1 signature (no tag) cannot be
/// replayed as a v1.1 request and vice-versa. ASCII; no embedded
/// newlines (the `\n` separator inside the canonical is the field
/// delimiter, not part of this tag).
const V1_1_DOMAIN_TAG: &str = "ED25519-V1.1";

/// Canonical-string version. Picked by the dispatcher (the agent uses
/// the path prefix `/proxy/` to choose v1.1; everything else stays on
/// v1). Wire-stable; new variants append, never reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalKind {
    /// Original wire-protocol-v1: `method\npath\nts\nnonce\nsha256_hex(body)`.
    /// Query strings are NOT covered (and rejected outright by the
    /// /exec, /files, /tree handlers). Used by every endpoint that
    /// pre-dates the preview proxy.
    V1,
    /// `auth.ed25519-v1.1` — adds a domain-separator tag and folds
    /// the URL query into the canonical so the proxy endpoints
    /// (`/proxy/{port}/{path*}?...`) can carry Vite cache-busters
    /// (`?t=…`) without breaking the signature. Used by `/proxy/...`.
    ///
    /// ```text
    /// canonical_v1.1 = "ED25519-V1.1\n"
    ///                + method + "\n"
    ///                + path  // raw bytes; if URL had `?`, the
    ///                + canonical_query  //   `?` is INCLUDED literally
    ///                + "\n" + ts + "\n" + nonce + "\n"
    ///                + sha256_hex(body)
    /// ```
    ///
    /// Empty-vs-absent query (round-6 I2 corner-case table):
    /// - URL bytes contain `?` → the literal `?` AND the bytes between
    ///   `?` and `#` (or end) are included. `/foo?` → `…/foo?\n…`.
    /// - URL bytes do NOT contain `?` → the canonical does NOT include
    ///   the literal `?` separator. `/foo` → `…/foo\n…`.
    /// Both signer and verifier MUST hash the same bytes per these
    /// rules; the helper [`v1_1_path_query`] enforces them.
    V1_1,
}

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
    /// The canonical-version selected by the dispatcher (e.g. v1.1
    /// for `/proxy/...`) doesn't match what the controller signed.
    /// Distinct from `BadSignature` so audit + alerting can spot a
    /// controller misconfigured to sign v1 against the proxy path
    /// (or vice-versa) without lighting up the same dashboard tile
    /// as an actual signature-tamper attempt.
    WrongCanonicalVersion,
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
            AuthFail::WrongCanonicalVersion => "wrong-canonical-version",
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

    /// Verify a v1-canonical request. Equivalent to
    /// [`Verifier::verify_kind`] with [`CanonicalKind::V1`] and
    /// `path_query` set to the bare path (no query). Kept as the
    /// short-form for the existing (non-proxy) handlers.
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
        self.verify_kind(
            CanonicalKind::V1,
            method,
            path,
            body,
            ts_hdr,
            nonce_hdr,
            sig_hdr,
        )
    }

    /// Verify a request under a specific [`CanonicalKind`].
    ///
    /// For [`CanonicalKind::V1`] the `path_query` argument MUST be
    /// the bare path with no query (callers pass `req.path()` and
    /// gate on "no query string" themselves; the verifier doesn't
    /// re-check that).
    ///
    /// For [`CanonicalKind::V1_1`] the `path_query` argument MUST be
    /// the v1.1 path-and-query string per [`v1_1_path_query`]
    /// (path bytes, then if the URL had a `?`, the literal `?`
    /// followed by everything up to a `#` or end-of-URL). The signer
    /// and verifier MUST agree byte-exact; the helper centralises
    /// the rule.
    pub fn verify_kind(
        &self,
        kind: CanonicalKind,
        method: &str,
        path_query: &str,
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
        let canonical = build_canonical(
            kind,
            method,
            path_query,
            ts,
            nonce_hdr,
            &body_hash_hex,
        );

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

/// Build the canonical-string for a given [`CanonicalKind`].
///
/// Centralises the format so signer and verifier are guaranteed
/// byte-identical. Anyone reading this should remember: every byte
/// that lands in the returned string MUST land in the controller's
/// signer too. Don't add headers, don't normalise whitespace, don't
/// uppercase — the canonical is the contract.
fn build_canonical(
    kind: CanonicalKind,
    method: &str,
    path_query: &str,
    ts: u64,
    nonce: &str,
    body_hash_hex: &str,
) -> String {
    match kind {
        CanonicalKind::V1 => {
            format!("{method}\n{path_query}\n{ts}\n{nonce}\n{body_hash_hex}")
        }
        CanonicalKind::V1_1 => {
            format!(
                "{V1_1_DOMAIN_TAG}\n{method}\n{path_query}\n{ts}\n{nonce}\n{body_hash_hex}"
            )
        }
    }
}

/// Compose the v1.1 path-and-query slot from a raw request URI.
///
/// **Rules (round-6 § II.1 I2):**
///
/// - If `raw_uri` contains `?`, the result is `path` followed by `?`
///   and everything between the first `?` and the first `#` (or end).
///   Empty params are preserved (`/foo?` → `/foo?`).
/// - If `raw_uri` does NOT contain `?`, the result is the bare path
///   bytes — NO trailing `?` is appended. (Same path that a v1
///   canonical would use, but bound under a different domain-separator
///   so the two canonicals can never collide.)
/// - The fragment (`#…`) is stripped — it never reaches a server in
///   HTTP/1.1 anyway.
/// - The path bytes are passed through raw — no decode, no reencode,
///   no `..` collapsing. The signer (controller) and verifier (agent)
///   MUST receive the same `raw_uri` bytes from ntex; round-6's
///   "dispatcher byte-equality" invariant lives at the call site.
///
/// Returns a borrow of `raw_uri` so the caller doesn't allocate when
/// the URI is the canonical bytes already.
pub fn v1_1_path_query(raw_uri: &str) -> &str {
    // Trim a fragment first if any.
    let no_frag = match raw_uri.find('#') {
        Some(i) => &raw_uri[..i],
        None => raw_uri,
    };
    no_frag
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

/// Sign a v1-canonical request, producing the `X-Sbx-Signature`
/// header value. Equivalent to [`sign_kind`] with
/// [`CanonicalKind::V1`].
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
    sign_kind(
        CanonicalKind::V1,
        signing_key,
        method,
        path,
        body,
        ts,
        nonce,
    )
}

/// Sign a request under a specific [`CanonicalKind`].
///
/// `path_query` carries the bare path for [`CanonicalKind::V1`] and
/// the v1.1 path-and-query string for [`CanonicalKind::V1_1`] —
/// see [`v1_1_path_query`] for the byte-exact format.
pub fn sign_kind(
    kind: CanonicalKind,
    signing_key: &ed25519_dalek::SigningKey,
    method: &str,
    path_query: &str,
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
    let canonical = build_canonical(
        kind,
        method,
        path_query,
        ts,
        nonce,
        &body_hash_hex,
    );
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

    // ─── canonical v1.1 ───────────────────────────────────────────

    #[test]
    fn v1_1_path_query_drops_fragment() {
        // Fragments never reach the server in HTTP/1.1; the helper
        // strips them so signer and verifier see the same bytes.
        assert_eq!(v1_1_path_query("/proxy/5173/foo"), "/proxy/5173/foo");
        assert_eq!(v1_1_path_query("/proxy/5173/foo#frag"), "/proxy/5173/foo");
        assert_eq!(
            v1_1_path_query("/proxy/5173/foo?a=1#frag"),
            "/proxy/5173/foo?a=1"
        );
    }

    /// Round-6 § II.1 I2 — the 6-row corner-case table. Every row
    /// pins the byte-exact canonical fragment for v1.1 path-query.
    #[test]
    fn v1_1_path_query_corner_cases() {
        // Row 1: no `?` → bare path, no trailing `?` appended.
        assert_eq!(v1_1_path_query("/proxy/5173/foo"), "/proxy/5173/foo");
        // Row 2: literal `?` with empty params → preserved as-is.
        assert_eq!(v1_1_path_query("/proxy/5173/foo?"), "/proxy/5173/foo?");
        // Row 3: single param.
        assert_eq!(v1_1_path_query("/proxy/5173/foo?a=1"), "/proxy/5173/foo?a=1");
        // Row 4: multiple params (NOT sorted — we pass through raw
        // bytes; the controller MUST emit the same order it signs).
        assert_eq!(
            v1_1_path_query("/proxy/5173/foo?a=1&b=2"),
            "/proxy/5173/foo?a=1&b=2"
        );
        // Row 5: fragment without query.
        assert_eq!(v1_1_path_query("/proxy/5173/foo#frag"), "/proxy/5173/foo");
        // Row 6: fragment after query.
        assert_eq!(
            v1_1_path_query("/proxy/5173/foo?a=1#frag"),
            "/proxy/5173/foo?a=1"
        );
    }

    #[test]
    fn v1_1_canonical_includes_domain_tag() {
        // A v1.1-signed request MUST start with the ED25519-V1.1 tag.
        // This is the property that closes cross-version replay (a v1
        // canonical has no leading tag and therefore can't pass v1.1
        // verification regardless of the underlying signature).
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "v1-1-tag";
        let path_query = "/proxy/5173/foo";
        let sig = sign_kind(CanonicalKind::V1_1, &sk, "GET", path_query, b"", ts, nonce);
        assert!(v
            .verify_kind(
                CanonicalKind::V1_1,
                "GET",
                path_query,
                b"",
                &ts.to_string(),
                nonce,
                &sig,
            )
            .is_ok());
    }

    #[test]
    fn v1_1_query_is_covered() {
        // Tampering with the query bytes after signing must invalidate.
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "v1-1-q";
        let signed = "/proxy/5173/foo?a=1";
        let sent = "/proxy/5173/foo?a=2";
        let sig = sign_kind(CanonicalKind::V1_1, &sk, "GET", signed, b"", ts, nonce);
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1_1,
                "GET",
                sent,
                b"",
                &ts.to_string(),
                nonce,
                &sig,
            ),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn v1_canonical_does_not_validate_under_v1_1() {
        // CRITICAL-4 negative: a v1 canonical (no domain-separator)
        // sent against a v1.1 verifier MUST fail with BadSignature.
        // The verifier does NOT translate; the dispatcher chooses one
        // kind and that's the kind we evaluate.
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "v1-as-v11";
        let path = "/proxy/5173/foo";
        let v1_sig = sign_kind(CanonicalKind::V1, &sk, "GET", path, b"", ts, nonce);
        // Verifier picks v1.1; v1's canonical (no leading tag) won't match.
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1_1,
                "GET",
                path,
                b"",
                &ts.to_string(),
                nonce,
                &v1_sig,
            ),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn v1_1_canonical_does_not_validate_under_v1() {
        // Symmetric: a v1.1 signature includes the domain tag, so v1's
        // tag-less canonical computation produces a different message
        // and verification fails.
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "v11-as-v1";
        let path = "/exec";
        let v11_sig = sign_kind(CanonicalKind::V1_1, &sk, "POST", path, b"x", ts, nonce);
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1,
                "POST",
                path,
                b"x",
                &ts.to_string(),
                nonce,
                &v11_sig,
            ),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn v1_1_empty_query_is_distinct_from_absent_query() {
        // Round-6 I2 invariant: `?` with empty params and no `?` are
        // signed differently. A controller that signed `/foo?` and a
        // verifier asked to validate `/foo` (or vice-versa) MUST 401.
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce_a = "empty-q-a";
        let nonce_b = "empty-q-b";
        // Sign for `/foo?` (literal `?`, empty query).
        let sig = sign_kind(CanonicalKind::V1_1, &sk, "GET", "/proxy/5173/foo?", b"", ts, nonce_a);
        // Verifying against `/foo` (no `?`) MUST fail.
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1_1,
                "GET",
                "/proxy/5173/foo",
                b"",
                &ts.to_string(),
                nonce_a,
                &sig,
            ),
            Err(AuthFail::BadSignature)
        );
        // And sign-for-`/foo` does not validate at `/foo?`.
        let sig2 = sign_kind(CanonicalKind::V1_1, &sk, "GET", "/proxy/5173/foo", b"", ts, nonce_b);
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1_1,
                "GET",
                "/proxy/5173/foo?",
                b"",
                &ts.to_string(),
                nonce_b,
                &sig2,
            ),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn v1_1_body_hash_covered() {
        // Bodies bind into the canonical the same way as v1.
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "v1-1-body";
        let sig = sign_kind(
            CanonicalKind::V1_1,
            &sk,
            "POST",
            "/proxy/3000/api",
            b"hello",
            ts,
            nonce,
        );
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1_1,
                "POST",
                "/proxy/3000/api",
                b"world",
                &ts.to_string(),
                nonce,
                &sig,
            ),
            Err(AuthFail::BadSignature)
        );
    }

    #[test]
    fn auth_fail_wrong_canonical_version_string_stable() {
        // Wire-stable identifier; renaming would break audit pipelines.
        assert_eq!(
            AuthFail::WrongCanonicalVersion.as_str(),
            "wrong-canonical-version"
        );
    }

    #[test]
    fn v1_1_path_traversal_signature_binds_percent_encoded_bytes() {
        // CRITICAL-4: the percent-encoded bytes MUST be in the canonical
        // verbatim. A captured signature for `/proxy/5173/%2e%2e%2fexec`
        // must NOT validate `/proxy/5173/../exec` and vice-versa — they
        // are different byte strings to the canonical, regardless of how
        // the upstream interprets them.
        let (sk, pk) = keypair();
        let v = Verifier::new(pk);
        let ts = ts_now();
        let nonce = "trav";
        let raw = "/proxy/5173/%2e%2e%2fexec";
        let decoded = "/proxy/5173/../exec";
        let sig = sign_kind(CanonicalKind::V1_1, &sk, "GET", raw, b"", ts, nonce);
        // The percent-encoded form validates.
        assert!(v
            .verify_kind(
                CanonicalKind::V1_1,
                "GET",
                raw,
                b"",
                &ts.to_string(),
                nonce,
                &sig
            )
            .is_ok());
        // The decoded form does not — different bytes, different canonical.
        let nonce2 = "trav2";
        let sig2 = sign_kind(CanonicalKind::V1_1, &sk, "GET", raw, b"", ts, nonce2);
        assert_eq!(
            v.verify_kind(
                CanonicalKind::V1_1,
                "GET",
                decoded,
                b"",
                &ts.to_string(),
                nonce2,
                &sig2,
            ),
            Err(AuthFail::BadSignature)
        );
    }
}
