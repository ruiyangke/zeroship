//! Gateway-signed **stateless session cookie** (`__Host-zeroship_app_session`).
//!
//! BFF redesign **slice R1b** (`2026-05-30-auth-bff-session-redesign.md`,
//! "Decision addendum: signed STATELESS session cookie"). The session cookie is
//! no longer an opaque `gateway_sessions.id` looked up per request — it is a
//! gateway-SIGNED, `HttpOnly`, short-lived (~15 min) **identity assertion**,
//! verified LOCALLY on every request (no per-request DB/Redis read).
//!
//! ## A dedicated token type, stamped `typ: zeroship-sess+jwt`
//!
//! The cookie is signed with the gateway's ed25519 signing key + the
//! current/previous `kid` rotation/overlap, and is stamped `typ: zeroship-sess+jwt`.
//! That typ tag is load-bearing: this [`Verifier`] hard-rejects any token whose
//! typ is not `zeroship-sess+jwt` (e.g. an RFC 9068 `at+jwt` access token), so a
//! resource-server access token can never be replayed as a session cookie, and
//! the cookie — which is inert identity, never a capability — can never be
//! presented as authorization on the Bearer arm (which recognizes only a
//! raw OP `iss`, not this token).
//!
//! ## What the cookie carries — identity + scopes only, NOT a capability
//!
//! The claims are the full [`WorkerUser`](crate::oidc_rp::WorkerUser) projection
//! the worker needs (`sub = pws_…`, relay-alias `email`, `name`, `avatar`,
//! `email_verified`, `scopes`) plus the route app binding (`app` = the per-app
//! `client_id`), `iss`, `iat`, `exp` (~15 min), `auth_time`, and `amr`. It is
//! identity + scopes — **not** a power token, **not** JS-readable (`HttpOnly`).
//! No resource server accepts it as authorization; the gateway emits the signed
//! `ZeroShip-User` header directly from these claims on the cookie arm.
//!
//! ## Revocation: per-app family marker (one DB read, NOT cached)
//!
//! Identity verification is stateless (local signature + `kid` + `iss` + `exp`
//! + `app` binding — no DB). The revocation gate is NOT: the cookie arm runs the
//! SAME per-app family-marker check the Bearer arm uses —
//! `is_family_revoked_since(client_id = app, sub = pws_, iat)`
//! ([`zeroship_core::wrapper_revocation`]) — and that is a direct
//! `SELECT EXISTS` against a pooled connection, with NO in-memory TTL cache in
//! front of it. `iat` is the binding instant, so a revoked `(client_id, pws_)`
//! family rejects a still-valid signed cookie WITHOUT re-reading any session
//! store — but it does cost one revocation DB round-trip per request when a DB
//! is configured. With `db = None` (smoke mode) the gate is skipped and the
//! whole path is DB-free.

use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{GatewayError, Result};

/// RFC-8725-style typ tag for the signed session cookie. Distinct from the
/// wrapper's `at+jwt` so the two verifiers never accept each other's tokens.
pub const SESSION_TOKEN_TYP: &str = "zeroship-sess+jwt";

/// Short hot-path lifetime of the signed session cookie (~15 min).
///
/// The durable credential is the 30-day server-held anchor; this short cookie is
/// silently re-signed from the anchor via `GET /__zeroship/auth/session` when it
/// lapses.
pub const SESSION_TOKEN_TTL_SECS: i64 = 15 * 60;

/// Signed-session-cookie claim set (`typ: zeroship-sess+jwt`).
///
/// Carries the full [`crate::oidc_rp::WorkerUser`] projection (so the cookie arm
/// emits `ZeroShip-User` directly) plus the route app binding + freshness/audit
/// claims. NO `cnf`, NO `wraps`, NO power-token lineage — it is inert identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionClaims {
    pub iss: String,
    /// The route app binding — the per-app OAuth `client_id` (`oac_…`). The
    /// cookie arm rejects a cookie whose `app` does not match the resolved
    /// route's client (audience binding, same role as the wrapper `aud`/Host).
    pub app: String,
    /// Per-app pairwise subject (`pws_…`). NEVER the global OP UUID.
    pub sub: String,
    pub iat: i64,
    pub exp: i64,
    /// The OIDC `auth_time` (authenticating-event instant), for step-up
    /// freshness. `None` when the originating `id_token` omitted it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_time: Option<i64>,
    /// The OIDC `amr` (authentication methods, e.g. `["pwd"]`).
    #[serde(default)]
    pub amr: Vec<String>,
    /// Relay-alias email (or empty string — fail closed). NEVER the real inbox.
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub email_verified: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Granted OAuth scopes for this app + user (the same set the worker reads
    /// via `env.auth.getUser().scopes`). Empty when none were granted.
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Free-parameter mint request for [`Issuer::issue`]. Every projected field is
/// explicit so the `/session` POST (exchange) and `?mint=1` (re-sign) paths
/// share one primitive.
#[derive(Debug, Clone)]
pub struct SessionMint<'a> {
    /// Route app binding — the per-app `oac_…` `client_id`.
    pub app: &'a str,
    /// Per-app pairwise `pws_…` subject (the same projection the wrapper arms
    /// stamp).
    pub sub: &'a str,
    pub auth_time: Option<i64>,
    pub amr: &'a [String],
    pub email: &'a str,
    pub email_verified: bool,
    pub name: &'a str,
    pub avatar: Option<&'a str>,
    pub scopes: &'a [String],
}

