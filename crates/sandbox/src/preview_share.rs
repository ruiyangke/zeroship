//! Preview share-token format + mint/list/revoke handlers
//! (preview-URL § II.4 + § III).
//!
//! ## Token wire format
//!
//! ```text
//! token = <payload_b> "~" <sig_b>
//!   payload_b = base64url_no_pad(payload_json_bytes)
//!   sig_b     = base64url_no_pad(HMAC-SHA-256(secret_v<sv>, payload_b))
//! ```
//!
//! `payload_json` is a flat JSON object with these claims:
//!
//! - `aud: "preview"` — literal; the validator rejects every other
//!   value (audience separation against future `aud: "shared-logs"`).
//! - `sbx: <typed-id>` — the bound sandbox; refused by the validator
//!   if it doesn't match the path-bound id.
//! - `port: <u16>`
//! - `iss: <creator-typed-id>` — optional issuer (audit-only at v7;
//!   gates abuse on the next bump).
//! - `iat: <unix-seconds>` — issue time. Validate-side: must be
//!   `<= now + 5` (small skew).
//! - `exp: <unix-seconds>` — expiry. Validate-side ceiling
//!   `exp <= iat + 1 week`.
//! - `sv: <u32>` — secret-version. The validator looks up
//!   `secret_v<sv>` in the registry's per-sandbox ring.
//! - `tid: <16-byte-base64url>` — token-id; surfaced in the audit
//!   table as `shr_<tid>` (in this module's mint helper).
//! - `scope: "ro" | "rw"` — `ro` allows GET/HEAD/OPTIONS; `rw` allows
//!   all methods.
//!
//! Notes for reviewers:
//!
//! - **Wire-stable.** `~` separator (round-6 LOW-3, not `.` — JWT
//!   confusion); base64url no-pad alphabet `A-Za-z0-9_-`.
//! - **HMAC-input invariant** (round-6 H5). The validator hashes the
//!   on-wire `payload_b` ASCII bytes; canonicalizing the JSON before
//!   HMAC verification breaks every issued token. Pinned via test.
//! - **Hardening caps.** Raw token ≤ 1 KiB, decoded payload ≤ 4 KiB,
//!   `serde(deny_unknown_fields)`. Anything malformed → 401 (no
//!   information leak).
//! - **Constant-time HMAC compare** via `subtle::ConstantTimeEq`.
//! - **No JSON canonicalization on the validator side.** The mint
//!   helper here uses `serde_json::to_vec` — whatever order serde
//!   emits is what's signed; both sides hash exactly the on-wire
//!   bytes.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;

/// Hard cap on the raw `?t=...` query value AND any cookie body that
/// carries a token. Rejected before base64 decode (§ II.4).
pub const TOKEN_RAW_MAX: usize = 1024;

/// Hard cap on the decoded JSON payload (§ II.4). 4 KiB is generous
/// — the canonical claim set serializes to ~280 bytes.
pub const PAYLOAD_BYTES_MAX: usize = 4096;

/// Validate-side ceiling on `exp - iat`. Defends against a controller
/// bug that would mint a 10-year token by checking BOTH at mint time
/// (the API layer) AND at validate time. (§ II.4 round-6 LOW-4)
pub const TTL_CEILING_SECS: u64 = 7 * 24 * 60 * 60;

/// Mint-side ceiling on `expires_in_secs`. Matches `TTL_CEILING_SECS`
/// at the seconds granularity. The mint API also enforces a 60 s
/// minimum (`expires_in_secs >= 60`) per § III.
pub const MINT_TTL_MAX_SECS: u64 = TTL_CEILING_SECS;
pub const MINT_TTL_MIN_SECS: u64 = 60;

type HmacSha256 = Hmac<Sha256>;

/// Token claims. Field names match the JSON wire format byte-for-byte.
///
/// **`deny_unknown_fields`** (round-2 hardening): forces forward-incompat
/// tokens with novel fields to fail closed at the validator. A future
/// bump that wants to add a field MUST coordinate with every running
/// validator (or use `Option<…>` with a default for graceful rollout).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TokenClaims {
    pub aud: String,
    pub sbx: String,
    pub port: u16,
    /// Audit-only at v7 (round-6 H7). Optional so legacy mints that
    /// didn't set it still validate; recorded in the audit log when
    /// present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    pub iat: u64,
    pub exp: u64,
    pub sv: u32,
    pub tid: String,
    pub scope: String,
}

