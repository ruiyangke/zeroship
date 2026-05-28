//! OIDC Back-Channel Logout 1.0 `logout_token` JWT verifier.
//!
//! When a user signs out via hydra's `/oauth2/sessions/logout` endpoint,
//! hydra POSTs a signed `logout_token` JWT to every RP's registered
//! `backchannel_logout_uri`. The RP must verify the JWT and revoke the
//! affected sessions.
//!
//! This module owns the verification side of that contract. The
//! signature-verification path reuses the existing [`JwksCache`] (with
//! its 5-min TTL + on-failure refresh) from [`crate::oidc_verify`]; the
//! BCL-specific claim checks (events marker, nonce-must-be-absent,
//! sub/sid presence, iat freshness) live here.
//!
//! Why not call [`crate::oidc_verify::verify_id_token`] and re-parse?
//! That function returns
//! [`crate::oidc_verify::TokenClaims`], which requires `sub: String`
//! and `exp: i64`. BCL `logout_token` JWTs do not require either claim
//! (`sub` is optional — `sid` may stand alone — and `exp` SHOULD NOT
//! be present per spec §2.4). So we do the JWT decode locally with a
//! shape tailored to BCL.
//!
//! Spec: <https://openid.net/specs/openid-connect-backchannel-1_0.html>

use std::collections::HashSet;

use jsonwebtoken::{decode, decode_header, Validation};
use serde::{Deserialize, Serialize};

use crate::oidc_verify::{JwksCache, OidcError};

/// The OIDC BCL 1.0 `events` claim marker. Per §2.4 the `events` claim
/// MUST contain exactly this key (with an empty object value).
pub const BCL_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

/// Maximum allowed skew between the JWT `iat` and the verifier's clock.
/// Tokens older than this (or that claim to be issued from the future
/// by more than this) are rejected. 5 minutes is the OIDC convention
/// and matches what we accept on ID tokens.
const IAT_SKEW_SECS: i64 = 300;

/// Parsed BCL `logout_token` claims. Constructed only by
/// [`verify`] — callers should never deserialize this struct directly
/// from an untrusted token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutToken {
    /// Issuer — matches the configured hydra issuer URL.
    pub iss: String,
    /// Audience. Per RFC 7519 this is either a string or an array of
    /// strings; we keep it as `Value` and let the signature-verification
    /// step do the actual match.
    pub aud: serde_json::Value,
    /// Issue time (seconds since UNIX epoch).
    pub iat: i64,
    /// Unique token id — for replay defense at the RP. The verifier
    /// surfaces this but does not itself track seen `jti`s; callers can
    /// add a one-shot cache if they need stricter replay semantics.
    pub jti: String,
    /// Event marker(s). Must contain [`BCL_EVENT`] with an empty-object
    /// value. Use a [`HashSet`] of keys for cheap membership lookups —
    /// the value shape (`{}`) isn't validated beyond "present".
    pub events: std::collections::BTreeMap<String, serde_json::Value>,
    /// Subject (end-user id). Either `sub` or `sid` (or both) MUST be
    /// present.
    #[serde(default)]
    pub sub: Option<String>,
    /// Session id (hydra-issued). Either `sub` or `sid` (or both) MUST
    /// be present.
    #[serde(default)]
    pub sid: Option<String>,
    /// MUST NOT be present per BCL §2.4. If found, [`verify`] rejects
    /// with [`LogoutError::NonceForbidden`].
    #[serde(default)]
    pub nonce: Option<String>,
}

