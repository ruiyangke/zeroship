//! RFC 9449 — OAuth 2.0 Demonstrating Proof-of-Possession (DPoP).
//!
//! A DPoP proof is a JWT (signed with a per-client ephemeral keypair)
//! that travels in the `DPoP:` request header alongside an access
//! token. Receivers verify the proof to bind the token to the
//! presenter's public key (the "jkt" thumbprint).
//!
//! This module owns the **proof verifier**. We:
//!
//! 1. Hand-decode the JWT header (because the embedded `jwk` is not
//!    exposed by `jsonwebtoken::decode_header` on every release we
//!    pin to). The header parsing yields a [`DpopHeader`] whose
//!    `typ` and `alg` are checked against an allowlist before any
//!    crypto runs.
//! 2. Build a `jsonwebtoken::DecodingKey` from the embedded JWK
//!    (RSA / EC / OKP). DPoP forbids HMAC algorithms (`HS*`) and the
//!    unsigned `none` algorithm.
//! 3. Verify the signature with `jsonwebtoken::decode`. DPoP body
//!    claims (`htm`, `htu`, `iat`, `jti`, optional `ath`) are NOT
//!    the standard JWT spec claims — we clear `required_spec_claims`
//!    and disable `validate_exp` so the library doesn't reject for
//!    missing `sub`/`exp`/`aud`.
//! 4. Cross-check the body: `htm` ↔ HTTP method, `htu` ↔ request URI
//!    (query/fragment stripped on both sides), `iat` within ±60 s,
//!    `ath` matches `base64url(SHA-256(access_token))` when one is
//!    provided.
//! 5. Compute the RFC 7638 JWK thumbprint over the **canonical** JSON
//!    of the required JWK members (NOT `serde_json::to_string` —
//!    that may emit extra members like `kid`/`use` and isn't
//!    guaranteed lexicographically ordered). Return that thumbprint
//!    as `jkt` so the caller can match it against the access token's
//!    `cnf.jkt` claim.
//!
//! Replay defense (the `jti` cache) is the caller's responsibility —
//! we surface `jti` in [`VerifiedDpop`] but do not track seen ids
//! ourselves; the auth-server backed cache lives in a separate module.

use std::collections::HashSet;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Tolerance window (in seconds) for the `iat` claim. RFC 9449 §4.3
/// recommends a "reasonably short" lifetime; 60 s matches what the
/// reference implementations (and the major IdPs that ship DPoP) use.
const IAT_SKEW_SECS: i64 = 60;

/// DPoP proof allowlist. RFC 9449 §4.2 says the `alg` claim MUST be
/// an asymmetric digital-signature algorithm — `none` and the
/// `HS*` family are explicitly forbidden. `jsonwebtoken` v9 does not
/// expose `ES512`, `PS384`, or `PS512`; we map only the ones it
/// supports.
const ALLOWED_ALGS: &[&str] = &["ES256", "ES384", "RS256", "RS384", "PS256", "EdDSA"];

/// The required `typ` header value for a DPoP proof per RFC 9449 §4.2.
const DPOP_TYP: &str = "dpop+jwt";

/// Successful verification surfaces the JWK thumbprint (`jkt`) — used
/// to match against the access token's `cnf.jkt` — and the proof's
/// `jti`, which the caller is expected to cache for replay defense.
#[derive(Debug, Clone)]
pub struct VerifiedDpop {
    /// RFC 7638 JWK thumbprint of the proof's signing key. base64url,
    /// no padding. This is what the access token's `cnf.jkt` claim
    /// points at.
    pub jkt: String,
    /// The proof's `jti` claim, surfaced so callers can run their own
    /// replay-prevention cache. The verifier itself is stateless.
    pub jti: String,
}

/// Failure modes for [`verify`]. Each variant maps 1:1 to a specific
/// rule in RFC 9449; callers can pattern-match for telemetry.
#[derive(Debug, thiserror::Error)]
pub enum DpopError {
    #[error("JWT not in header.body.signature form")]
    Malformed,

    #[error("JWT header decode: {0}")]
    HeaderDecode(String),

    #[error("JWT body decode: {0}")]
    BodyDecode(String),

    #[error("typ MUST be 'dpop+jwt', got '{0}'")]
    WrongTyp(String),