/// Reasons the validator can refuse a token. Distinct variants for
/// audit logging; the wire-side error code is `"unauthorized"` /
/// `"expired"` / `"revoked"` (a small public surface; see [`Self::wire_code`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// Raw token exceeded `TOKEN_RAW_MAX` before decode.
    RawTooLong,
    /// Separator missing or doubled (round-6 LOW-3).
    BadSeparator,
    /// `payload_b` or `sig_b` failed base64url decode.
    Base64,
    /// Decoded JSON exceeded `PAYLOAD_BYTES_MAX`.
    PayloadTooLarge,
    /// Sig not 32 bytes after decode (HMAC-SHA-256 output length).
    BadSigLen,
    /// JSON parse failed (incl. `deny_unknown_fields`).
    Json,
    /// `aud != "preview"`.
    WrongAudience,
    /// `exp - iat > TTL_CEILING_SECS` (defense-in-depth at validate-side).
    TtlTooLong,
    /// `iat > now + 5`.
    FutureIat,
    /// `iat >= exp`.
    InvalidWindow,
    /// `exp <= now`.
    Expired,
    /// `sbx` doesn't match the path-bound sandbox-id.
    SandboxMismatch,
    /// `port` doesn't match the path-bound port.
    PortMismatch,
    /// `scope` doesn't allow this method.
    ScopeForbidden,
    /// `sv` not in the registry's ring (current OR previous-in-grace).
    Revoked,
    /// HMAC signature didn't verify against the looked-up secret.
    BadSig,
    /// Scope string not one of the supported values (`ro`, `rw`).
    UnknownScope,
}

impl TokenError {
    /// Map to the public error code surfaced on the wire. Most
    /// failures coalesce into `unauthorized` to avoid an oracle by
    /// failure-reason; `expired` is surfaced separately so the AI
    /// builder UI can render "your share link has expired" specifically
    /// (§ II.4 + § XI.4 error-code contract).
    pub fn wire_code(&self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::ScopeForbidden => "scope_forbidden",
            _ => "unauthorized",
        }
    }
}

/// Mint a token. The HMAC input is the on-wire `payload_b` ASCII
/// bytes; do NOT re-encode the JSON before signing on either side.
///
/// `secret` is the per-sandbox `secret_v<sv_current>` lifted out of
/// the registry's ring.
pub fn mint(claims: &TokenClaims, secret: &[u8; 32]) -> String {
    let payload_json = serde_json::to_vec(claims).expect("claims serialize");
    let payload_b = URL_SAFE_NO_PAD.encode(&payload_json);
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC keylen");
    mac.update(payload_b.as_bytes());
    let sig_bytes = mac.finalize().into_bytes();
    let sig_b = URL_SAFE_NO_PAD.encode(sig_bytes);
    format!("{payload_b}~{sig_b}")
}

/// Method-scope policy. `"ro"` allows GET/HEAD/OPTIONS only; `"rw"`
/// allows every method. Returns `false` for any unrecognized scope
/// — defense-in-depth against a future "scope = ro+/path" string
/// being read as `rw` by accident.
pub fn scope_allows_method(scope: &str, method: &str) -> bool {
    let m = method.eq_ignore_ascii_case("GET")
        || method.eq_ignore_ascii_case("HEAD")
        || method.eq_ignore_ascii_case("OPTIONS");
    match scope {
        "ro" => m,
        "rw" => true,
        _ => false,
    }
}

/// Mint-time scope validator. Refuses unknown scopes at the API layer
/// (`POST .../share`); the validator already refuses them again as
/// belt-and-suspenders.
pub fn is_known_scope(scope: &str) -> bool {
    matches!(scope, "ro" | "rw")
}

/// Lookup interface for the per-sandbox secret ring. The validator
/// is decoupled from the registry to keep the function pure and
/// unit-testable. Returns `Some(secret)` for current OR previous-in-
/// grace versions; `None` is treated as "revoked" by the caller.
pub trait SecretLookup {
    fn secret_for_version(&self, sv: u32) -> Option<[u8; 32]>;
}