/// Errors surfaced by [`verify`]. `Verify` wraps the shared
/// [`OidcError`] from [`crate::oidc_verify`]; the rest are BCL-only
/// claim-validation failures.
#[derive(Debug, thiserror::Error)]
pub enum LogoutError {
    #[error("verify: {0}")]
    Verify(#[from] OidcError),

    #[error("nonce claim present (forbidden by OIDC BCL §2.4)")]
    NonceForbidden,

    #[error("events claim missing the backchannel-logout marker")]
    EventsMissing,

    #[error("sub and sid both missing (BCL §2.4 requires at least one)")]
    SubjectMissing,

    #[error("iat outside ±{IAT_SKEW_SECS}s freshness window")]
    Stale,
}

/// Verify a BCL `logout_token` JWT and return its parsed claims.
///
/// Steps (in order):
/// 1. Decode the JWT header → look up the signing key by `kid` + `alg`
///    in the shared [`JwksCache`]. On a cache miss (or first-attempt
///    verify failure), the cache is force-refreshed once and the
///    lookup retried.
/// 2. Verify the signature, `iss`, and `aud` via `jsonwebtoken::decode`.
///    `exp` is NOT required (BCL forbids it); the standard library's
///    default `validate_exp` is disabled accordingly.
/// 3. Run the BCL-specific claim checks:
///    - `nonce` MUST NOT be present
///    - `events` MUST contain the [`BCL_EVENT`] marker
///    - at least one of `sub` / `sid` MUST be present
///    - `iat` MUST be within ±5 min of the verifier's clock
///
/// # Errors
///
/// See [`LogoutError`] variants. `Verify` wraps signature/issuer/audience/
/// key-lookup failures from the shared cache; the remaining variants are
/// BCL-specific claim-validation failures.
pub async fn verify(
    cache: &JwksCache,
    token: &str,
    expected_iss: &str,
    expected_aud: &str,
) -> Result<LogoutToken, LogoutError> {
    // 1. Decode header → kid + alg.
    let header = decode_header(token).map_err(|e| OidcError::DecodeHeader(e.to_string()))?;
    let kid = header
        .kid
        .clone()
        .ok_or_else(|| OidcError::DecodeHeader("no kid".into()))?;
    let alg = header.alg;

    // 2. Build the jsonwebtoken Validation. BCL §2.4: tokens have `iat`
    //    but SHOULD NOT have `exp`, so we drop `exp` from the required
    //    set and turn off expiry validation. We keep iss/aud validation
    //    enabled — that's what the signed-token contract requires.
    let mut validation = Validation::new(alg);
    validation.set_issuer(&[expected_iss]);
    validation.set_audience(&[expected_aud]);
    validation.validate_exp = false;
    // The default required set includes "exp"; drop it (BCL forbids `exp`).
    // Keep "iss" and "aud" — those are still mandatory.
    validation.required_spec_claims = {
        let mut s = HashSet::new();
        s.insert("iss".to_string());
        s.insert("aud".to_string());
        s
    };

    // Closure that runs the actual decode against a key set — used twice
    // (once on the cached set, once after a forced refresh).
    let try_verify = |keys: Vec<crate::oidc_verify::CachedKey>| -> Result<LogoutToken, OidcError> {
        let key = keys
            .iter()
            .find(|k| k.kid == kid && k.alg == alg)
            .ok_or_else(|| OidcError::NoMatchingKey(kid.clone()))?;
        let data: jsonwebtoken::TokenData<LogoutToken> =
            decode(token, &key.decoding, &validation)
                .map_err(|e| OidcError::Verify(e.to_string()))?;
        Ok(data.claims)
    };

    let claims = match try_verify(cache.keys().await?) {
        Ok(c) => c,
        // Only force-refresh on a "no matching key" miss — that's the
        // signal that the JWKS rotated under us. Signature/claim
        // failures aren't fixable by refetching the JWKS, and forcing
        // a refresh on them just doubles the latency on every bad
        // token (and breaks the `for_test` cache, which has no
        // reachable JWKS URL).
        Err(OidcError::NoMatchingKey(_)) => {
            cache.refresh().await?;
            try_verify(cache.keys().await?)?
        }
        Err(e) => return Err(LogoutError::Verify(e)),
    };

    // 3. BCL-specific claim checks.
    //
    //    Defense-in-depth iss check — `jsonwebtoken` already validated
    //    iss against the validation config above, but re-checking here
    //    makes the contract obvious to callers and matches the pattern
    //    in `oidc_verify::verify_id_token`.
    if claims.iss != expected_iss {
        return Err(LogoutError::Verify(OidcError::IssuerMismatch {
            expected: expected_iss.into(),
            got: claims.iss,
        }));
    }
    if claims.nonce.is_some() {
        return Err(LogoutError::NonceForbidden);
    }
    if !claims.events.contains_key(BCL_EVENT) {
        return Err(LogoutError::EventsMissing);
    }
    if claims.sub.is_none() && claims.sid.is_none() {
        return Err(LogoutError::SubjectMissing);
    }
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0);
    if (now_secs - claims.iat).abs() > IAT_SKEW_SECS {
        return Err(LogoutError::Stale);
    }

    Ok(claims)
}

