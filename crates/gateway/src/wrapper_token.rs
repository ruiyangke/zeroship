//! Gateway-issued wrapper access tokens.
//!
//! Per the Phase 8 plan, the gateway wraps every hydra-issued opaque
//! access token in its own short-lived signed `JWT` before forwarding
//! it to creator-app workers. Compared with passing the raw hydra
//! token through, the wrapper buys us three things:
//!
//! 1. **`DPoP` key binding.** The wrapper carries an RFC 9449
//!    `cnf.jkt` confirmation claim — the `JWK` thumbprint of the
//!    `DPoP` key the client used at the gateway. Downstream verifiers
//!    (the dispatcher on the next hop, the worker on the inside)
//!    reject a wrapper token presented with a different `DPoP` proof
//!    key. The raw hydra introspection response has no equivalent
//!    binding.
//! 2. **Audience scoping.** `aud` is the per-app host
//!    (`{app}.zeroship.ai`), so a wrapper minted for app A cannot be
//!    replayed against app B — even though hydra issued one access
//!    token covering both. The dispatcher (U4) is the audience
//!    enforcer.
//! 3. **Stable typed claims.** The worker receives a `JOSE` `JWT`
//!    with a fixed schema (`sub`, `scope`, `client_id`, `email`, …)
//!    instead of having to call `/oauth2/introspect` itself. The
//!    `wraps` claim ties the wrapper to the underlying hydra token
//!    (SHA-256 of the raw access-token string, base64url-encoded) so
//!    revocation pings can invalidate the wrapper by introspecting
//!    the original.
//!
//! Tokens carry the RFC 9068 `typ: at+jwt` header so they cannot be
//! confused with hydra ID tokens (`typ: JWT`) on the verify side, and
//! are signed `EdDSA` (Ed25519) with the key loaded by
//! [`crate::signing::load_from_path`]. `kid` is the RFC 7638 `JWK`
//! thumbprint so JWKS rotation is automatic — no manual `kid`
//! bookkeeping.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{GatewayError, Result};
use crate::oidc_rp::IntrospectionResponse;

/// Wrapper-token claim set.
///
/// Shape is the `JOSE`/RFC 7519 superset of:
/// - the standard registered claims (`iss`/`aud`/`sub`/`exp`/`iat`/`jti`),
/// - the RFC 9449 `cnf.jkt` `DPoP` confirmation,
/// - hydra introspection projections
///   (`scope`/`client_id`/`email`/`name`/`email_verified`),
/// - and `wraps` — the SHA-256 of the underlying hydra access token,
///   so revocation can map a wrapper back to its origin token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperClaims {
    pub iss: String,
    pub aud: String,
    pub sub: String,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub cnf: Cnf,
    pub scope: String,
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// SHA-256 of the wrapped hydra access-token string,
    /// base64url-no-pad encoded. Used by the revocation back-channel
    /// (Phase 7) to invalidate the wrapper when hydra invalidates the
    /// origin token.
    pub wraps: String,
}

/// RFC 9449 §3.1 confirmation member. Bound to a `DPoP` key by `jkt`
/// (the RFC 7638 `JWK` thumbprint of the client's `DPoP` key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cnf {
    /// Base64url-no-pad SHA-256 of the canonical `JWK` of the
    /// client's `DPoP` key. Verifiers compare this to the thumbprint
    /// of the `JWK` header in the incoming `DPoP` proof —
    /// mismatch ⇒ reject.
    pub jkt: String,
}

/// Wrapper-token issuer.
///
/// Caches the PKCS#8 DER form of the Ed25519 signing key so we encode
/// it only once at construction time — `EncodingKey::from_ed_der`
/// takes the DER bytes by reference but
/// `ed25519_dalek::SigningKey::to_pkcs8_der` returns a `SecretDocument`
/// that's allocated on every call, so caching saves an allocation per
/// issued token.
pub struct Issuer {
    /// PKCS#8 DER bytes of the Ed25519 private key. `EncodingKey` is
    /// built from this on every `issue` call; constructing an
    /// `EncodingKey` from a byte slice is itself a single allocation.
    private_der: Vec<u8>,
    /// RFC 7638 `JWK` thumbprint of the public half — surfaced in
    /// the `JWT` header as `kid` so verifiers can pick the right key
    /// out of the `JWKS`.
    kid: String,
    /// `iss` claim — the gateway's public URL. Verifiers pin this on
    /// the way in.
    iss: String,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omit `private_der` from the Debug output —
        // leaking the signing key's bytes defeats the whole point of
        // the signing key. `.finish_non_exhaustive()` signals that
        // omission to clippy (and to readers).
        f.debug_struct("Issuer")
            .field("kid", &self.kid)
            .field("iss", &self.iss)
            .finish_non_exhaustive()
    }
}