/// Signed-session-cookie issuer. Caches the PKCS#8 DER form of the ed25519 key,
/// stamps `kid`, signs `EdDSA`.
pub struct Issuer {
    private_der: Vec<u8>,
    kid: String,
    iss: String,
}

impl std::fmt::Debug for Issuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("session_token::Issuer")
            .field("kid", &self.kid)
            .field("iss", &self.iss)
            .finish_non_exhaustive()
    }
}

impl Issuer {
    /// Build an Issuer from the gateway ed25519 `SigningKey` (the SAME key the
    /// wrapper Issuer uses). `issuer_url` is the gateway's public URL.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] if PKCS#8 DER encoding fails.
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

    /// Issue a signed session cookie from an explicit [`SessionMint`]. The
    /// Issuer adds the registered `iss`/`iat`/`exp` claims, the
    /// [`SESSION_TOKEN_TTL_SECS`] lifetime, and signs with the CURRENT key,
    /// stamping its `kid` + the `zeroship-sess+jwt` typ.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] on an empty `sub`, JWT encode failure, or
    /// system-clock failure.
    pub fn issue(&self, m: &SessionMint<'_>) -> Result<String> {
        if m.sub.is_empty() {
            return Err(GatewayError::Internal("missing session sub".into()));
        }
        let now: i64 = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|e| GatewayError::Internal(format!("clock: {e}")))?
                .as_secs(),
        )
        .map_err(|e| GatewayError::Internal(format!("clock overflow: {e}")))?;

        let claims = SessionClaims {
            iss: self.iss.clone(),
            app: m.app.to_string(),
            sub: m.sub.to_string(),
            iat: now,
            exp: now + SESSION_TOKEN_TTL_SECS,
            auth_time: m.auth_time,
            amr: m.amr.to_vec(),
            email: m.email.to_string(),
            email_verified: m.email_verified,
            name: m.name.to_string(),
            avatar: m.avatar.map(str::to_string),
            scopes: m.scopes.to_vec(),
        };

        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(SESSION_TOKEN_TYP.into());
        header.kid = Some(self.kid.clone());

        let key = EncodingKey::from_ed_der(&self.private_der);
        encode(&header, &claims, &key)
            .map_err(|e| GatewayError::Internal(format!("jwt encode: {e}")))
    }

    /// The signing key's `kid` (RFC 7638 thumbprint), so the JWKS endpoint can
    /// publish a matching JWK if needed. Same key/thumbprint as the wrapper.
    #[must_use]
    #[allow(dead_code)]
    pub fn kid(&self) -> &str {
        &self.kid
    }
}

/// One verification key in the [`Verifier`]'s accept-list.
struct VerifyKey {
    kid: String,
    decoding_key: DecodingKey,
}

/// Signed-session-cookie verifier.
///
/// Holds an ordered `[current, previous]` ed25519 accept-list (the
/// rotation-overlap story, identical to the wrapper verifier) plus the expected
/// `iss`. Selects the key by JWT header `kid`; an unknown/absent `kid` is
/// rejected. Enforces the `zeroship-sess+jwt` typ gate.
pub struct Verifier {
    keys: Vec<VerifyKey>,
    expected_iss: String,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kids: Vec<&str> = self.keys.iter().map(|k| k.kid.as_str()).collect();
        f.debug_struct("session_token::Verifier")
            .field("kids", &kids)
            .field("expected_iss", &self.expected_iss)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// Build a single-key Verifier (current key only).
    #[must_use]
    pub fn new(public_key: &ed25519_dalek::VerifyingKey, issuer_url: String) -> Self {
        Self {
            keys: vec![Self::make_key(public_key)],
            expected_iss: issuer_url,
        }
    }