/// Decode-only path for tests that want to inspect the unverified body
/// (e.g. cross-check what we sign matches what we parse). Not exposed
/// outside the crate.
#[cfg(test)]
fn decode_body_unverified(token: &str) -> Option<LogoutToken> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let body_bytes = URL_SAFE_NO_PAD.decode(parts[1]).ok()?;
    serde_json::from_slice(&body_bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oidc_verify::CachedKey;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::SigningKey;
    use jsonwebtoken::{encode, Algorithm, DecodingKey, EncodingKey, Header};
    use serde_json::json;

    /// Test fixture: deterministic Ed25519 keypair + a [`JwksCache`]
    /// pre-loaded with the matching public key. Mirrors the pattern in
    /// `crates/auth/tests/common/mock_provider.rs` but exposes a direct
    /// [`JwksCache`] constructor (no live HTTP server) — the cache
    /// already memoises the parsed keys, so feeding it from a test-only
    /// constructor short-circuits the network fetch entirely.
    struct TestKey {
        encoding: EncodingKey,
        kid: String,
    }

    fn make_key() -> (TestKey, JwksCache) {
        // Deterministic seed — we don't care what the key is, only that
        // the signer and the JWKS publisher agree.
        let sk = SigningKey::from_bytes(&[7u8; 32]);
        let pkcs8 = sk.to_pkcs8_der().expect("encode pkcs8");
        let encoding = EncodingKey::from_ed_der(pkcs8.as_bytes());
        let pub_b64 = URL_SAFE_NO_PAD.encode(sk.verifying_key().to_bytes());
        let decoding = DecodingKey::from_ed_components(&pub_b64).expect("decode key");

        let kid = "test-kid".to_string();
        let cache = JwksCache::for_test(vec![CachedKey {
            kid: kid.clone(),
            alg: Algorithm::EdDSA,
            decoding,
        }]);
        (TestKey { encoding, kid }, cache)
    }

    /// Sign a JWT with arbitrary claims using the fixture key. `claims`
    /// is a [`serde_json::Value`] so each test can supply exactly the
    /// shape under test (missing fields, extra fields, etc.).
    fn sign(key: &TestKey, claims: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(key.kid.clone());
        encode(&header, claims, &key.encoding).expect("encode jwt")
    }

    /// Builder for the standard "happy path" BCL claims. Tests mutate
    /// the returned `Value` to construct negative cases.
    fn happy_claims() -> serde_json::Value {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        json!({
            "iss": "https://auth.zeroship.ai/",
            "aud": "gateway",
            "iat": now,
            "jti": "jti-abc-123",
            "events": { BCL_EVENT: {} },
            "sub": "usr_alice",
        })
    }

    #[compio::test]
    async fn happy_path_returns_logout_token() {
        let (key, cache) = make_key();
        let token = sign(&key, &happy_claims());
        let claims = verify(&cache, &token, "https://auth.zeroship.ai/", "gateway")
            .await
            .expect("verify");
        assert_eq!(claims.sub.as_deref(), Some("usr_alice"));
        assert_eq!(claims.iss, "https://auth.zeroship.ai/");
        assert_eq!(claims.jti, "jti-abc-123");
        assert!(claims.events.contains_key(BCL_EVENT));
        assert!(claims.nonce.is_none());
        // Sanity: the unverified decoder yields the same body.
        let unverified = decode_body_unverified(&token).expect("decode body");
        assert_eq!(unverified.sub.as_deref(), Some("usr_alice"));
    }

    #[compio::test]
    async fn rejects_stale_iat() {
        let (key, cache) = make_key();
        let mut c = happy_claims();
        // 10 minutes in the past — outside the ±5min window.
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        c["iat"] = json!(now - 600);
        let token = sign(&key, &c);
        let err = verify(&cache, &token, "https://auth.zeroship.ai/", "gateway")
            .await
            .expect_err("must reject stale iat");
        assert!(matches!(err, LogoutError::Stale), "got: {err:?}");
    }

    #[compio::test]
    async fn rejects_missing_events_marker() {
        let (key, cache) = make_key();
        let mut c = happy_claims();
        // Replace events with one that lacks the BCL marker.
        c["events"] = json!({ "some-other-event": {} });
        let token = sign(&key, &c);
        let err = verify(&cache, &token, "https://auth.zeroship.ai/", "gateway")
            .await
            .expect_err("must reject missing BCL event");
        assert!(matches!(err, LogoutError::EventsMissing), "got: {err:?}");
    }

    #[compio::test]
    async fn rejects_when_both_sub_and_sid_missing() {
        let (key, cache) = make_key();
        let mut c = happy_claims();
        // Drop sub, don't add sid.
        c.as_object_mut().unwrap().remove("sub");
        let token = sign(&key, &c);
        let err = verify(&cache, &token, "https://auth.zeroship.ai/", "gateway")
            .await
            .expect_err("must reject missing subject");
        assert!(matches!(err, LogoutError::SubjectMissing), "got: {err:?}");
    }

    #[compio::test]
    async fn rejects_when_nonce_present() {
        let (key, cache) = make_key();
        let mut c = happy_claims();
        c["nonce"] = json!("forbidden-by-spec");
        let token = sign(&key, &c);
        let err = verify(&cache, &token, "https://auth.zeroship.ai/", "gateway")
            .await
            .expect_err("must reject nonce present");
        assert!(
            matches!(err, LogoutError::NonceForbidden),
            "got: {err:?}"
        );
    }

    #[compio::test]
    async fn rejects_audience_mismatch() {
        let (key, cache) = make_key();
        let token = sign(&key, &happy_claims());
        // Expected aud is "control" but the token carries "gateway".
        let err = verify(&cache, &token, "https://auth.zeroship.ai/", "control")
            .await
            .expect_err("must reject aud mismatch");
        // jsonwebtoken surfaces the aud failure inside a Verify(...) error.
        assert!(
            matches!(err, LogoutError::Verify(OidcError::Verify(_))),
            "got: {err:?}"
        );
    }

    #[compio::test]
    async fn sid_only_is_accepted() {
        // BCL §2.4: either sub OR sid suffices. Make sure sid-only
        // tokens verify (gives the gateway a path to per-session revoke
        // even when sub isn't known).
        let (key, cache) = make_key();
        let mut c = happy_claims();
        c.as_object_mut().unwrap().remove("sub");
        c["sid"] = json!("ses_xyz");
        let token = sign(&key, &c);
        let claims = verify(&cache, &token, "https://auth.zeroship.ai/", "gateway")
            .await
            .expect("verify sid-only");
        assert!(claims.sub.is_none());
        assert_eq!(claims.sid.as_deref(), Some("ses_xyz"));
    }
}