    #[error("alg '{0}' is not in the DPoP allowlist (no symmetric/none algs)")]
    UnsupportedAlg(String),

    #[error("JWK header missing or malformed: {0}")]
    BadJwk(String),

    #[error("signature verification failed: {0}")]
    BadSignature(String),

    #[error("htm mismatch: expected '{expected}', got '{got}'")]
    HtmMismatch { expected: String, got: String },

    #[error("htu mismatch")]
    HtuMismatch,

    #[error("iat outside ±{IAT_SKEW_SECS}s freshness window")]
    Stale,

    #[error("ath claim missing or does not match access token hash")]
    AthMismatch,
}

/// The minimal JWT header shape we accept. We hand-parse this rather
/// than going through `jsonwebtoken::decode_header` because the
/// embedded `jwk` is not exposed as a typed field across all
/// `jsonwebtoken` releases we pin to. Storing the JWK as
/// `serde_json::Value` lets us pull out the per-kty members below.
#[derive(Debug, Deserialize)]
struct DpopHeader {
    typ: String,
    alg: String,
    jwk: serde_json::Value,
}

/// DPoP proof body claims (RFC 9449 §4.2). `ath` is optional; it's
/// required only when an access token accompanies the proof.
#[derive(Debug, Serialize, Deserialize)]
struct DpopBody {
    jti: String,
    htm: String,
    htu: String,
    iat: i64,
    #[serde(default)]
    ath: Option<String>,
}

/// Verify a DPoP proof JWT.
///
/// `expected_method` and `expected_uri` are the exact HTTP method and
/// the canonical request URI of the request the proof accompanies —
/// both should already have query/fragment stripped at the caller, but
/// [`htu_matches`] re-strips defensively.
///
/// When `access_token` is `Some(t)`, the proof's `ath` claim must equal
/// `base64url(SHA-256(t))`. When it's `None`, `ath` is ignored (the
/// proof is being used for token-request binding, not resource access).
///
/// `now_secs` is the verifier's clock as UNIX-seconds, passed in so
/// callers can pin time for deterministic tests.
///
/// # Errors
///
/// See [`DpopError`]. Each variant matches one of the numbered checks
/// in the module docs.
pub fn verify(
    proof: &str,
    expected_method: &str,
    expected_uri: &str,
    access_token: Option<&str>,
    now_secs: i64,
) -> Result<VerifiedDpop, DpopError> {
    // 1. Split header.body.sig.
    let parts: Vec<&str> = proof.split('.').collect();
    if parts.len() != 3 {
        return Err(DpopError::Malformed);
    }

    // 2. Decode the header.
    let header_bytes = URL_SAFE_NO_PAD
        .decode(parts[0])
        .map_err(|e| DpopError::HeaderDecode(e.to_string()))?;
    let header: DpopHeader = serde_json::from_slice(&header_bytes)
        .map_err(|e| DpopError::HeaderDecode(e.to_string()))?;

    // 3. typ MUST be exactly "dpop+jwt".
    if header.typ != DPOP_TYP {
        return Err(DpopError::WrongTyp(header.typ));
    }

    // 4. alg MUST be in the allowlist.
    if !ALLOWED_ALGS.contains(&header.alg.as_str()) {
        return Err(DpopError::UnsupportedAlg(header.alg));
    }
    let alg = alg_from_str(&header.alg)
        .ok_or_else(|| DpopError::UnsupportedAlg(header.alg.clone()))?;

    // 5. Decode the body. We do it explicitly so the body's `iat`,
    //    `htm`, etc. are typed up front; `jsonwebtoken::decode` would
    //    also do this, but we want a clear error variant if the body
    //    is structurally bad before any crypto runs.
    let body_bytes = URL_SAFE_NO_PAD
        .decode(parts[1])
        .map_err(|e| DpopError::BodyDecode(e.to_string()))?;
    let _body_typed: DpopBody = serde_json::from_slice(&body_bytes)
        .map_err(|e| DpopError::BodyDecode(e.to_string()))?;

    // 6. Build a DecodingKey from the embedded jwk.
    let decoding = decoding_key_from_jwk(&header.jwk)?;

    // 7. Verify the signature via jsonwebtoken. DPoP body has none of
    //    the standard JWT spec claims; clear the required set and
    //    disable expiry validation. The signature check is the only
    //    crypto we want from `decode` here.
    let mut validation = Validation::new(alg);
    validation.required_spec_claims = HashSet::new();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    let data: jsonwebtoken::TokenData<DpopBody> =
        decode(proof, &decoding, &validation).map_err(|e| DpopError::BadSignature(e.to_string()))?;
    let body = data.claims;

    // 8. htm — compare exactly. RFC 9449 §4.2 says the proof's method
    //    SHOULD match the HTTP method value; method strings are
    //    conventionally uppercase, so "post" must not validate as
    //    "POST".
    if body.htm != expected_method {
        return Err(DpopError::HtmMismatch {
            expected: expected_method.to_string(),
            got: body.htm,
        });
    }

    // 9. htu — strip query/fragment on both sides.
    if !htu_matches(&body.htu, expected_uri) {
        return Err(DpopError::HtuMismatch);
    }

    // 10. iat — within ±IAT_SKEW_SECS of the verifier's clock.
    if (now_secs - body.iat).abs() > IAT_SKEW_SECS {
        return Err(DpopError::Stale);
    }

    // 11. ath — only checked when an access token accompanies the
    //     proof.
    if let Some(token) = access_token {
        let expected_ath = URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()));
        match body.ath.as_deref() {
            Some(got) if got == expected_ath => {}
            _ => return Err(DpopError::AthMismatch),
        }
    }

    // 12. Compute the JWK thumbprint and return.
    let jkt = jwk_thumbprint(&header.jwk)?;
    Ok(VerifiedDpop { jkt, jti: body.jti })
}