/// Convenience adapter for the registry's per-sandbox lookup.
#[derive(Debug)]
pub struct RegistrySecretLookup<'a> {
    pub registry: &'a crate::registry::SandboxRegistry,
    pub sandbox_id: uuid::Uuid,
}

impl SecretLookup for RegistrySecretLookup<'_> {
    fn secret_for_version(&self, sv: u32) -> Option<[u8; 32]> {
        self.registry.secret_for_version(self.sandbox_id, sv)
    }
}

/// Validate a raw token against a sandbox + port + method. Returns
/// the decoded claims on success.
///
/// Caller MUST have already enforced any `__Host-` cookie / Sec-Fetch
/// gating; this function only validates the token's wire form +
/// claims + HMAC.
///
/// Two callers, two scope-policies:
///
/// - **Cookie-conversion handler** (`__zsbx_share?t=…`) wants the
///   *helpful* `403 scope_forbidden` so the AI-builder UI can render
///   "this is a read-only link, the action you tried needs a `rw`
///   token". It calls this top-level [`validate_token`] which DOES
///   enforce scope.
/// - **Dispatch path** (`try_share_cookie`) wants the *uniform 404*
///   that port-deny + not-owner already produce — adding a 403 here
///   would create an oracle for "this is a real, owned, ro-scoped
///   token". That path calls [`validate_token_skip_scope`] and lets
///   `authorize_with_method` re-check scope as a uniform-404 path.
pub fn validate_token(
    raw_token: &str,
    sandbox_id_str: &str,
    port: u16,
    method: &str,
    now_unix: u64,
    secrets: &dyn SecretLookup,
) -> Result<TokenClaims, TokenError> {
    // 0. Length precondition. Done BEFORE base64 / JSON to bound the
    //    work an unauthenticated attacker can drive.
    if raw_token.len() > TOKEN_RAW_MAX {
        return Err(TokenError::RawTooLong);
    }

    // 1. Split on `~`. Exactly one `~` allowed (round-6 LOW-3).
    let (payload_b, sig_b) = match raw_token.split_once('~') {
        Some(p) => p,
        None => return Err(TokenError::BadSeparator),
    };
    if sig_b.contains('~') {
        return Err(TokenError::BadSeparator);
    }
    if payload_b.is_empty() || sig_b.is_empty() {
        return Err(TokenError::BadSeparator);
    }

    // 2. Decode parts. base64url, no padding.
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload_b.as_bytes())
        .map_err(|_| TokenError::Base64)?;
    if payload_bytes.len() > PAYLOAD_BYTES_MAX {
        return Err(TokenError::PayloadTooLarge);
    }
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(sig_b.as_bytes())
        .map_err(|_| TokenError::Base64)?;
    if sig_bytes.len() != 32 {
        return Err(TokenError::BadSigLen);
    }

    // 3. Parse claims with deny_unknown_fields.
    let claims: TokenClaims =
        serde_json::from_slice(&payload_bytes).map_err(|_| TokenError::Json)?;
    if claims.aud != "preview" {
        return Err(TokenError::WrongAudience);
    }
    if !is_known_scope(&claims.scope) {
        return Err(TokenError::UnknownScope);
    }

    // 4. TTL ceiling. The mint API enforces this; the validator
    //    re-checks for defense-in-depth.
    if claims.exp.saturating_sub(claims.iat) > TTL_CEILING_SECS {
        return Err(TokenError::TtlTooLong);
    }

    // 5. Pick secret by `sv`. `None` from the registry means the
    //    secret is older than current AND outside grace — i.e.
    //    revoked.
    let secret = secrets
        .secret_for_version(claims.sv)
        .ok_or(TokenError::Revoked)?;

    // 6. HMAC over the on-wire `payload_b` ASCII bytes (round-6 H5).
    //    Do NOT re-serialize the parsed claims and hash that.
    let mut mac = HmacSha256::new_from_slice(&secret).expect("HMAC keylen");
    mac.update(payload_b.as_bytes());
    let expected = mac.finalize().into_bytes();
    // Constant-time compare. `expected` is 32 bytes; `sig_bytes` was
    // length-checked above.
    if !bool::from(sig_bytes.ct_eq(expected.as_slice())) {
        return Err(TokenError::BadSig);
    }

    // 7. Bind the token to the request. iat skew tolerance is 5 s.
    if claims.iat > now_unix.saturating_add(5) {
        return Err(TokenError::FutureIat);
    }
    if claims.iat >= claims.exp {
        return Err(TokenError::InvalidWindow);
    }
    if claims.exp <= now_unix {
        return Err(TokenError::Expired);
    }
    if claims.sbx != sandbox_id_str {
        return Err(TokenError::SandboxMismatch);
    }
    if claims.port != port {
        return Err(TokenError::PortMismatch);
    }
    if !scope_allows_method(&claims.scope, method) {
        return Err(TokenError::ScopeForbidden);
    }

    Ok(claims)
}