impl Issuer {
    /// Build an Issuer from an Ed25519 `SigningKey`. Caches the
    /// PKCS#8 DER bytes for [`EncodingKey::from_ed_der`] (allocated
    /// once at boot rather than once per issued token).
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] if PKCS#8 DER encoding fails — this
    /// shouldn't happen for any valid Ed25519 key produced by
    /// `SigningKey::from_bytes` or our PKCS#8 loader, but the
    /// `pkcs8::Error` is propagated rather than panicked on.
    pub fn new(signing_key: &ed25519_dalek::SigningKey, issuer_url: String) -> Result<Self> {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let der = signing_key
            .to_pkcs8_der()
            .map_err(|e| GatewayError::Internal(format!("PKCS#8 encode: {e}")))?
            .as_bytes()
            .to_vec();
        let kid = crate::signing::jwk_thumbprint(signing_key);
        Ok(Self {
            private_der: der,
            kid,
            iss: issuer_url,
        })
    }

    /// Issue a wrapper token bound to `proof_jkt`.
    ///
    /// - `aud` — the per-app host the wrapper is destined for
    ///   (`{app}.zeroship.ai`). The dispatcher (U4) checks this matches
    ///   the request's `Host` so wrappers can't cross-pollinate apps.
    /// - `introspection` — the hydra `/oauth2/introspect` response for
    ///   the original opaque access token. Caller MUST have already
    ///   gated on `.active == true`.
    /// - `proof_jkt` — `JWK` thumbprint of the client's `DPoP` key
    ///   (from the verified `DPoP` header on the incoming request).
    ///   Lands in `cnf.jkt`.
    /// - `hydra_token` — the raw hydra access token, hashed into the
    ///   `wraps` claim so revocation can dereference.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] on JWT encoding failure or system
    /// clock failure. Both are extraordinarily unlikely in normal
    /// operation but are propagated rather than panicked on so the
    /// gateway dispatch path can surface a 500.
    pub fn issue(
        &self,
        aud: &str,
        introspection: &IntrospectionResponse,
        proof_jkt: &str,
        hydra_token: &str,
    ) -> Result<String> {
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        // `as_secs()` returns u64. We need i64 for the JWT claim
        // (RFC 7519 numeric date is signed). The conversion is
        // lossless until year 292 277 026 596 — well past the heat
        // death of the sun — so a `try_from + expect` is the
        // clippy-clean way to assert it.
        let now: i64 = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| GatewayError::Internal(format!("clock: {e}")))?
                .as_secs(),
        )
        .map_err(|e| GatewayError::Internal(format!("clock overflow: {e}")))?;

        // `wraps` ties the wrapper to the hydra-issued opaque access
        // token without leaking the token itself: SHA-256 the
        // (utf-8) bytes and base64url-encode.
        let wraps = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(hydra_token.as_bytes()));

        let claims = WrapperClaims {
            iss: self.iss.clone(),
            aud: aud.to_string(),
            // hydra omits `sub` for client-credentials grants — the
            // empty default keeps the JWT valid; downstream callers
            // that need `sub` (user-bound flows) check it on the way
            // in.
            sub: introspection.sub.clone().unwrap_or_default(),
            exp: now + 3600,
            iat: now,
            jti: uuid::Uuid::new_v4().to_string(),
            cnf: Cnf {
                jkt: proof_jkt.to_string(),
            },
            scope: introspection.scope.clone().unwrap_or_default(),
            client_id: introspection.client_id.clone().unwrap_or_default(),
            email: introspection.email.clone(),
            email_verified: introspection.email_verified,
            name: introspection.name.clone(),
            wraps,
        };

        let mut header = Header::new(Algorithm::EdDSA);
        // RFC 9068 §2.1 — access tokens MUST be tagged with this typ
        // so verifiers can distinguish them from ID tokens / generic
        // JWTs without re-parsing the body. The default `Header::new`
        // value is `Some("JWT")`, which we override.
        header.typ = Some("at+jwt".into());
        header.kid = Some(self.kid.clone());

        let key = EncodingKey::from_ed_der(&self.private_der);
        encode(&header, &claims, &key)
            .map_err(|e| GatewayError::Internal(format!("jwt encode: {e}")))
    }

    /// The RFC 7638 JWK thumbprint of the signing key's public half,
    /// surfaced in issued tokens as the `kid` header. Exposed so the
    /// JWKS endpoint (U3) can publish a JWK with the same `kid`.
    ///
    /// Currently unused in the gateway bin — the JWKS handler that
    /// consumes it lands in P8-U3. Lib tests exercise `kid` through
    /// the header parsing in [`Verifier::verify`], so this is not a
    /// stale API.
    #[must_use]
    #[allow(dead_code)]
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