/// Map the on-the-wire `alg` string to a `jsonwebtoken::Algorithm`.
/// The caller has already filtered via [`ALLOWED_ALGS`]; this is just
/// the type-converter.
fn alg_from_str(alg: &str) -> Option<Algorithm> {
    Some(match alg {
        "ES256" => Algorithm::ES256,
        "ES384" => Algorithm::ES384,
        "RS256" => Algorithm::RS256,
        "RS384" => Algorithm::RS384,
        "PS256" => Algorithm::PS256,
        "EdDSA" => Algorithm::EdDSA,
        _ => return None,
    })
}

/// Build a `DecodingKey` from a JWK embedded in a DPoP proof header.
/// Supports RSA / EC / OKP (Ed25519) — the three key types DPoP allows
/// in practice. Anything else gets a [`DpopError::BadJwk`].
fn decoding_key_from_jwk(jwk: &serde_json::Value) -> Result<DecodingKey, DpopError> {
    let kty = jwk
        .get("kty")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DpopError::BadJwk("kty missing".into()))?;
    match kty {
        "RSA" => {
            let n = jwk
                .get("n")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("RSA n missing".into()))?;
            let e = jwk
                .get("e")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("RSA e missing".into()))?;
            DecodingKey::from_rsa_components(n, e)
                .map_err(|err| DpopError::BadJwk(format!("RSA components: {err}")))
        }
        "EC" => {
            let x = jwk
                .get("x")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("EC x missing".into()))?;
            let y = jwk
                .get("y")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("EC y missing".into()))?;
            DecodingKey::from_ec_components(x, y)
                .map_err(|err| DpopError::BadJwk(format!("EC components: {err}")))
        }
        "OKP" => {
            let x = jwk
                .get("x")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("OKP x missing".into()))?;
            DecodingKey::from_ed_components(x)
                .map_err(|err| DpopError::BadJwk(format!("OKP components: {err}")))
        }
        other => Err(DpopError::BadJwk(format!("unsupported kty '{other}'"))),
    }
}