/// Like [`validate_token`] but does NOT enforce the scope-vs-method
/// check. Used by the dispatch path so that scope-violation collapses
/// to the same uniform 404 as port-deny / not-owner / sandbox-not-
/// found, rather than surfacing a `403 scope_forbidden` oracle on the
/// public edge.
///
/// The caller is responsible for re-running `scope_allows_method` —
/// in practice this is `preview::authorize_with_method` which already
/// does so as a belt-and-suspenders check, then returns `false` →
/// `uniform_404`.
pub fn validate_token_skip_scope(
    raw_token: &str,
    sandbox_id_str: &str,
    port: u16,
    now_unix: u64,
    secrets: &dyn SecretLookup,
) -> Result<TokenClaims, TokenError> {
    // Reuse `validate_token` with method=GET (always passes scope for
    // any known scope), then trust the caller's
    // `scope_allows_method(claims.scope, real_method)` re-check. This
    // keeps the validation logic in one place — there's no second
    // copy to drift.
    validate_token(raw_token, sandbox_id_str, port, "GET", now_unix, secrets)
}

/// Generate a token-id (`tid` claim). 16 random bytes, base64url
/// no-pad — 22 chars. Used as the audit-table key (`shr_<tid>` is
/// the human-facing form).
pub fn fresh_token_id() -> String {
    use std::fs::File;
    use std::io::Read;
    let mut buf = [0u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = ((nanos >> (i * 4)) ^ (std::process::id() as u128) << (i % 8)) as u8;
        }
    }
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedSecrets {
        sv: u32,
        bytes: [u8; 32],
    }

    impl SecretLookup for FixedSecrets {
        fn secret_for_version(&self, sv: u32) -> Option<[u8; 32]> {
            if sv == self.sv {
                Some(self.bytes)
            } else {
                None
            }
        }
    }

    fn fresh_claims(sandbox_id: &str, port: u16, scope: &str) -> TokenClaims {
        TokenClaims {
            aud: "preview".into(),
            sbx: sandbox_id.into(),
            port,
            iss: Some("usr_test".into()),
            iat: 1_000_000,
            exp: 1_000_000 + 3600,
            sv: 1,
            tid: "abc".into(),
            scope: scope.into(),
        }
    }

    #[test]
    fn mint_validate_round_trip() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_test", 5173, "ro");
        let tok = mint(&claims, &secret);
        let got = validate_token(&tok, "sbx_test", 5173, "GET", 1_000_500, &lookup)
            .expect("validate");
        assert_eq!(got, claims);
    }

    #[test]
    fn separator_rejects_dot_and_double_tilde() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_test", 5173, "ro");
        let tok = mint(&claims, &secret);
        let dotted = tok.replacen('~', ".", 1);
        assert_eq!(
            validate_token(&dotted, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::BadSeparator
        );
        let extra = format!("{tok}~extra");
        assert_eq!(
            validate_token(&extra, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::BadSeparator
        );
    }

    #[test]
    fn raw_too_long_rejected_before_decode() {
        let lookup = FixedSecrets { sv: 1, bytes: [0; 32] };
        let raw = "x".repeat(TOKEN_RAW_MAX + 1);
        assert_eq!(
            validate_token(&raw, "sbx_test", 5173, "GET", 0, &lookup).unwrap_err(),
            TokenError::RawTooLong
        );
    }

    #[test]
    fn unknown_field_rejected_via_deny_unknown_fields() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        // Hand-craft a JSON with an extra "evil" field, encode + sign
        // it correctly. `serde_json::from_slice` MUST refuse.
        let raw = serde_json::json!({
            "aud": "preview",
            "sbx": "sbx_test",
            "port": 5173,
            "iat": 1_000_000,
            "exp": 1_000_000 + 3600,
            "sv": 1,
            "tid": "abc",
            "scope": "ro",
            "evil": "x",
        });
        let payload_json = serde_json::to_vec(&raw).unwrap();
        let payload_b = URL_SAFE_NO_PAD.encode(&payload_json);
        let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
        mac.update(payload_b.as_bytes());
        let sig_b = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
        let tok = format!("{payload_b}~{sig_b}");
        assert_eq!(
            validate_token(&tok, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::Json,
            "deny_unknown_fields must reject `evil`"
        );
    }

    #[test]
    fn wrong_audience_rejected() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let mut claims = fresh_claims("sbx_test", 5173, "ro");
        claims.aud = "shared-logs".into();
        let tok = mint(&claims, &secret);
        assert_eq!(
            validate_token(&tok, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::WrongAudience
        );
    }

    #[test]
    fn tampered_signature_constant_time_rejected() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_test", 5173, "ro");
        let tok = mint(&claims, &secret);
        // Flip last byte of sig (safe rebuild; base64url chars all
        // ASCII so byte-replace is char-safe).
        let mut bytes = tok.clone().into_bytes();
        let last_idx = bytes.len() - 1;
        bytes[last_idx] = if bytes[last_idx] == b'A' { b'B' } else { b'A' };
        let bad_last = String::from_utf8(bytes).unwrap();
        assert_eq!(
            validate_token(&bad_last, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::BadSig
        );
        // Flip first sig byte (after `~`).
        let tilde = tok.find('~').unwrap();
        let mut bytes = tok.clone().into_bytes();
        bytes[tilde + 1] = if bytes[tilde + 1] == b'A' { b'B' } else { b'A' };
        let bad_first = String::from_utf8(bytes).unwrap();
        assert_eq!(
            validate_token(&bad_first, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::BadSig
        );
    }

    #[test]
    fn expired_token_rejected_with_expired_code() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_test", 5173, "ro");
        let tok = mint(&claims, &secret);
        let err = validate_token(&tok, "sbx_test", 5173, "GET", claims.exp + 1, &lookup)
            .unwrap_err();
        assert_eq!(err, TokenError::Expired);
        assert_eq!(err.wire_code(), "expired");
    }

    #[test]
    fn future_iat_rejected() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let mut claims = fresh_claims("sbx_test", 5173, "ro");
        claims.iat = 2_000_000;
        claims.exp = 2_001_000;
        let tok = mint(&claims, &secret);
        // now < iat - 5
        assert_eq!(
            validate_token(&tok, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::FutureIat
        );
    }

    #[test]
    fn ttl_ceiling_validate_side() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let mut claims = fresh_claims("sbx_test", 5173, "ro");
        claims.iat = 1_000_000;
        claims.exp = claims.iat + TTL_CEILING_SECS + 1;
        let tok = mint(&claims, &secret);
        assert_eq!(
            validate_token(&tok, "sbx_test", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::TtlTooLong
        );
    }

    #[test]
    fn cross_sandbox_rejected() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_a", 5173, "ro");
        let tok = mint(&claims, &secret);
        assert_eq!(
            validate_token(&tok, "sbx_b", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::SandboxMismatch
        );
    }

    #[test]
    fn cross_port_rejected() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_a", 5173, "ro");
        let tok = mint(&claims, &secret);
        assert_eq!(
            validate_token(&tok, "sbx_a", 3000, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::PortMismatch
        );
    }

    #[test]
    fn ro_scope_blocks_post() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_a", 5173, "ro");
        let tok = mint(&claims, &secret);
        assert_eq!(
            validate_token(&tok, "sbx_a", 5173, "POST", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::ScopeForbidden
        );
        // GET / HEAD / OPTIONS pass.
        assert!(validate_token(&tok, "sbx_a", 5173, "GET", 1_000_500, &lookup).is_ok());
        assert!(validate_token(&tok, "sbx_a", 5173, "HEAD", 1_000_500, &lookup).is_ok());
        assert!(validate_token(&tok, "sbx_a", 5173, "OPTIONS", 1_000_500, &lookup).is_ok());
    }

    #[test]
    fn rw_scope_allows_all_methods() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let claims = fresh_claims("sbx_a", 5173, "rw");
        let tok = mint(&claims, &secret);
        for m in ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"] {
            assert!(
                validate_token(&tok, "sbx_a", 5173, m, 1_000_500, &lookup).is_ok(),
                "method {m} must pass with rw scope"
            );
        }
    }

    #[test]
    fn revoked_when_sv_unknown() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let mut claims = fresh_claims("sbx_a", 5173, "ro");
        claims.sv = 99;
        let tok = mint(&claims, &secret);
        let err = validate_token(&tok, "sbx_a", 5173, "GET", 1_000_500, &lookup)
            .unwrap_err();
        assert_eq!(err, TokenError::Revoked);
        assert_eq!(err.wire_code(), "revoked");
    }

    /// Round-6 H5 — the validator MUST hash on-wire bytes, not
    /// re-encode JSON. Two tokens with different field orders both
    /// validate iff the validator is wire-byte-faithful.
    #[test]
    fn hmac_input_invariant_on_wire_bytes() {
        let secret = [0x55; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        // Two byte-distinct payloads with the SAME logical claim set.
        // Hand-build them to control field order.
        let json_a = br#"{"aud":"preview","sbx":"sbx_a","port":5173,"iat":1000000,"exp":1003600,"sv":1,"tid":"x","scope":"ro"}"#;
        let json_b = br#"{"port":5173,"sbx":"sbx_a","aud":"preview","sv":1,"tid":"x","scope":"ro","iat":1000000,"exp":1003600}"#;
        for payload in [json_a.as_slice(), json_b.as_slice()] {
            let payload_b = URL_SAFE_NO_PAD.encode(payload);
            let mut mac = HmacSha256::new_from_slice(&secret).unwrap();
            mac.update(payload_b.as_bytes());
            let sig_b = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes());
            let tok = format!("{payload_b}~{sig_b}");
            let got = validate_token(&tok, "sbx_a", 5173, "GET", 1_000_500, &lookup);
            assert!(
                got.is_ok(),
                "field order independent — both must validate; got {got:?}"
            );
        }
    }

    /// Round-6 H7: token without `iss` validates (legacy compat).
    #[test]
    fn missing_iss_validates() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let mut claims = fresh_claims("sbx_a", 5173, "ro");
        claims.iss = None;
        let tok = mint(&claims, &secret);
        let got = validate_token(&tok, "sbx_a", 5173, "GET", 1_000_500, &lookup)
            .expect("validate");
        assert!(got.iss.is_none());
    }

    #[test]
    fn unknown_scope_rejected() {
        let secret = [0xab; 32];
        let lookup = FixedSecrets { sv: 1, bytes: secret };
        let mut claims = fresh_claims("sbx_a", 5173, "ro");
        claims.scope = "evil".into();
        let tok = mint(&claims, &secret);
        assert_eq!(
            validate_token(&tok, "sbx_a", 5173, "GET", 1_000_500, &lookup)
                .unwrap_err(),
            TokenError::UnknownScope
        );
    }

    #[test]
    fn fresh_token_id_is_22_chars_base64url() {
        let id = fresh_token_id();
        assert_eq!(id.len(), 22, "16 bytes -> 22 base64url chars");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn scope_allows_method_smoke() {
        assert!(scope_allows_method("ro", "GET"));
        assert!(scope_allows_method("ro", "HEAD"));
        assert!(scope_allows_method("ro", "OPTIONS"));
        assert!(!scope_allows_method("ro", "POST"));
        assert!(!scope_allows_method("ro", "DELETE"));
        assert!(scope_allows_method("rw", "GET"));
        assert!(scope_allows_method("rw", "POST"));
        assert!(!scope_allows_method("evil", "GET"));
    }
}