/// Wrapper-token verifier. Holds the public half of the gateway's
/// signing key and the expected `iss` value.
pub struct Verifier {
    /// `DecodingKey` wrapping the raw 32-byte Ed25519 public key —
    /// jsonwebtoken 9.x's `from_ed_der` takes raw public-key bytes
    /// (despite the name; it does NOT expect a PKCS#8 SPKI wrapper),
    /// which `ed25519_dalek::VerifyingKey::as_bytes` returns directly.
    decoding_key: DecodingKey,
    /// Expected `kid` in the JWT header. Pre-checked before signature
    /// verification — a wrong `kid` is rejected with a clear error
    /// rather than a generic "signature verification failed".
    kid: String,
    /// Expected `iss` claim. Pinned in [`Validation::set_issuer`].
    expected_iss: String,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `decoding_key` is omitted because `DecodingKey` doesn't
        // implement Debug — the public key bytes are not secret but
        // there's no useful representation to print here.
        f.debug_struct("Verifier")
            .field("kid", &self.kid)
            .field("expected_iss", &self.expected_iss)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// Build a Verifier from a [`ed25519_dalek::VerifyingKey`] (the
    /// public half of the gateway's signing key).
    ///
    /// `issuer_url` MUST match the `iss` claim the matching
    /// [`Issuer`] writes (i.e. the gateway's public URL).
    #[must_use]
    pub fn new(public_key: &ed25519_dalek::VerifyingKey, issuer_url: String) -> Self {
        let kid = crate::signing::jwk_thumbprint_public(public_key);
        // ring (jsonwebtoken's EdDSA backend) verifies Ed25519 via
        // `signature::UnparsedPublicKey::new(&ED25519, raw_32_bytes)`.
        // The `from_ed_der` constructor stores the bytes verbatim,
        // so the raw 32-byte public key is the right input.
        let decoding_key = DecodingKey::from_ed_der(public_key.as_bytes());
        Self {
            decoding_key,
            kid,
            expected_iss: issuer_url,
        }
    }