/// Compute the RFC 7638 JWK thumbprint of a DPoP proof's embedded
/// public key.
///
/// The thumbprint is `base64url(SHA-256(canonical-JSON))` where the
/// canonical JSON contains only the **required** JWK members for the
/// `kty`, sorted lexicographically by member name, with no whitespace.
///
/// We hand-build the JSON string rather than serialising the
/// `serde_json::Value` because (a) the input `jwk` may carry extra
/// members like `kid`/`use`/`alg` that MUST NOT appear in the
/// thumbprint input, and (b) `serde_json::to_string` does not
/// guarantee lexicographic key ordering across all input shapes.
fn jwk_thumbprint(jwk: &serde_json::Value) -> Result<String, DpopError> {
    let kty = jwk
        .get("kty")
        .and_then(|v| v.as_str())
        .ok_or_else(|| DpopError::BadJwk("kty missing".into()))?;
    let canonical = match kty {
        "RSA" => {
            // Required members per RFC 7638 §3.2: e, kty, n.
            let e = jwk
                .get("e")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("RSA e missing".into()))?;
            let n = jwk
                .get("n")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("RSA n missing".into()))?;
            format!(r#"{{"e":"{e}","kty":"RSA","n":"{n}"}}"#)
        }
        "EC" => {
            // Required members per RFC 7638 §3.2: crv, kty, x, y.
            let crv = jwk
                .get("crv")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("EC crv missing".into()))?;
            let x = jwk
                .get("x")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("EC x missing".into()))?;
            let y = jwk
                .get("y")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("EC y missing".into()))?;
            format!(r#"{{"crv":"{crv}","kty":"EC","x":"{x}","y":"{y}"}}"#)
        }
        "OKP" => {
            // Required members per RFC 8037 §2: crv, kty, x.
            let crv = jwk
                .get("crv")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("OKP crv missing".into()))?;
            let x = jwk
                .get("x")
                .and_then(|v| v.as_str())
                .ok_or_else(|| DpopError::BadJwk("OKP x missing".into()))?;
            format!(r#"{{"crv":"{crv}","kty":"OKP","x":"{x}"}}"#)
        }
        other => return Err(DpopError::BadJwk(format!("unsupported kty '{other}'"))),
    };
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(canonical.as_bytes())))
}

