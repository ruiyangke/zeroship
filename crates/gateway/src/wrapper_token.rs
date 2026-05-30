//! Gateway-issued wrapper access tokens.
//!
//! Per the Phase 8 plan, the gateway wraps every hydra-issued opaque
//! access token in its own short-lived signed `JWT` before forwarding
//! it to creator-app workers. Compared with passing the raw hydra
//! token through, the wrapper buys us three things:
//!
//! 1. **`DPoP` key binding.** The wrapper MAY carry an RFC 9449
//!    `cnf.jkt` confirmation claim — the `JWK` thumbprint of the
//!    `DPoP` key the client used at the gateway. Downstream verifiers
//!    (the dispatcher on the next hop, the worker on the inside)
//!    reject a wrapper token presented with a different `DPoP` proof
//!    key. The raw hydra introspection response has no equivalent
//!    binding. `cnf` is **optional**: the DPoP-exchange path sets it;
//!    the plain-Bearer browser path (no underlying `DPoP` key) omits it.
//! 2. **Audience scoping.** `aud` is the per-app host
//!    (`{app}.zeroship.ai`), so a wrapper minted for app A cannot be
//!    replayed against app B — even though hydra issued one access
//!    token covering both. The dispatcher (U4) is the audience
//!    enforcer.
//! 3. **Stable typed claims.** The worker receives a `JOSE` `JWT`
//!    with a fixed schema (`sub`, `scope`, `client_id`, `email`, …)
//!    instead of having to call `/oauth2/introspect` itself. The
//!    `wraps` claim (when present) ties the wrapper to the underlying
//!    hydra token (SHA-256 of the raw access-token string,
//!    base64url-encoded) so revocation pings can invalidate the wrapper
//!    by introspecting the original. The browser plain-Bearer path has
//!    no underlying raw token and omits `wraps`.
//!
//! The `sub` claim is an **opaque string**: the DPoP path stamps
//! whatever subject the introspection response carried (a global UUID
//! today); a future browser path stamps a per-app pairwise `pws_…`
//! projection. The wrapper machinery makes no UUID assumption — the
//! caller supplies the `sub` it wants.
//!
//! Tokens carry the RFC 9068 `typ: at+jwt` header so they cannot be
//! confused with hydra ID tokens (`typ: JWT`) on the verify side, and
//! are signed `EdDSA` (Ed25519) with the key loaded by
//! [`crate::signing::load_from_path`]. `kid` is the RFC 7638 `JWK`
//! thumbprint so JWKS rotation is automatic — no manual `kid`
//! bookkeeping.
//!
//! ## Signing-key rotation / overlap
//!
//! The wrapper is the primary browser-held access token, so its
//! ed25519 signing key needs a rotation story that doesn't sign every
//! live session out at once. The [`Issuer`] always signs with the
//! **current** key (stamping its `kid`). The [`Verifier`] holds an
//! ordered accept-list — `[current, previous]` — and accepts a wrapper
//! signed by **either**, selected by the JWT header `kid`. During an
//! overlap window (≥ wrapper TTL + clock skew) both keys verify; once
//! the overlap elapses, the previous key is dropped from the
//! accept-list. A wrapper with an unknown/absent `kid` is rejected.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{GatewayError, Result};

/// Wrapper-token claim set.
///
/// Shape is the `JOSE`/RFC 7519 superset of:
/// - the standard registered claims (`iss`/`aud`/`sub`/`exp`/`iat`/`jti`),
/// - the **optional** RFC 9449 `cnf.jkt` `DPoP` confirmation,
/// - hydra introspection projections
///   (`scope`/`client_id`/`email`/`name`/`email_verified`),
/// - and `wraps` — the SHA-256 of the underlying hydra access token,
///   so revocation can map a wrapper back to its origin token. Absent
///   on the plain-Bearer browser path (no underlying raw token).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperClaims {
    pub iss: String,
    pub aud: String,
    /// Opaque subject. No UUID assumption — the DPoP path stamps the
    /// introspected sub; a browser path stamps a `pws_…` projection.
    pub sub: String,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    /// RFC 9449 `DPoP` confirmation. `Some` for DPoP-bound wrappers,
    /// `None` for plain-Bearer wrappers that have no underlying key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cnf: Option<Cnf>,
    pub scope: String,
    /// The per-app OAuth `client_id`. The Bearer arm binds on this
    /// (`claims.client_id == route.oauth_client_id`).
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
    /// origin token. `None` when there is no underlying raw token (the
    /// plain-Bearer browser path, whose claims come from a validated
    /// id_token + route, not an introspected opaque token).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wraps: Option<String>,
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