    /// Build a Verifier with an explicit `[current, previous]` accept-list for
    /// the rotation overlap window. A cookie signed just before a key roll
    /// (still within its ~15 min TTL) still verifies under the previous key.
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
        let decoding_key = DecodingKey::from_ed_der(public_key.as_bytes());
        VerifyKey { kid, decoding_key }
    }

    /// Verify a signed session cookie's signature + `iss` + `exp` + `kid` + the
    /// `zeroship-sess+jwt` typ, and bind `app` to `expected_app` (the resolved
    /// route's per-app `client_id`). Returns the decoded claims on success.
    ///
    /// There is NO `aud` claim on a session cookie (the audience binding is the
    /// `app == client_id` check below — the `__Host-` cookie is already
    /// host-scoped by the browser), so `aud` validation is disabled.
    ///
    /// # Errors
    ///
    /// [`GatewayError::Internal`] for any verification failure: missing/unknown
    /// `kid`, wrong `typ`, expired token, wrong issuer, `app` mismatch, or
    /// invalid signature. Callers translate this into "no valid session".
    pub fn verify(&self, token: &str, expected_app: &str) -> Result<SessionClaims> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.expected_iss]);
        // No `aud` claim on a session cookie — the per-app binding is `app`.
        validation.validate_aud = false;
        // `validate_exp` is on by default with a 60s leeway.

        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| GatewayError::Internal(format!("decode header: {e}")))?;
        let Some(kid) = header.kid.as_deref() else {
            return Err(GatewayError::Internal("missing kid".into()));
        };
        let Some(key) = self.keys.iter().find(|k| k.kid == kid) else {
            return Err(GatewayError::Internal(format!("unknown kid: {kid}")));
        };
        if header.typ.as_deref() != Some(SESSION_TOKEN_TYP) {
            return Err(GatewayError::Internal(format!(
                "unexpected typ: {:?} (session cookie must be {SESSION_TOKEN_TYP})",
                header.typ
            )));
        }

        let data: jsonwebtoken::TokenData<SessionClaims> =
            decode(token, &key.decoding_key, &validation)
                .map_err(|e| GatewayError::Internal(format!("jwt verify: {e}")))?;

        // Per-app binding: the cookie's `app` claim MUST match the route's
        // client_id. Checked after signature so an attacker can't probe valid
        // client_ids with a forged token.
        if data.claims.app != expected_app {
            return Err(GatewayError::Internal(format!(
                "app mismatch: token {:?} != expected {expected_app:?}",
                data.claims.app
            )));
        }

        Ok(data.claims)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    const ISS: &str = "https://api.zeroship.ai";

    fn mint<'a>(app: &'a str, sub: &'a str, scopes: &'a [String]) -> SessionMint<'a> {
        SessionMint {
            app,
            sub,
            auth_time: Some(1_700_000_000),
            amr: &[],
            email: "relay-alias@zeroship.ai",
            email_verified: true,
            name: "Test User",
            avatar: None,
            scopes,
        }
    }

    #[test]
    fn issue_then_verify_roundtrip() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let scopes = vec!["openid".to_string(), "email".to_string()];
        let token = issuer
            .issue(&mint("oac_myapp", "pws_abc123", &scopes))
            .expect("issue");

        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let claims = verifier.verify(&token, "oac_myapp").expect("verify");
        assert_eq!(claims.sub, "pws_abc123");
        assert_eq!(claims.app, "oac_myapp");
        assert_eq!(claims.email, "relay-alias@zeroship.ai");
        assert_eq!(claims.scopes, scopes);
        assert_eq!(claims.auth_time, Some(1_700_000_000));
    }

    #[test]
    fn typ_is_zs_sess_jwt_not_at_jwt() {
        // The session cookie's typ header MUST be zeroship-sess+jwt (never at+jwt),
        // so a wrapper Verifier (which hard-checks at+jwt) rejects it.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let token = issuer.issue(&mint("oac_a", "pws_x", &[])).expect("issue");
        let header = jsonwebtoken::decode_header(&token).expect("header");
        assert_eq!(header.typ.as_deref(), Some(SESSION_TOKEN_TYP));
        assert_ne!(header.typ.as_deref(), Some("at+jwt"));
    }

    #[test]
    fn verify_rejects_app_mismatch() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).expect("issuer");
        let token = issuer.issue(&mint("oac_a", "pws_x", &[])).expect("issue");
        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let err = verifier
            .verify(&token, "oac_b")
            .expect_err("wrong app must reject");
        assert!(err.to_string().contains("app mismatch"), "got: {err}");
    }

    #[test]
    fn verify_rejects_tampered_token() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let token = issuer.issue(&mint("oac_a", "pws_x", &[])).unwrap();
        let mut chars: Vec<char> = token.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();
        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        assert!(verifier.verify(&tampered, "oac_a").is_err());
    }

    #[test]
    fn verify_rejects_wrong_signer() {
        let signing_a = SigningKey::from_bytes(&[7u8; 32]);
        let signing_b = SigningKey::from_bytes(&[8u8; 32]);
        let issuer = Issuer::new(&signing_a, ISS.into()).unwrap();
        let token = issuer.issue(&mint("oac_a", "pws_x", &[])).unwrap();
        let verifier = Verifier::new(&signing_b.verifying_key(), ISS.into());
        // Different public key ⇒ different kid ⇒ unknown kid before sig check.
        assert!(verifier.verify(&token, "oac_a").is_err());
    }

    #[test]
    fn verify_rejects_wrong_issuer() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let token = issuer.issue(&mint("oac_a", "pws_x", &[])).unwrap();
        let verifier = Verifier::new(&signing.verifying_key(), "https://other.zeroship.ai".into());
        assert!(verifier.verify(&token, "oac_a").is_err());
    }

    #[test]
    fn verify_rejects_expired_token() {
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let kid = crate::signing::jwk_thumbprint(&signing);
        let now = i64::try_from(
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        )
        .unwrap();
        let claims = SessionClaims {
            iss: ISS.into(),
            app: "oac_a".into(),
            sub: "pws_x".into(),
            iat: now - 2000,
            exp: now - 100, // expired beyond the 60s leeway
            auth_time: None,
            amr: vec![],
            email: String::new(),
            email_verified: false,
            name: String::new(),
            avatar: None,
            scopes: vec![],
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some(SESSION_TOKEN_TYP.into());
        header.kid = Some(kid);
        let der = signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let token = encode(&header, &claims, &key).unwrap();
        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        assert!(verifier.verify(&token, "oac_a").is_err());
    }

    #[test]
    fn verify_accepts_previous_key_during_overlap() {
        let key_a = SigningKey::from_bytes(&[7u8; 32]); // previous
        let key_b = SigningKey::from_bytes(&[8u8; 32]); // current
        let issuer_a = Issuer::new(&key_a, ISS.into()).unwrap();
        let a_token = issuer_a.issue(&mint("oac_a", "pws_x", &[])).unwrap();
        let issuer_b = Issuer::new(&key_b, ISS.into()).unwrap();
        let b_token = issuer_b.issue(&mint("oac_a", "pws_x", &[])).unwrap();
        let verifier =
            Verifier::with_previous(&key_b.verifying_key(), &key_a.verifying_key(), ISS.into());
        verifier
            .verify(&a_token, "oac_a")
            .expect("previous-key cookie still verifies during overlap");
        verifier
            .verify(&b_token, "oac_a")
            .expect("current-key cookie verifies");
    }

    #[test]
    fn verify_rejects_token_after_previous_key_dropped() {
        let key_a = SigningKey::from_bytes(&[7u8; 32]); // old
        let key_b = SigningKey::from_bytes(&[8u8; 32]); // current
        let issuer_a = Issuer::new(&key_a, ISS.into()).unwrap();
        let a_token = issuer_a.issue(&mint("oac_a", "pws_x", &[])).unwrap();
        let verifier = Verifier::new(&key_b.verifying_key(), ISS.into());
        let err = verifier
            .verify(&a_token, "oac_a")
            .expect_err("dropped-key cookie rejected");
        assert!(err.to_string().contains("unknown kid"), "got: {err}");
    }

    #[test]
    fn verify_rejects_a_wrapper_typ() {
        // A token stamped at+jwt (the wrapper typ) must be rejected by the
        // session-cookie verifier's zeroship-sess+jwt typ gate.
        use ed25519_dalek::pkcs8::EncodePrivateKey;
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let kid = crate::signing::jwk_thumbprint(&signing);
        let now = i64::try_from(
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs(),
        )
        .unwrap();
        let claims = SessionClaims {
            iss: ISS.into(),
            app: "oac_a".into(),
            sub: "pws_x".into(),
            iat: now,
            exp: now + 900,
            auth_time: None,
            amr: vec![],
            email: String::new(),
            email_verified: false,
            name: String::new(),
            avatar: None,
            scopes: vec![],
        };
        let mut header = Header::new(Algorithm::EdDSA);
        header.typ = Some("at+jwt".into()); // wrapper typ — must be rejected here
        header.kid = Some(kid);
        let der = signing.to_pkcs8_der().unwrap();
        let key = EncodingKey::from_ed_der(der.as_bytes());
        let token = encode(&header, &claims, &key).unwrap();
        let verifier = Verifier::new(&signing.verifying_key(), ISS.into());
        let err = verifier.verify(&token, "oac_a").expect_err("at+jwt rejected");
        assert!(err.to_string().contains("unexpected typ"), "got: {err}");
    }

    #[test]
    fn issue_rejects_empty_sub() {
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let issuer = Issuer::new(&signing, ISS.into()).unwrap();
        let err = issuer.issue(&mint("oac_a", "", &[])).expect_err("empty sub");
        assert_eq!(err.to_string(), "internal: missing session sub");
    }
}