/// Strip `?…` and `#…` from both URIs and compare. The caller is
/// expected to canonicalise scheme/host casing upstream — DPoP does
/// not specify normalisation beyond "compare the URI"; the
/// query/fragment strip defends against trailing-token smuggling.
fn htu_matches(claimed: &str, actual: &str) -> bool {
    fn normalize(u: &str) -> &str {
        let no_query = u.split('?').next().unwrap_or(u);
        no_query.split('#').next().unwrap_or(no_query)
    }
    normalize(claimed) == normalize(actual)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;

    /// Build a header.body.signature DPoP proof JWT signed with the
    /// supplied Ed25519 signing key. Returns the JWT and the public
    /// JWK so callers can assert on the thumbprint.
    fn sign_ed25519(sk: &SigningKey, claims: &serde_json::Value) -> (String, serde_json::Value) {
        let pk = sk.verifying_key();
        let x = URL_SAFE_NO_PAD.encode(pk.to_bytes());
        let jwk = json!({ "kty": "OKP", "crv": "Ed25519", "x": x });
        let header = json!({ "typ": "dpop+jwt", "alg": "EdDSA", "jwk": jwk });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let body_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let signing_input = format!("{header_b64}.{body_b64}");
        let sig = sk.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        (format!("{signing_input}.{sig_b64}"), jwk)
    }

    /// Deterministic Ed25519 keypair — every test uses the same seed
    /// so failures don't depend on RNG state.
    fn test_key() -> SigningKey {
        SigningKey::from_bytes(&[42u8; 32])
    }

    fn happy_claims(now: i64) -> serde_json::Value {
        json!({
            "jti": "test-jti-1",
            "htm": "POST",
            "htu": "https://api.zeroship.ai/v1/resource",
            "iat": now,
        })
    }

    #[test]
    fn happy_path_verifies() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let (jwt, _jwk) = sign_ed25519(&sk, &happy_claims(now));
        let verified = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("happy path verifies");
        assert_eq!(verified.jti, "test-jti-1");
        assert!(!verified.jkt.is_empty());
    }

    #[test]
    fn rejects_malformed_jwt() {
        let err = verify("not-a-jwt", "GET", "https://x.test/", None, 1_700_000_000)
            .expect_err("must reject");
        assert!(matches!(err, DpopError::Malformed), "got: {err:?}");
    }

    #[test]
    fn rejects_wrong_typ() {
        // Hand-build a JWT with typ="JWT" instead of "dpop+jwt".
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let pk = sk.verifying_key();
        let x = URL_SAFE_NO_PAD.encode(pk.to_bytes());
        let header = json!({
            "typ": "JWT",
            "alg": "EdDSA",
            "jwk": { "kty": "OKP", "crv": "Ed25519", "x": x },
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let body_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&happy_claims(now)).unwrap());
        let signing_input = format!("{header_b64}.{body_b64}");
        let sig = sk.sign(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(sig.to_bytes());
        let jwt = format!("{signing_input}.{sig_b64}");

        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject wrong typ");
        assert!(matches!(err, DpopError::WrongTyp(ref t) if t == "JWT"), "got: {err:?}");
    }

    #[test]
    fn rejects_unsupported_alg_hs256() {
        // RFC 9449 forbids HMAC algorithms. Build a header with
        // alg="HS256" — we should bail out before any signature
        // check.
        let now = 1_700_000_000_i64;
        let header = json!({
            "typ": "dpop+jwt",
            "alg": "HS256",
            "jwk": { "kty": "oct", "k": "secret" },
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let body_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&happy_claims(now)).unwrap());
        // Garbage signature — we'll fail on alg before we touch it.
        let jwt = format!("{header_b64}.{body_b64}.AAAA");

        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject HS256");
        assert!(
            matches!(err, DpopError::UnsupportedAlg(ref a) if a == "HS256"),
            "got: {err:?}"
        );
    }

    #[test]
    fn rejects_unsupported_alg_none() {
        // alg="none" is the canonical JWT footgun. Reject it.
        let now = 1_700_000_000_i64;
        let header = json!({
            "typ": "dpop+jwt",
            "alg": "none",
            "jwk": { "kty": "OKP", "crv": "Ed25519", "x": "abc" },
        });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let body_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&happy_claims(now)).unwrap());
        let jwt = format!("{header_b64}.{body_b64}.");

        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject none");
        assert!(
            matches!(err, DpopError::UnsupportedAlg(ref a) if a == "none"),
            "got: {err:?}"
        );
    }

    #[test]
    fn rejects_missing_jwk() {
        // Header without a jwk field — should reject with BadJwk
        // (the embedded key is non-negotiable per RFC 9449 §4.2).
        let now = 1_700_000_000_i64;
        let header = json!({ "typ": "dpop+jwt", "alg": "EdDSA" });
        let header_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap());
        let body_b64 =
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&happy_claims(now)).unwrap());
        let jwt = format!("{header_b64}.{body_b64}.AAAA");

        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject missing jwk");
        // serde fails the header parse because `jwk` is non-optional
        // on `DpopHeader`; surfaces as HeaderDecode.
        assert!(matches!(err, DpopError::HeaderDecode(_)), "got: {err:?}");
    }

    #[test]
    fn rejects_stale_iat() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        // 5 minutes in the past — well outside the ±60s window.
        claims["iat"] = json!(now - 300);
        let (jwt, _) = sign_ed25519(&sk, &claims);
        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject stale iat");
        assert!(matches!(err, DpopError::Stale), "got: {err:?}");
    }

    #[test]
    fn rejects_iat_from_future() {
        // iat 5 min into the future is just as suspicious as one
        // 5 min in the past.
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        claims["iat"] = json!(now + 300);
        let (jwt, _) = sign_ed25519(&sk, &claims);
        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject future iat");
        assert!(matches!(err, DpopError::Stale), "got: {err:?}");
    }

    #[test]
    fn rejects_htm_mismatch() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let (jwt, _) = sign_ed25519(&sk, &happy_claims(now));
        // Token says POST; we're verifying for a GET.
        let err = verify(&jwt, "GET", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject htm mismatch");
        assert!(matches!(err, DpopError::HtmMismatch { .. }), "got: {err:?}");
    }

    #[test]
    fn htm_compares_exactly_uppercase() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let (jwt, _) = sign_ed25519(&sk, &happy_claims(now));
        verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("exact uppercase htm should match");
    }

    #[test]
    fn htm_rejects_lowercase() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        claims["htm"] = json!("post");
        let (jwt, _) = sign_ed25519(&sk, &claims);
        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject lowercase htm");
        assert!(matches!(err, DpopError::HtmMismatch { .. }), "got: {err:?}");
    }

    #[test]
    fn htm_rejects_mixed_case() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        claims["htm"] = json!("Post");
        let (jwt, _) = sign_ed25519(&sk, &claims);
        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject mixed-case htm");
        assert!(matches!(err, DpopError::HtmMismatch { .. }), "got: {err:?}");
    }

    #[test]
    fn rejects_htu_mismatch() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        claims["htu"] = json!("https://api.zeroship.ai/v1/other");
        let (jwt, _) = sign_ed25519(&sk, &claims);
        let err = verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect_err("must reject htu mismatch");
        assert!(matches!(err, DpopError::HtuMismatch), "got: {err:?}");
    }

    #[test]
    fn htu_strips_query_and_fragment() {
        // Defence against trailing-token smuggling: query/fragment on
        // either side must not affect the match.
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        claims["htu"] = json!("https://api.zeroship.ai/v1/resource?token=foo#frag");
        let (jwt, _) = sign_ed25519(&sk, &claims);
        verify(&jwt, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("query/fragment should be stripped");
    }

    #[test]
    fn ath_matches_when_token_supplied() {
        // When an access token accompanies the proof, the `ath` claim
        // must equal base64url(SHA-256(token)).
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let token = "opaque-access-token-xyz";
        let expected_ath = URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()));
        let mut claims = happy_claims(now);
        claims["ath"] = json!(expected_ath);
        let (jwt, _) = sign_ed25519(&sk, &claims);
        verify(
            &jwt,
            "POST",
            "https://api.zeroship.ai/v1/resource",
            Some(token),
            now,
        )
        .expect("ath should match");
    }

    #[test]
    fn rejects_ath_for_wrong_token() {
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut claims = happy_claims(now);
        claims["ath"] = json!("totally-wrong-hash");
        let (jwt, _) = sign_ed25519(&sk, &claims);
        let err = verify(
            &jwt,
            "POST",
            "https://api.zeroship.ai/v1/resource",
            Some("real-token"),
            now,
        )
        .expect_err("must reject ath mismatch");
        assert!(matches!(err, DpopError::AthMismatch), "got: {err:?}");
    }

    #[test]
    fn rejects_missing_ath_when_token_required() {
        // Token supplied to verify() but proof body has no ath →
        // reject (can't bind the token).
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let (jwt, _) = sign_ed25519(&sk, &happy_claims(now));
        let err = verify(
            &jwt,
            "POST",
            "https://api.zeroship.ai/v1/resource",
            Some("some-token"),
            now,
        )
        .expect_err("must reject missing ath");
        assert!(matches!(err, DpopError::AthMismatch), "got: {err:?}");
    }

    #[test]
    fn rejects_bad_signature() {
        // Flip one byte in the signature — must fail signature
        // verification.
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let (jwt, _) = sign_ed25519(&sk, &happy_claims(now));
        // The signature is the third dot-segment. Swap its last char
        // for something that still base64url-decodes but produces a
        // different signature byte string.
        let mut parts: Vec<String> = jwt.split('.').map(String::from).collect();
        let sig = parts.last_mut().unwrap();
        // Replace last char with a different base64url char.
        let last = sig.pop().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        sig.push(replacement);
        let tampered = parts.join(".");

        let err = verify(
            &tampered,
            "POST",
            "https://api.zeroship.ai/v1/resource",
            None,
            now,
        )
        .expect_err("must reject bad signature");
        assert!(matches!(err, DpopError::BadSignature(_)), "got: {err:?}");
    }

    #[test]
    fn jwk_thumbprint_is_stable_across_calls() {
        // Two proofs from the SAME key → identical jkt. This is the
        // invariant that lets a resource server bind a token to a
        // long-lived public key.
        let now = 1_700_000_000_i64;
        let sk = test_key();
        let mut c1 = happy_claims(now);
        c1["jti"] = json!("first");
        let mut c2 = happy_claims(now);
        c2["jti"] = json!("second");
        let (jwt1, _) = sign_ed25519(&sk, &c1);
        let (jwt2, _) = sign_ed25519(&sk, &c2);
        let v1 = verify(&jwt1, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("v1");
        let v2 = verify(&jwt2, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("v2");
        assert_eq!(v1.jkt, v2.jkt, "same key must yield same thumbprint");
        assert_ne!(v1.jti, v2.jti, "different jti expected");
    }

    #[test]
    fn jwk_thumbprint_differs_across_keys() {
        // Two different signing keys → two different thumbprints.
        // This is the other half of the binding invariant.
        let now = 1_700_000_000_i64;
        let sk_a = SigningKey::from_bytes(&[11u8; 32]);
        let sk_b = SigningKey::from_bytes(&[22u8; 32]);
        let (jwt_a, _) = sign_ed25519(&sk_a, &happy_claims(now));
        let (jwt_b, _) = sign_ed25519(&sk_b, &happy_claims(now));
        let va = verify(&jwt_a, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("va");
        let vb = verify(&jwt_b, "POST", "https://api.zeroship.ai/v1/resource", None, now)
            .expect("vb");
        assert_ne!(va.jkt, vb.jkt, "different keys must yield different jkts");
    }

    #[test]
    fn jwk_thumbprint_matches_rfc7638_okp_canonical_form() {
        // Spot-check the canonical-JSON shape used for OKP thumbprints.
        // RFC 7638 §3.2 requires the members `{crv, kty, x}` in
        // lexicographic order with no whitespace. We assert the
        // resulting thumbprint matches what an independent
        // SHA-256(canonical) computation gives.
        let pk_bytes = test_key().verifying_key().to_bytes();
        let x = URL_SAFE_NO_PAD.encode(pk_bytes);
        let jwk = json!({ "kty": "OKP", "crv": "Ed25519", "x": x });

        let expected_canonical = format!(r#"{{"crv":"Ed25519","kty":"OKP","x":"{x}"}}"#);
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(expected_canonical.as_bytes()));
        let got = jwk_thumbprint(&jwk).expect("thumbprint");
        assert_eq!(got, expected);
    }
}

// ─── jti replay protection ─────────────────────────────────────────────
//
// Bounded in-process LRU-ish cache for DPoP `jti` values. Phase 7 v1
// ships this as a per-instance cache; for multi-instance gateway
// deployments, a replay-across-instances attack is possible during the
// freshness window (±60s by default). A PG-backed `auth.dpop_jti` table
// sweep'd by the same retention pattern as `audit_retention` is the
// Phase 8+ hardening path.

use std::collections::HashMap;
use std::sync::Mutex;

/// Bounded in-process cache of `DPoP` `jti` values for replay protection.
///
/// Each entry stores its absolute `expires_at_secs`; expired entries are
/// swept lazily on every [`JtiCache::insert`] call (no background task,
/// no separate timer thread — keeps the runtime tokio-free).
///
/// **Multi-instance caveat.** This cache is per-process. In a multi-node
/// gateway deployment a replay landing on a different instance during
/// the ±60 s freshness window will NOT be detected. The Phase 8+ plan
/// promotes the cache to a shared PG-backed `auth.dpop_jti` table
/// (sweep'd by the same retention pattern as `audit_retention`).
///
/// # Panics
///
/// The accessor methods (`insert`, `len`, `is_empty`) acquire an
/// internal `Mutex` and will panic if the lock has been poisoned by
/// another thread panicking while holding it. In practice that
/// requires a panic inside the very small critical sections in this
/// file, none of which can panic on well-formed input.
#[derive(Debug)]
pub struct JtiCache {
    inner: Mutex<HashMap<String, i64>>, // jti → expires_at_secs
    max_entries: usize,
}

impl JtiCache {
    /// Build a cache with the given maximum entries. Choose a value
    /// comfortably larger than peak QPS × the `DPoP` freshness window —
    /// e.g. 100 RPS × 120 s = 12 000 jtis; size to 50 000 for headroom.
    #[must_use]
    pub fn new(max_entries: usize) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            max_entries,
        }
    }

    /// Attempt to insert a fresh `jti`. Returns `true` if it is new and
    /// was inserted; `false` if the `jti` was already present (replay
    /// detected).
    ///
    /// Expired entries are swept lazily on each insert. When the cache
    /// is at `max_entries` after the sweep, one arbitrary entry is
    /// dropped to make room — exact LRU behaviour is not required for
    /// correctness (the freshness window already bounds the attack
    /// surface), and a `HashMap`-based "drop any" policy keeps the
    /// implementation contention-free.
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` has been poisoned by another
    /// thread panicking while holding the lock. The critical section
    /// here cannot itself panic on well-formed input.
    pub fn insert(&self, jti: &str, now_secs: i64, ttl_secs: i64) -> bool {
        let mut guard = self.inner.lock().expect("poisoned");
        // 1. Evict expired entries.
        guard.retain(|_, expires_at| *expires_at > now_secs);
        // 2. Replay check.
        if guard.contains_key(jti) {
            return false;
        }
        // 3. Capacity guard: drop one arbitrary entry if at the limit.
        if guard.len() >= self.max_entries {
            if let Some(victim) = guard.keys().next().cloned() {
                guard.remove(&victim);
            }
        }
        // 4. Insert.
        guard.insert(jti.to_string(), now_secs + ttl_secs);
        true
    }

    /// Current live entry count (post-last-sweep). Useful for metrics.
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` has been poisoned.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner.lock().expect("poisoned").len()
    }

    /// Whether the cache is currently empty.
    ///
    /// # Panics
    ///
    /// Panics if the internal `Mutex` has been poisoned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.lock().expect("poisoned").is_empty()
    }
}