/// Free-parameter mint request for [`Issuer::issue`].
///
/// Every projected claim is an explicit parameter rather than read
/// from an introspection response, so the same primitive serves the
/// DPoP-exchange path (UUID `sub`, `cnf = Some`, `wraps = Some`, 3600 s
/// `exp`) and the plain-Bearer browser path (`pws_` `sub`, `cnf =
/// None`, `wraps = None`, 600 s `exp`).
#[derive(Debug, Clone)]
pub struct WrapperMint<'a> {
    /// Request `Host` the wrapper is destined for (`{app}.zeroship.ai`).
    pub aud: &'a str,
    /// Opaque subject. A global UUID on the DPoP path; a per-app
    /// `pws_…` projection on the browser path. Stamped verbatim.
    pub sub: &'a str,
    /// Granted scope string.
    pub scope: &'a str,
    /// Per-app OAuth `client_id` (the Bearer arm binds on this).
    pub client_id: &'a str,
    /// Wrapper lifetime in seconds. 3600 on the DPoP path; 600 on the
    /// browser path.
    pub exp_secs: i64,
    /// `Some(jkt)` for a DPoP-bound wrapper; `None` for plain Bearer.
    pub cnf: Option<&'a str>,
    /// `Some(base64url(SHA256(hydra_token)))` when a raw token
    /// underlies the wrapper; `None` otherwise.
    pub wraps: Option<&'a str>,
    pub email: Option<&'a str>,
    pub email_verified: Option<bool>,
    pub name: Option<&'a str>,
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

    /// Issue a wrapper token from an explicit [`WrapperMint`].
    ///
    /// Every projected claim (`sub`/`scope`/`client_id`/`email`/…) is a
    /// free parameter so the same primitive serves the DPoP-exchange
    /// and plain-Bearer browser paths. The Issuer adds the standard
    /// registered claims (`iss`/`iat`/`exp`/`jti`) and signs with the
    /// current key, stamping its `kid` and the RFC 9068 `typ: at+jwt`.
    ///
    /// `m.sub` MUST be non-empty (an empty subject is a programming
    /// error in every caller); it is stamped verbatim and treated as
    /// opaque (no UUID parsing).
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] on an empty `sub`, JWT encoding
    /// failure, or system-clock failure. The clock/encode failures are
    /// extraordinarily unlikely in normal operation but are propagated
    /// rather than panicked on so the gateway dispatch path can surface
    /// a 500.
    pub fn issue(&self, m: &WrapperMint<'_>) -> Result<String> {
        if m.sub.is_empty() {
            return Err(GatewayError::Internal("missing wrapper sub".into()));
        }

        // `as_secs()` returns u64. We need i64 for the JWT claim
        // (RFC 7519 numeric date is signed). The conversion is
        // lossless until year 292 277 026 596 — well past the heat
        // death of the sun — so a `try_from + map_err` is the
        // clippy-clean way to assert it.
        let now: i64 = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| GatewayError::Internal(format!("clock: {e}")))?
                .as_secs(),
        )
        .map_err(|e| GatewayError::Internal(format!("clock overflow: {e}")))?;

        let claims = WrapperClaims {
            iss: self.iss.clone(),
            aud: m.aud.to_string(),
            sub: m.sub.to_string(),
            exp: now + m.exp_secs,
            iat: now,
            jti: uuid::Uuid::new_v4().to_string(),
            cnf: m.cnf.map(|jkt| Cnf {
                jkt: jkt.to_string(),
            }),
            scope: m.scope.to_string(),
            client_id: m.client_id.to_string(),
            email: m.email.map(str::to_string),
            email_verified: m.email_verified,
            name: m.name.map(str::to_string),
            wraps: m.wraps.map(str::to_string),
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

/// A single verification key in the [`Verifier`]'s accept-list: the
/// `kid` it answers to and the `DecodingKey` that verifies wrappers
/// stamped with that `kid`.
struct VerifyKey {
    kid: String,
    decoding_key: DecodingKey,
}

/// Wrapper-token verifier.
///
/// Holds an ordered accept-list of `[current, previous]` ed25519 keys
/// (the rotation-overlap story) plus the expected `iss` value. A
/// wrapper is verified against the accept-list entry whose `kid`
/// matches the JWT header `kid`; an unknown or absent `kid` is
/// rejected. The Issuer only ever signs with the current key.
pub struct Verifier {
    /// Ordered accept-list. Element 0 is the current key; element 1
    /// (when present) is the previous key, retained for the rotation
    /// overlap window. Lookup is by `kid`, so order is informational
    /// rather than load-bearing for correctness.
    keys: Vec<VerifyKey>,
    /// Expected `iss` claim. Pinned in [`Validation::set_issuer`].
    expected_iss: String,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `decoding_key` is omitted because `DecodingKey` doesn't
        // implement Debug — the public key bytes are not secret but
        // there's no useful representation to print here. We surface
        // the kids so the accept-list is visible in logs.
        let kids: Vec<&str> = self.keys.iter().map(|k| k.kid.as_str()).collect();
        f.debug_struct("Verifier")
            .field("kids", &kids)
            .field("expected_iss", &self.expected_iss)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// Build a single-key Verifier (current key only) from a
    /// [`ed25519_dalek::VerifyingKey`].
    ///
    /// `issuer_url` MUST match the `iss` claim the matching
    /// [`Issuer`] writes (i.e. the gateway's public URL). Use
    /// [`Verifier::with_previous`] to add a previous key for the
    /// rotation overlap.
    #[must_use]
    pub fn new(public_key: &ed25519_dalek::VerifyingKey, issuer_url: String) -> Self {
        Self {
            keys: vec![Self::make_key(public_key)],
            expected_iss: issuer_url,
        }
    }

    /// Build a Verifier with an explicit `[current, previous]`
    /// accept-list for the rotation overlap window. During the overlap
    /// a wrapper signed by either key verifies, matched by `kid`.
    ///
    /// The overlap *duration* is an OPERATIONAL (runbook) invariant, not
    /// enforced here: a [`VerifyKey`] carries no validity window, so the
    /// previous key is accepted until an operator drops it. It MUST stay in
    /// the accept-list for at least the wrapper TTL + clock skew — otherwise
    /// a wrapper minted just before the roll (`exp = mint + TTL`) is orphaned
    /// (`unknown kid`) mid-life, the exact failure rotation overlap exists to
    /// prevent. See the rotation runbook in the design doc.
    #[must_use]
    pub fn with_previous(
        current: &ed25519_dalek::VerifyingKey,
        previous: &ed25519_dalek::VerifyingKey,
        issuer_url: String,
    ) -> Self {
        Self {
            keys: vec![Self::make_key(current), Self::make_key(previous)],
            expected_iss: issuer_url,
        }
    }

    fn make_key(public_key: &ed25519_dalek::VerifyingKey) -> VerifyKey {
        let kid = crate::signing::jwk_thumbprint_public(public_key);
        // ring (jsonwebtoken's EdDSA backend) verifies Ed25519 via
        // `signature::UnparsedPublicKey::new(&ED25519, raw_32_bytes)`.
        // The `from_ed_der` constructor stores the bytes verbatim,
        // so the raw 32-byte public key is the right input.
        let decoding_key = DecodingKey::from_ed_der(public_key.as_bytes());
        VerifyKey { kid, decoding_key }
    }

    /// Verify a wrapper token's signature + iss + aud + exp + kid + typ,
    /// and (when `expected_client_id` is `Some`) bind the
    /// `client_id` claim. Returns the decoded claims on success.
    ///
    /// `expected_aud` is the request `Host` value. It is per-request
    /// rather than stored on the verifier because one gateway verifier
    /// serves every app host.
    ///
    /// `expected_client_id` is the per-app OAuth `client_id` resolved
    /// from the route. When `Some`, the wrapper's `client_id` claim
    /// MUST equal it (the Bearer arm's per-app binding). When `None`,
    /// the `client_id` claim is not checked — the DPoP dispatcher
    /// passes `None` because it already enforces `aud == Host`.
    ///
    /// The header `kid` selects which accept-list key verifies the
    /// signature; an unknown or absent `kid` is rejected before the
    /// signature check.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] for any verification failure:
    /// missing/unknown `kid`, wrong `typ`, expired token, wrong issuer,
    /// `client_id` mismatch, or invalid signature. Callers translate
    /// this into a `401 invalid_token` response.
    pub fn verify(
        &self,
        token: &str,
        expected_aud: &str,
        expected_client_id: Option<&str>,
    ) -> Result<WrapperClaims> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.expected_iss]);
        validation.set_audience(&[expected_aud]);
        // `validate_exp` is on by default with a 60s leeway, which is
        // what we want.

        // Pre-check the header so a mismatch surfaces as "unknown
        // kid" / "unexpected typ" rather than a generic
        // signature-failure error from `decode`. Cheap — just parses
        // the base64url-encoded JOSE header without touching the
        // signature. The `kid` also selects the accept-list key.
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| GatewayError::Internal(format!("decode header: {e}")))?;
        let Some(kid) = header.kid.as_deref() else {
            return Err(GatewayError::Internal("missing kid".into()));
        };
        let Some(key) = self.keys.iter().find(|k| k.kid == kid) else {
            return Err(GatewayError::Internal(format!("unknown kid: {kid}")));
        };
        if header.typ.as_deref() != Some("at+jwt") {
            return Err(GatewayError::Internal(format!(
                "unexpected typ: {:?}",
                header.typ
            )));
        }

        let data: jsonwebtoken::TokenData<WrapperClaims> =
            decode(token, &key.decoding_key, &validation)
                .map_err(|e| GatewayError::Internal(format!("jwt verify: {e}")))?;

        // Per-app binding: the wrapper's client_id claim MUST match the
        // route's oauth_client_id. Checked after signature so an
        // attacker can't probe valid client_ids with a forged token.
        if let Some(expected) = expected_client_id {
            if data.claims.client_id != expected {
                return Err(GatewayError::Internal(format!(
                    "client_id mismatch: token {:?} != expected {expected:?}",
                    data.claims.client_id
                )));
            }
        }

        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    const ISS: &str = "https://api.zeroship.ai";

    /// A DPoP-style mint: opaque UUID-ish sub, cnf + wraps present,
    /// 3600 s lifetime.
    fn dpop_mint<'a>(aud: &'a str, sub: &'a str, jkt: &'a str) -> WrapperMint<'a> {
        WrapperMint {
            aud,
            sub,
            scope: "openid",
            client_id: "gateway",
            exp_secs: 3600,
            cnf: Some(jkt),
            wraps: Some("d2hhdGV2ZXI"),
            email: Some("test@example.com"),
            email_verified: Some(true),
            name: Some("Test"),
        }
    }

    #[test]
    fn issue_then_verify_roundtrip() {
        // Happy path: sign with the current key, verify it with the
        // matching public key, confirm every claim the Issuer wrote
        // survived the JWT encode/decode round-trip intact.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let token = issuer
            .issue(&dpop_mint("myapp.zeroship.ai", "usr_test", "test-jkt"))
            .expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let claims = verifier
            .verify(&token, "myapp.zeroship.ai", None)
            .expect("verify");

        assert_eq!(claims.sub, "usr_test");
        assert_eq!(claims.cnf.as_ref().expect("cnf present").jkt, "test-jkt");
        assert_eq!(claims.aud, "myapp.zeroship.ai");
        assert_eq!(claims.client_id, "gateway");
        assert_eq!(claims.wraps.as_deref(), Some("d2hhdGV2ZXI"));
    }

    #[test]
    fn issue_rejects_empty_sub() {
        // An empty subject is a programming error in every caller —
        // the Issuer refuses rather than minting a subject-less token.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let mint = dpop_mint("myapp.zeroship.ai", "", "test-jkt");
        let err = issuer.issue(&mint).expect_err("empty sub must not issue");
        assert_eq!(err.to_string(), "internal: missing wrapper sub");
    }

    #[test]
    fn plain_bearer_wrapper_roundtrips_without_cnf_or_wraps() {
        // The browser plain-Bearer path mints a 600 s wrapper with an
        // opaque pairwise `pws_` sub, no DPoP confirmation, and no
        // underlying raw token. `cnf` and `wraps` must serialize away
        // entirely (not as JSON `null`) and round-trip as `None`.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let mint = WrapperMint {
            aud: "myapp.zeroship.ai",
            sub: "pws_2x9q4abc",
            scope: "openid email",
            client_id: "oac_myapp",
            exp_secs: 600,
            cnf: None,
            wraps: None,
            email: Some("relay-alias@zeroship.ai"),
            email_verified: Some(true),
            name: None,
        };
        let token = issuer.issue(&mint).expect("issue");

        // The serialized JSON body must omit cnf/wraps, not null them.
        use base64::Engine as _;
        let body_b64 = token.split('.').nth(1).expect("body segment");
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body_b64)
            .expect("decode body");
        let body_str = String::from_utf8(body).expect("utf8");
        assert!(
            !body_str.contains("cnf"),
            "plain wrapper must omit cnf: {body_str}"
        );
        assert!(
            !body_str.contains("wraps"),
            "plain wrapper must omit wraps: {body_str}"
        );

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let claims = verifier
            .verify(&token, "myapp.zeroship.ai", Some("oac_myapp"))
            .expect("verify");
        assert_eq!(claims.sub, "pws_2x9q4abc");
        assert!(claims.cnf.is_none(), "cnf round-trips as None");
        assert!(claims.wraps.is_none(), "wraps round-trips as None");
    }

    #[test]
    fn opaque_non_uuid_sub_round_trips() {
        // The wrapper makes no UUID assumption about `sub`. A `pws_…`
        // value (or any opaque string) must survive sign+verify intact.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let mut mint = dpop_mint("myapp.zeroship.ai", "pws_not-a-uuid_##", "jkt");
        mint.cnf = None;
        mint.wraps = None;
        let token = issuer.issue(&mint).expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let claims = verifier
            .verify(&token, "myapp.zeroship.ai", None)
            .expect("verify");
        assert_eq!(claims.sub, "pws_not-a-uuid_##");
    }

    #[test]
    fn verify_binds_client_id_match() {
        // `Some(client_id)` that matches the claim accepts.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let mut mint = dpop_mint("aud", "usr", "jkt");
        mint.client_id = "oac_app";
        let token = issuer.issue(&mint).expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let claims = verifier
            .verify(&token, "aud", Some("oac_app"))
            .expect("matching client_id verifies");
        assert_eq!(claims.client_id, "oac_app");
    }

    #[test]
    fn verify_rejects_client_id_mismatch() {
        // `Some(wrong_client_id)` rejects even when sig/iss/aud/exp are
        // all valid — the per-app binding the Bearer arm relies on.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let mut mint = dpop_mint("aud", "usr", "jkt");
        mint.client_id = "oac_app_a";
        let token = issuer.issue(&mint).expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let err = verifier
            .verify(&token, "aud", Some("oac_app_b"))
            .expect_err("wrong client_id must reject");
        assert!(
            err.to_string().contains("client_id mismatch"),
            "got: {err}"
        );
    }

    #[test]
    fn verify_with_none_client_id_skips_binding() {
        // The DPoP dispatcher passes `None`; the client_id claim is not
        // checked (it already enforces aud == Host).
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let mut mint = dpop_mint("aud", "usr", "jkt");
        mint.client_id = "anything";
        let token = issuer.issue(&mint).expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        verifier
            .verify(&token, "aud", None)
            .expect("None client_id verifies regardless of claim");
    }

    #[test]
    fn verify_accepts_token_signed_by_previous_key_during_overlap() {
        // Rotation overlap: a wrapper signed by the PREVIOUS key (A)
        // must still verify while A is in the accept-list, and a
        // freshly minted wrapper is signed by the CURRENT key (B).
        let key_a = SigningKey::from_bytes(&[7u8; 32]); // previous
        let key_b = SigningKey::from_bytes(&[8u8; 32]); // current

        // Wrapper minted under A (before rotation).
        let issuer_a = Issuer::new(&key_a, ISS.into()).expect("issuer a");
        let a_token = issuer_a
            .issue(&dpop_mint("aud", "usr", "jkt"))
            .expect("issue a");

        // Wrapper minted under B (after rotation).
        let issuer_b = Issuer::new(&key_b, ISS.into()).expect("issuer b");
        let b_token = issuer_b
            .issue(&dpop_mint("aud", "usr", "jkt"))
            .expect("issue b");

        // Verifier in overlap: current = B, previous = A.
        let verifier = Verifier::with_previous(
            &key_b.verifying_key(),
            &key_a.verifying_key(),
            ISS.into(),
        );

        // Both verify during the overlap window.
        verifier
            .verify(&a_token, "aud", None)
            .expect("previous-key wrapper still verifies during overlap");
        verifier
            .verify(&b_token, "aud", None)
            .expect("current-key wrapper verifies");

        // The A token's header kid is A's thumbprint (it was signed by
        // A); the B token's is B's — distinct keys, distinct kids.
        let a_kid = jsonwebtoken::decode_header(&a_token).unwrap().kid;
        let b_kid = jsonwebtoken::decode_header(&b_token).unwrap().kid;
        assert_ne!(a_kid, b_kid);
    }

    #[test]
    fn verify_rejects_token_after_previous_key_dropped() {
        // After the overlap window the previous key is dropped from the
        // accept-list. A wrapper still bearing the old (now-unknown)
        // kid is rejected as `unknown kid`.
        let key_a = SigningKey::from_bytes(&[7u8; 32]); // old/previous
        let key_b = SigningKey::from_bytes(&[8u8; 32]); // current

        let issuer_a = Issuer::new(&key_a, ISS.into()).expect("issuer a");
        let a_token = issuer_a
            .issue(&dpop_mint("aud", "usr", "jkt"))
            .expect("issue a");

        // Verifier now holds ONLY B (overlap elapsed, A dropped).
        let verifier = Verifier::new(&key_b.verifying_key(), ISS.into());
        let err = verifier
            .verify(&a_token, "aud", None)
            .expect_err("A-signed wrapper rejected after A dropped");
        assert!(err.to_string().contains("unknown kid"), "got: {err}");
    }

    #[test]
    fn verify_rejects_tampered_token() {
        // Bit-flip in the signature must surface as an error — the
        // Ed25519 verifier should refuse to validate a token whose
        // signature has been altered, even by a single character.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let token = issuer.issue(&dpop_mint("aud", "usr", "jkt")).unwrap();

        // Flip a byte in the signature segment (the last segment).
        let mut chars: Vec<char> = token.chars().collect();
        let last_idx = chars.len() - 1;
        chars[last_idx] = if chars[last_idx] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        assert!(verifier.verify(&tampered, "aud", None).is_err());
    }

    #[test]
    fn verify_rejects_wrong_signer() {
        // A token signed by signing_a must not verify with a verifier
        // that holds only signing_b's public key. Defends against an
        // attacker who controls a different gateway key trying to mint
        // tokens for this gateway's audience.
        let signing_a = SigningKey::from_bytes(&[7u8; 32]);
        let signing_b = SigningKey::from_bytes(&[8u8; 32]);
        let issuer = Issuer::new(&signing_a, ISS.into()).unwrap();
        let token = issuer.issue(&dpop_mint("aud", "usr", "jkt")).unwrap();

        // The verifier built from signing_b's pub key will see a
        // mismatched `kid` (since `kid` is a thumbprint of the
        // PUBLIC half) — that surfaces as `unknown kid` before the
        // signature check.
        let verifier = Verifier::new(&signing_b.verifying_key(), ISS.into());
        assert!(verifier.verify(&token, "aud", None).is_err());
    }

    #[test]
    fn verify_rejects_wrong_issuer() {
        // `iss` mismatch must be rejected — a verifier configured
        // for `https://other.zeroship.ai` should refuse tokens minted
        // with `iss: https://api.zeroship.ai`.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let token = issuer.issue(&dpop_mint("aud", "usr", "jkt")).unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), "https://other.zeroship.ai".into());
        assert!(verifier.verify(&token, "aud", None).is_err());
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
            iss: ISS.into(),
            aud: "aud".into(),
            sub: "usr".into(),
            exp: now - 100, // expired (beyond the 60s default leeway)
            iat: now - 200,
            jti: "j".into(),
            cnf: Some(Cnf { jkt: "k".into() }),
            scope: String::new(),
            client_id: "c".into(),
            email: None,
            email_verified: None,
            name: None,
            wraps: Some("w".into()),
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into());
        header.kid = Some(kid);

        let der = signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let token = encode(&header, &claims, &key).unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        assert!(verifier.verify(&token, "aud", None).is_err());
    }

    #[test]
    fn verify_rejects_wrapper_with_wrong_aud() {
        // Audience is the request Host. A wrapper minted for one app
        // host must not verify for another app host, even when the
        // signature, issuer and DPoP binding are otherwise valid.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let token = issuer
            .issue(&dpop_mint("app-a.zeroship.ai", "usr", "jkt"))
            .unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        assert!(verifier
            .verify(&token, "app-b.zeroship.ai", None)
            .is_err());
    }

    #[test]
    fn cnf_jkt_round_trips_through_encode_decode() {
        // The whole point of the DPoP wrapper is to carry `cnf.jkt`
        // so downstream verifiers can match the wrapper to the DPoP
        // proof. Verify that the JKT survives sign+verify unchanged
        // — a serde-rename or default-value regression in
        // `WrapperClaims` would silently break DPoP binding.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let token = issuer
            .issue(&dpop_mint("aud", "usr", "the-special-jkt-value"))
            .unwrap();

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let claims = verifier.verify(&token, "aud", None).expect("verify");
        assert_eq!(
            claims.cnf.as_ref().expect("cnf present").jkt,
            "the-special-jkt-value"
        );
    }
}