    /// Verify a wrapper token's signature + iss + aud + exp + kid + typ.
    /// Returns the decoded claims on success.
    ///
    /// `expected_aud` is the request `Host` value. It is per-request
    /// rather than stored on the verifier because one gateway verifier
    /// serves every app host.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] for any verification failure:
    /// missing/wrong `kid`, wrong `typ`, expired token, wrong issuer,
    /// or invalid signature. Callers translate this into a `401
    /// invalid_token` response.
    pub fn verify(&self, token: &str, expected_aud: &str) -> Result<WrapperClaims> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.expected_iss]);
        validation.set_audience(&[expected_aud]);
        // `validate_exp` is on by default with a 60s leeway, which is
        // what we want.

        // Pre-check the header so a mismatch surfaces as "unknown
        // kid" / "unexpected typ" rather than a generic
        // signature-failure error from `decode`. Cheap — just parses
        // the base64url-encoded JOSE header without touching the
        // signature.
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| GatewayError::Internal(format!("decode header: {e}")))?;
        match header.kid.as_deref() {
            Some(kid) if kid == self.kid => {}
            Some(other) => {
                return Err(GatewayError::Internal(format!("unknown kid: {other}")));
            }
            None => return Err(GatewayError::Internal("missing kid".into())),
        }
        if header.typ.as_deref() != Some("at+jwt") {
            return Err(GatewayError::Internal(format!(
                "unexpected typ: {:?}",
                header.typ
            )));
        }

        let data: jsonwebtoken::TokenData<WrapperClaims> =
            decode(token, &self.decoding_key, &validation)
                .map_err(|e| GatewayError::Internal(format!("jwt verify: {e}")))?;
        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oidc_rp::IntrospectionResponse;
    use ed25519_dalek::SigningKey;

    fn make_introspection() -> IntrospectionResponse {
        IntrospectionResponse {
            active: true,
            sub: Some("usr_test".into()),
            client_id: Some("gateway".into()),
            email: Some("test@example.com".into()),
            email_verified: Some(true),
            name: Some("Test".into()),
            scope: Some("openid".into()),
            exp: None,
        }
    }

    #[test]
    fn issue_then_verify_roundtrip() {
        // Happy path: sign a token, verify it with the matching
        // public key, confirm every claim the Issuer wrote survived
        // the JWT encode/decode round-trip intact.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, "https://api.zeroship.ai".into()).expect("issuer");
        let intro = make_introspection();
        let token = issuer
            .issue("myapp.zeroship.ai", &intro, "test-jkt", "hydra-token")
            .expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), "https://api.zeroship.ai".into());
        let claims = verifier.verify(&token, "myapp.zeroship.ai").expect("verify");

        assert_eq!(claims.sub, "usr_test");
        assert_eq!(claims.cnf.jkt, "test-jkt");
        assert_eq!(claims.aud, "myapp.zeroship.ai");
        assert_eq!(claims.client_id, "gateway");
    }

    #[test]
    fn verify_rejects_tampered_token() {
        // Bit-flip in the signature must surface as an error — the
        // Ed25519 verifier should refuse to validate a token whose
        // signature has been altered, even by a single character.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, "https://api.zeroship.ai".into()).unwrap();
        let intro = make_introspection();
        let token = issuer.issue("aud", &intro, "jkt", "ht").unwrap();

        // Flip a byte in the signature segment (the last segment).
        let mut chars: Vec<char> = token.chars().collect();
        let last_idx = chars.len() - 1;
        chars[last_idx] = if chars[last_idx] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();

        let verifier = Verifier::new(&signing.verifying_key(), "https://api.zeroship.ai".into());
        assert!(verifier.verify(&tampered, "aud").is_err());
    }

    #[test]
    fn verify_rejects_wrong_signer() {
        // A token signed by signing_a must not verify with the public
        // key of signing_b. Defends against an attacker who controls
        // a different gateway key trying to mint tokens for this
        // gateway's audience.
        let signing_a = SigningKey::from_bytes(&[7u8; 32]);
        let signing_b = SigningKey::from_bytes(&[8u8; 32]);
        let issuer = Issuer::new(&signing_a, "https://api.zeroship.ai".into()).unwrap();
        let intro = make_introspection();
        let token = issuer.issue("aud", &intro, "jkt", "ht").unwrap();

        // The verifier built from signing_b's pub key will see a
        // mismatched `kid` (since `kid` is a thumbprint of the
        // PUBLIC half) — that surfaces before the signature check.
        // Either way, the result is an error.
        let verifier =
            Verifier::new(&signing_b.verifying_key(), "https://api.zeroship.ai".into());
        assert!(verifier.verify(&token, "aud").is_err());
    }

    #[test]
    fn verify_rejects_wrong_issuer() {
        // `iss` mismatch must be rejected — a verifier configured
        // for `https://other.zeroship.ai` should refuse tokens minted
        // with `iss: https://api.zeroship.ai`.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, "https://api.zeroship.ai".into()).unwrap();
        let intro = make_introspection();
        let token = issuer.issue("aud", &intro, "jkt", "ht").unwrap();

        let verifier = Verifier::new(
            &signing.verifying_key(),
            "https://other.zeroship.ai".into(),
        );
        assert!(verifier.verify(&token, "aud").is_err());
    }

    #[test]
    fn verify_rejects_expired_token() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;

        // Build a wrapper claim set with `exp` 100s in the past —
        // beyond jsonwebtoken's default 60s leeway — and sign it
        // manually. The verifier must reject it.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let kid = crate::signing::jwk_thumbprint(&signing);

        let now = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let claims = WrapperClaims {
            iss: "https://api.zeroship.ai".into(),
            aud: "aud".into(),
            sub: "usr".into(),
            exp: now - 100, // expired (beyond the 60s default leeway)
            iat: now - 200,
            jti: "j".into(),
            cnf: Cnf { jkt: "k".into() },
            scope: String::new(),
            client_id: "c".into(),
            email: None,
            email_verified: None,
            name: None,
            wraps: "w".into(),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(kid);

        let der = signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let token = encode(&header, &claims, &key).unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), "https://api.zeroship.ai".into());
        assert!(verifier.verify(&token, "aud").is_err());
    }

    #[test]
    fn verify_rejects_wrapper_with_wrong_aud() {
        // Audience is the request Host. A wrapper minted for one app
        // host must not verify for another app host, even when the
        // signature, issuer and DPoP binding are otherwise valid.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, "https://api.zeroship.ai".into()).unwrap();
        let intro = make_introspection();
        let token = issuer
            .issue("app-a.zeroship.ai", &intro, "jkt", "ht")
            .unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), "https://api.zeroship.ai".into());
        assert!(verifier.verify(&token, "app-b.zeroship.ai").is_err());
    }

    #[test]
    fn cnf_jkt_round_trips_through_encode_decode() {
        // The whole point of the wrapper token is to carry `cnf.jkt`
        // so downstream verifiers can match the wrapper to the DPoP
        // proof. Verify that the JKT survives sign+verify unchanged
        // — a serde-rename or default-value regression in
        // `WrapperClaims` would silently break DPoP binding.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, "https://api.zeroship.ai".into()).unwrap();
        let intro = make_introspection();
        let token = issuer
            .issue("aud", &intro, "the-special-jkt-value", "ht")
            .unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), "https://api.zeroship.ai".into());
        let claims = verifier.verify(&token, "aud").expect("verify");
        assert_eq!(claims.cnf.jkt, "the-special-jkt-value");
    }
}