impl Default for JtiCache {
    /// 50 000 entries — comfortable headroom for ~100 RPS sustained
    /// over the default 120 s `DPoP` freshness window.
    fn default() -> Self {
        Self::new(50_000)
    }
}

#[cfg(test)]
mod jti_cache_tests {
    use super::*;

    #[test]
    fn first_insert_returns_true_and_grows() {
        let cache = JtiCache::new(10);
        assert!(cache.insert("jti-1", 1000, 60));
        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());
    }

    #[test]
    fn duplicate_insert_returns_false_and_does_not_grow() {
        let cache = JtiCache::new(10);
        assert!(cache.insert("jti-1", 1000, 60));
        assert!(!cache.insert("jti-1", 1001, 60), "replay must be detected");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn expired_entries_are_evicted_on_insert() {
        let cache = JtiCache::new(10);
        assert!(cache.insert("jti-old", 1000, 60)); // expires at 1060
        // Time advances past expiry; insert a fresh jti.
        assert!(cache.insert("jti-new", 1100, 60));
        // Old entry should have been evicted.
        assert_eq!(cache.len(), 1, "stale entry should be swept");
        // Old jti can now be re-used (the cache no longer remembers it).
        assert!(cache.insert("jti-old", 1100, 60));
    }

    #[test]
    fn capacity_caps_entry_count() {
        let cache = JtiCache::new(3);
        assert!(cache.insert("a", 1000, 600));
        assert!(cache.insert("b", 1000, 600));
        assert!(cache.insert("c", 1000, 600));
        assert!(cache.insert("d", 1000, 600));
        // After the 4th insert at capacity, the cache evicts one entry
        // (not necessarily "a" — eviction is arbitrary). Total stays at 3.
        assert_eq!(cache.len(), 3, "cache should not exceed max_entries");
    }

    #[test]
    fn zero_ttl_is_immediately_expired_on_next_insert() {
        let cache = JtiCache::new(10);
        assert!(cache.insert("jti-1", 1000, 0)); // expires at 1000 — effectively now
        // At now_secs == 1000 on the next call, the retain predicate is
        // `expires_at > now_secs`, i.e. 1000 > 1000 is false → evicted.
        let inserted = cache.insert("jti-2", 1000, 60);
        assert!(inserted);
        assert_eq!(cache.len(), 1, "zero-ttl entry should be swept on next insert");
    }

    #[test]
    fn concurrent_inserts_race_safe() {
        // Spawn N threads each inserting a unique jti; verify all
        // succeed and len == N. Uses std::thread (sync test runner;
        // no compio needed).
        use std::sync::Arc;
        use std::thread;
        let cache = Arc::new(JtiCache::new(1000));
        let mut handles = vec![];
        for i in 0..50 {
            let c = cache.clone();
            handles.push(thread::spawn(move || {
                c.insert(&format!("jti-{i}"), 1000, 60)
            }));
        }
        let all_ok = handles.into_iter().all(|h| h.join().unwrap());
        assert!(all_ok, "all unique jti inserts should return true");
        assert_eq!(cache.len(), 50);
    }
}
