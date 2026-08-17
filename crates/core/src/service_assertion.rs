//! Self-signed JWT service assertions: the minter, the verifier, the jti store.
//!
//! This is the single shipped mechanism behind the [`IdentityVerifier`] seam in
//! [`crate::service_identity`]. A service holds an ed25519 keypair, mints a
//! short-lived assertion naming the callee it is about to call, and the callee
//! verifies it against a trust bundle keyed on the assertion's `iss`.
//!
//! # The profile, and why it is not bare RFC 7523
//!
//! Bare RFC 7523 lists `jti` as MAY, so an implementation can be "7523
//! compliant" and offer no replay protection whatsoever. OIDC Core section 9
//! makes `jti` REQUIRED and single use a MUST; skipping it is CVE-2020-15222
//! (Fosite, CVSS 8.1). This module follows the stricter profile, with the
//! Keycloak reference numbers: a 60-second maximum lifetime, 15 seconds of
//! clock-skew tolerance, and an atomic put-if-absent into a store shared by
//! every replica of the callee.
//!
//! Every check below is a hard rejection. There is no warn-and-continue arm and
//! no configuration that turns one off, because an optional check is an
//! untested check - the Keycloak `cache-embedded-mtls-enabled` lesson
//! (CVE-2024-10973), where an optional hardening flag silently did nothing for
//! two version lines.
//!
//! # What the verifier checks, in order
//!
//! 1. The expected audience is a well-formed issuer identifier. A caller that
//!    passes an endpoint URL is a programming error and is rejected before any
//!    parsing of the assertion.
//! 2. The `typ` header is exactly [`SERVICE_ASSERTION_TYP`].
//! 3. The signing key is resolved FROM the assertion's `iss`, then narrowed by
//!    `kid` - never against a flat pool of every key the callee knows.
//! 4. The signature verifies under a pinned algorithm.
//! 5. `aud` equals the expected audience, `iss` equals the selector, `sub`
//!    equals `iss`.
//! 6. `exp` is present, in the future, and the assertion's total lifetime is
//!    within the ceiling the CALLEE sets.
//! 7. `jti` is claimed atomically in the replay store, and the claim wins.
//!
//! Any failure returns [`AuthError::CredentialRejected`] and nothing else, so
//! the error is not an oracle telling a prober which check it tripped. The
//! reason is emitted at `debug` for operators.
//!
//! # What lives elsewhere
//!
//! The Postgres implementation of [`ReplayStore`] lives in `zeroship-authn`,
//! because it needs `compio-postgres` and this crate is a leaf that everything
//! links. [`InMemoryReplayStore`] here is correct for a single-replica
//! deployment and for tests, and is NOT sufficient for a replicated callee -
//! "single use" would degrade to "single use per replica".

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use parking_lot::Mutex;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::service_identity::{
    AuthError, IdentityVerifier, MechanismTag, PresentedCredentials, ServiceIdentity, ServiceName,
    ServicePrincipal, TrustDomain, VerifyFuture,
};

/// The `typ` header every service assertion carries, and the only one accepted.
///
/// Its whole job is to make a user access token and a service assertion
/// unmistakable for one another in BOTH directions: a user token has no `typ`
/// of this value and is refused here, and this value is not `at+jwt` or `JWT`,
/// so an access-token verifier refuses an assertion. RFC 8725 section 3.11.
pub const SERVICE_ASSERTION_TYP: &str = "svc-assertion+jwt";

/// The opaque mechanism tag stamped on identities this module produces.
pub const JWT_ASSERTION_MECHANISM: &str = "jwt-assertion";

/// The longest assertion lifetime a callee accepts, matching Keycloak.
///
/// A self-signed assertion means the CALLER picks `exp`, so an uncapped
/// lifetime is a caller-controlled replay window. The callee caps it.
pub const MAX_ASSERTION_LIFETIME: Duration = Duration::from_secs(60);

/// Clock-skew tolerance applied to `exp` and `iat`, matching Keycloak.
pub const CLOCK_SKEW_TOLERANCE: Duration = Duration::from_secs(15);

/// The one signature algorithm this profile admits.
///
/// Pinned as a constant fed to [`Validation::new`], which is what makes
/// algorithm confusion structural: `jsonwebtoken` refuses to verify a token
/// whose header `alg` is not in the validation's algorithm list, and `none` is
/// not a variant of [`Algorithm`] at all, so a `none` header fails to parse
/// before any key is chosen. There is deliberately no runtime string compare
/// against the header, because that is the check a future edit can drop
/// without any test noticing.
const ASSERTION_ALGORITHM: Algorithm = Algorithm::EdDSA;

/// The URI scheme of a service issuer identifier.
const SPIFFE_SCHEME: &str = "spiffe://";

/// Characters admitted in an issuer identifier.
///
/// Deliberately narrower than RFC 3986. It excludes `@` (so
/// `spiffe://zeroship.ai@attacker.example/svc/control` cannot masquerade as a
/// trusted host to a reader), `:` (no port), `?`, `#`, `%`, and the `|` used to
/// join issuer and `jti` into a replay-store key.
const fn is_issuer_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '/')
}

/// Characters admitted in a `jti`.
const fn is_jti_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// The longest `jti` accepted, so a hostile caller cannot grow store keys.
///
/// Public so the test that pins the bound can exercise exactly one character
/// either side of it rather than a hardcoded 64 that would quietly stop being
/// the boundary if this changed.
pub const MAX_JTI_LEN: usize = 64;

/// Failure to build a minter, a trust bundle, or an issuer identifier.
///
/// Distinct from [`AuthError`] on purpose: these are configuration and
/// provisioning faults raised at construction time, not verdicts about a
/// credential presented by a peer.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum AssertionError {
    /// An issuer identifier was not a well-formed `spiffe://` URI.
    #[error("malformed service issuer identifier")]
    MalformedIssuer,
    /// Key material could not be read or encoded.
    #[error("service key material rejected: {0}")]
    KeyMaterial(String),
    /// A trust bundle already carried a different key under this issuer + kid.
    #[error("duplicate trust bundle entry for the same issuer and kid")]
    DuplicateTrustEntry,
    /// The assertion could not be signed.
    #[error("service assertion could not be signed: {0}")]
    Signing(String),
}

// ─── Issuer identifiers ──────────────────────────────────────────────────

/// A SPIFFE-shaped service issuer identifier: `spiffe://<domain>/<name>`.
///
/// It is the `iss` of every assertion the service mints and the `aud` of every
/// assertion sent TO it. Section 13 of the service-identity proposal chose this
/// spelling so the same identifiers carry unchanged into X.509 SANs if mTLS
/// ever arrives.
///
/// `aud` being an issuer identifier rather than an endpoint URL is the
/// `draft-ietf-oauth-rfc7523bis` rule, adopted after the 2025
/// audience-injection attacks. Both ends of this module take the typed form, so
/// `aud: "control/get_routes"` - the shape an early draft of the design
/// proposed - cannot be constructed here at all.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ServiceIssuer {
    uri: String,
    principal: ServicePrincipal,
}

impl ServiceIssuer {
    /// Parse a `spiffe://<trust domain>/<service name>` identifier.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::MalformedIssuer`] unless the value is exactly
    /// that shape: the scheme, a non-empty trust domain, and a non-empty path
    /// of non-empty segments, over the restricted character set that keeps an
    /// identifier unambiguous when it is read by a human or joined into a
    /// replay-store key.
    pub fn parse(value: &str) -> Result<Self, AssertionError> {
        let rest = value
            .strip_prefix(SPIFFE_SCHEME)
            .ok_or(AssertionError::MalformedIssuer)?;
        if !rest.chars().all(is_issuer_char) {
            return Err(AssertionError::MalformedIssuer);
        }
        let (domain, path) = rest.split_once('/').ok_or(AssertionError::MalformedIssuer)?;
        if domain.is_empty() || path.is_empty() {
            return Err(AssertionError::MalformedIssuer);
        }
        if path.split('/').any(|segment| {
            segment.is_empty() || segment == "." || segment == ".."
        }) {
            return Err(AssertionError::MalformedIssuer);
        }
        Ok(Self {
            uri: value.to_owned(),
            principal: ServicePrincipal::new(
                TrustDomain::new(domain),
                ServiceName::new(path),
            ),
        })
    }

    /// Return the identifier exactly as it travels on the wire.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.uri
    }

    /// Return the principal this identifier names.
    ///
    /// Trust domain and name are carried together, never handed out
    /// separately: comparing a name without its scope is the defect behind
    /// three Vault CVEs.
    #[must_use]
    pub const fn principal(&self) -> &ServicePrincipal {
        &self.principal
    }
}

// ─── Keys ────────────────────────────────────────────────────────────────

/// An ed25519 keypair a service uses to sign its own assertions.
///
/// Provisioning is an operator responsibility; see the module-level notes in
/// `docs/proposals/2026-08-16-service-identity.md` section 7. This type only
/// gives the operator the two ends of it: generate a key, and publish the JWK
/// half that peers put in their trust bundle.
pub struct ServiceSigningKey {
    inner: ed25519_dalek::SigningKey,
}

impl fmt::Debug for ServiceSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceSigningKey")
            .field("public", &self.public_jwk_x())
            .finish_non_exhaustive()
    }
}

impl ServiceSigningKey {
    /// Generate a fresh keypair from the operating system's CSPRNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut seed = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let inner = ed25519_dalek::SigningKey::from_bytes(&seed);
        seed.fill(0);
        Self { inner }
    }

    /// Load a keypair from its PKCS#8 DER encoding.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::KeyMaterial`] when the bytes are not a
    /// PKCS#8-encoded ed25519 private key.
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, AssertionError> {
        use ed25519_dalek::pkcs8::DecodePrivateKey as _;
        ed25519_dalek::SigningKey::from_pkcs8_der(der)
            .map(|inner| Self { inner })
            .map_err(|error| AssertionError::KeyMaterial(error.to_string()))
    }

    /// Return the base64url public key, the `x` member of the published JWK.
    #[must_use]
    pub fn public_jwk_x(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.inner.verifying_key().to_bytes())
    }

    /// Return the raw 32-byte ed25519 public key.
    #[must_use]
    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.inner.verifying_key().to_bytes()
    }

    fn encoding_key(&self) -> Result<EncodingKey, AssertionError> {
        use ed25519_dalek::pkcs8::EncodePrivateKey as _;
        let der = self
            .inner
            .to_pkcs8_der()
            .map_err(|error| AssertionError::KeyMaterial(error.to_string()))?;
        Ok(EncodingKey::from_ed_der(der.as_bytes()))
    }
}

// ─── The minter ──────────────────────────────────────────────────────────

/// Claims of a service assertion.
///
/// Every field is non-optional, which is what makes each one REQUIRED: a token
/// missing any of them fails to deserialize and is rejected before a single
/// guard runs. `jti` in particular must be structurally required rather than
/// conditionally checked - the whole replay defence rests on it, and a guard
/// written as `if let Some(jti)` silently disappears when the claim is absent.
///
/// `aud` is a `String`, not a list. `draft-ietf-oauth-rfc7523bis` mandates the
/// single issuer identifier of the callee, so a multi-audience assertion is
/// refused here by the type.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct AssertionClaims {
    iss: String,
    sub: String,
    aud: String,
    exp: i64,
    iat: i64,
    jti: String,
}

/// Mints short-lived assertions naming one callee at a time.
pub struct ServiceAssertionMinter {
    issuer: ServiceIssuer,
    key_id: String,
    encoding: EncodingKey,
    lifetime: Duration,
}

impl fmt::Debug for ServiceAssertionMinter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceAssertionMinter")
            .field("issuer", &self.issuer.as_str())
            .field("key_id", &self.key_id)
            .field("lifetime", &self.lifetime)
            .finish_non_exhaustive()
    }
}

impl ServiceAssertionMinter {
    /// Build a minter for one service identity and one signing key.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::KeyMaterial`] when the key cannot be encoded
    /// for signing.
    pub fn new(
        issuer: ServiceIssuer,
        key_id: impl Into<String>,
        signing_key: &ServiceSigningKey,
    ) -> Result<Self, AssertionError> {
        Ok(Self {
            issuer,
            key_id: key_id.into(),
            encoding: signing_key.encoding_key()?,
            lifetime: MAX_ASSERTION_LIFETIME,
        })
    }

    /// Override the lifetime this minter stamps on its assertions.
    ///
    /// Present because the lifetime a caller chooses is NOT a security control.
    /// The callee's ceiling is, and the test that proves the ceiling works has
    /// to be able to mint an over-long assertion. Raising this above
    /// [`MAX_ASSERTION_LIFETIME`] does not widen anything: every conforming
    /// callee rejects the result.
    #[must_use]
    pub const fn with_lifetime(mut self, lifetime: Duration) -> Self {
        self.lifetime = lifetime;
        self
    }

    /// Mint an assertion for one callee, valid from now.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::Signing`] when the JWT cannot be signed.
    pub fn mint(&self, audience: &ServiceIssuer) -> Result<String, AssertionError> {
        self.mint_at(audience, SystemTime::now())
    }

    /// Mint an assertion for one callee as if it were issued at `issued_at`.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::Signing`] when the JWT cannot be signed, and
    /// [`AssertionError::KeyMaterial`] when `issued_at` is not representable as
    /// a Unix timestamp.
    pub fn mint_at(
        &self,
        audience: &ServiceIssuer,
        issued_at: SystemTime,
    ) -> Result<String, AssertionError> {
        let issued = unix_seconds(issued_at)
            .ok_or_else(|| AssertionError::KeyMaterial("issue time out of range".into()))?;
        let lifetime = i64::try_from(self.lifetime.as_secs())
            .map_err(|_| AssertionError::KeyMaterial("lifetime out of range".into()))?;
        let claims = AssertionClaims {
            iss: self.issuer.as_str().to_owned(),
            sub: self.issuer.as_str().to_owned(),
            aud: audience.as_str().to_owned(),
            exp: issued.saturating_add(lifetime),
            iat: issued,
            jti: new_jti(),
        };
        let mut header = Header::new(ASSERTION_ALGORITHM);
        header.typ = Some(SERVICE_ASSERTION_TYP.to_owned());
        header.kid = Some(self.key_id.clone());
        jsonwebtoken::encode(&header, &claims, &self.encoding)
            .map_err(|error| AssertionError::Signing(error.to_string()))
    }
}

/// A fresh 128-bit `jti`, base64url encoded.
fn new_jti() -> String {
    let mut bytes = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn unix_seconds(at: SystemTime) -> Option<i64> {
    at.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|since| i64::try_from(since.as_secs()).ok())
}

// ─── The trust bundle ────────────────────────────────────────────────────

struct TrustedServiceKey {
    key_id: String,
    public: [u8; 32],
    decoding: DecodingKey,
}

/// The verification keys a callee trusts, indexed BY ISSUER.
///
/// The index is the security property, not a lookup convenience. RFC 8725
/// section 3.8 requires the verification key to be resolved from `iss`; a flat
/// pool of every service key means any service's key validates any service's
/// assertion, which collapses identity entirely. Storm-0558 is the canonical
/// instance. [`ServiceAssertionVerifier`] never sees a key that is not already
/// bound to the issuer the assertion claims.
///
/// This is a STATICALLY CONFIGURED bundle rather than JWKS documents fetched
/// from each peer. The reasoning, since the design leaned the other way:
///
/// - Fetching makes authentication depend on peer reachability at first
///   contact, adding a second availability dependency to a hot path that
///   already has one (the replay store). A service that has just restarted
///   would fail inbound calls until it could reach the caller it is being
///   called BY.
/// - Internal hops are cleartext HTTP in this deployment today. A JWKS fetched
///   over plain HTTP is attacker-substitutable, so key distribution would be
///   only as strong as the network the assertions exist to stop trusting.
/// - It does not save configuration. JWKS still needs a per-issuer URL in
///   config; the trade is a public key for a URL, which moves the trust
///   decision to DNS plus TLS plus peer liveness and buys only rotation
///   convenience.
/// - Rotation is still expressible: an issuer may carry several keys at once,
///   so a new key can be trusted before the old one is withdrawn. The operator
///   provisioning the private half is already editing configuration.
///
/// The one thing lost is unattended rotation. That is a real cost and is stated
/// in the report rather than hidden here.
///
/// Reusing [`crate::oidc_verify::JwksCache`] would NOT have given issuer
/// binding for free: it is keyed on a single URL and `keys()` returns a flat
/// `Vec<CachedKey>` with no issuer attached, so a JWKS design would have needed
/// one cache instance per issuer anyway.
#[derive(Default)]
pub struct ServiceTrustBundle {
    issuers: BTreeMap<String, Vec<TrustedServiceKey>>,
}

impl fmt::Debug for ServiceTrustBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let summary: BTreeMap<&String, usize> = self
            .issuers
            .iter()
            .map(|(issuer, keys)| (issuer, keys.len()))
            .collect();
        formatter
            .debug_struct("ServiceTrustBundle")
            .field("keys_by_issuer", &summary)
            .finish()
    }
}

impl ServiceTrustBundle {
    /// Build an empty bundle. A verifier holding one trusts nobody.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Trust `public_key` for assertions issued by `issuer` under `key_id`.
    ///
    /// # Errors
    ///
    /// Returns [`AssertionError::DuplicateTrustEntry`] when the same issuer
    /// already carries a DIFFERENT key under the same `kid`. Re-adding the same
    /// key is idempotent; silently replacing a key would let a later
    /// configuration line revoke an earlier one without saying so.
    pub fn trust(
        &mut self,
        issuer: &ServiceIssuer,
        key_id: impl Into<String>,
        public_key: [u8; 32],
    ) -> Result<(), AssertionError> {
        let key_id = key_id.into();
        let keys = self.issuers.entry(issuer.as_str().to_owned()).or_default();
        if let Some(existing) = keys.iter().find(|key| key.key_id == key_id) {
            if existing.public == public_key {
                return Ok(());
            }
            return Err(AssertionError::DuplicateTrustEntry);
        }
        keys.push(TrustedServiceKey {
            key_id,
            decoding: DecodingKey::from_ed_der(&public_key),
            public: public_key,
        });
        Ok(())
    }

    /// Trust the public half of `signing_key` for `issuer`.
    ///
    /// # Errors
    ///
    /// As [`ServiceTrustBundle::trust`].
    pub fn trust_signing_key(
        &mut self,
        issuer: &ServiceIssuer,
        key_id: impl Into<String>,
        signing_key: &ServiceSigningKey,
    ) -> Result<(), AssertionError> {
        self.trust(issuer, key_id, signing_key.verifying_key_bytes())
    }

    /// Return the keys trusted for exactly this issuer, or nothing.
    fn keys_for(&self, issuer: &str) -> Option<&[TrustedServiceKey]> {
        self.issuers.get(issuer).map(Vec::as_slice)
    }
}

// ─── The replay store ────────────────────────────────────────────────────

/// The verdict of a single-use claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReplayClaim {
    /// This caller is the first and only one to claim the key.
    Accepted,
    /// The key was already claimed and is still within its retention window.
    AlreadyUsed,
}

/// A replay store could not answer.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("replay store unavailable: {0}")]
pub struct ReplayStoreError(pub String);

/// The future returned by [`ReplayStore::claim`].
pub type ClaimFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReplayClaim, ReplayStoreError>> + 'a>>;

/// The `jti` single-use cache.
///
/// # The contract an implementation must meet
///
/// `claim` MUST be a single atomic put-if-absent. Read-then-write is a race a
/// concurrent attacker wins: two verifications of one captured assertion both
/// read "absent", both write, and both succeed.
///
/// The store MUST be shared by every replica of the callee. A per-process cache
/// degrades "single use" into "single use per replica", which is no defence
/// against an attacker who simply retries.
///
/// A claim MUST be retained until at least `expires_at`. Evicting it earlier
/// makes the assertion replayable for the remainder of its validity, which is
/// exactly the window the whole mechanism exists to close.
///
/// An error MUST NOT be treated as a free pass; the verifier rejects on it.
pub trait ReplayStore {
    /// Atomically claim `key` and retain the claim until at least `expires_at`.
    ///
    /// # Errors
    ///
    /// Returns [`ReplayStoreError`] when the store could not be consulted. The
    /// caller must reject the credential.
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a>;
}

/// A process-local [`ReplayStore`].
///
/// Correct for a single-replica callee and for tests. It is NOT sufficient for
/// a replicated callee - see the [`ReplayStore`] contract - and the type name
/// is the only warning the compiler can give, so the choice is deliberately
/// made at the call site that constructs a verifier.
#[derive(Debug, Default)]
pub struct InMemoryReplayStore {
    claimed: Mutex<BTreeMap<String, SystemTime>>,
}

impl InMemoryReplayStore {
    /// Build an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop every claim whose retention window has passed.
    ///
    /// Correctness does not depend on this running: [`InMemoryReplayStore`]
    /// purges lazily on every claim. It exists so a long-lived process can be
    /// swept on a timer instead of growing between claims.
    pub fn purge_expired(&self, now: SystemTime) {
        self.claimed.lock().retain(|_, until| *until > now);
    }

    /// The whole put-if-absent, under one lock, with no await inside it.
    fn claim_now(&self, key: &str, expires_at: SystemTime, now: SystemTime) -> ReplayClaim {
        let mut claimed = self.claimed.lock();
        claimed.retain(|_, until| *until > now);
        if claimed.contains_key(key) {
            return ReplayClaim::AlreadyUsed;
        }
        claimed.insert(key.to_owned(), expires_at);
        ReplayClaim::Accepted
    }
}

impl ReplayStore for InMemoryReplayStore {
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a> {
        let verdict = self.claim_now(key, expires_at, SystemTime::now());
        Box::pin(async move { Ok(verdict) })
    }
}

// ─── The verifier ────────────────────────────────────────────────────────

/// Verifies service assertions against a trust bundle and a replay store.
pub struct ServiceAssertionVerifier {
    bundle: ServiceTrustBundle,
    replay: Arc<dyn ReplayStore>,
    max_lifetime: Duration,
    leeway: Duration,
}

impl fmt::Debug for ServiceAssertionVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ServiceAssertionVerifier")
            .field("bundle", &self.bundle)
            .field("max_lifetime", &self.max_lifetime)
            .field("leeway", &self.leeway)
            .finish_non_exhaustive()
    }
}

impl ServiceAssertionVerifier {
    /// Build a verifier over a trust bundle and a replay store.
    ///
    /// There is no constructor without a replay store. Replay defence is not a
    /// mode of this mechanism; an assertion verifier without it is weaker than
    /// the mTLS it was chosen over, not equivalent to it.
    #[must_use]
    pub fn new(bundle: ServiceTrustBundle, replay: Arc<dyn ReplayStore>) -> Self {
        Self {
            bundle,
            replay,
            max_lifetime: MAX_ASSERTION_LIFETIME,
            leeway: CLOCK_SKEW_TOLERANCE,
        }
    }

    /// The cryptographic half: everything decidable without I/O.
    ///
    /// Split out so the replay claim is unambiguously LAST. An unauthenticated
    /// caller must not be able to write into the replay store, so nothing here
    /// may be reordered after it.
    fn verify_claims(
        &self,
        assertion: &str,
        expected_audience: &str,
        now: SystemTime,
    ) -> Result<(AssertionClaims, String), &'static str> {
        // The expected audience is verifier INPUT and must itself be an issuer
        // identifier. A caller passing an endpoint URL is refused here, which
        // is what makes `aud: "control/get_routes"` unreachable rather than
        // merely discouraged.
        let audience =
            ServiceIssuer::parse(expected_audience).map_err(|_| "expected audience is not an issuer identifier")?;

        let header = decode_header(assertion).map_err(|_| "unparseable JWT header")?;
        if header.typ.as_deref() != Some(SERVICE_ASSERTION_TYP) {
            return Err("typ header is not a service assertion");
        }
        let key_id = header.kid.ok_or("no kid header")?;

        // The issuer is read from the UNVERIFIED payload for one purpose only:
        // choosing which keys may verify it. The signature then has to hold
        // under a key already bound to that issuer, and `Validation::set_issuer`
        // re-checks the verified claim against the same selector, so a lie here
        // can only make verification fail.
        let issuer = ServiceIssuer::parse(&unverified_issuer(assertion)?)
            .map_err(|_| "issuer is not an issuer identifier")?;
        let keys = self
            .bundle
            .keys_for(issuer.as_str())
            .ok_or("no trusted key for this issuer")?;

        let mut validation = Validation::new(ASSERTION_ALGORITHM);
        validation.set_audience(&[audience.as_str()]);
        validation.set_issuer(&[issuer.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "sub", "aud"]);
        validation.validate_exp = true;
        validation.validate_nbf = true;
        validation.leeway = self.leeway.as_secs();

        let claims = keys
            .iter()
            .filter(|key| key.key_id == key_id)
            .find_map(|key| decode::<AssertionClaims>(assertion, &key.decoding, &validation).ok())
            .ok_or("no trusted key for this issuer verified the signature")?
            .claims;

        // RFC 7523 section 3: a service asserting its own identity is both the
        // issuer and the subject. Anything else is a caller claiming to speak
        // for someone, which this mechanism does not grant.
        if claims.sub != claims.iss {
            return Err("sub does not equal iss");
        }

        let now_secs = unix_seconds(now).ok_or("clock out of range")?;
        let leeway = i64::try_from(self.leeway.as_secs()).map_err(|_| "leeway out of range")?;
        let ceiling =
            i64::try_from(self.max_lifetime.as_secs()).map_err(|_| "ceiling out of range")?;

        // The CALLER picks exp, so the CALLEE caps it. Two bounds, and they are
        // both load-bearing:
        //
        // - The lifetime bound catches a long window. It cannot be replaced by
        //   a bound on `exp` alone, because `iat = now - 3600, exp = now + 60`
        //   is a 61-minute assertion whose `exp` is perfectly ordinary.
        // - The `iat` bound catches a window shifted into the future, where
        //   `exp - iat` is small but the assertion stays live for an hour.
        //
        // A THIRD bound was here and has been removed:
        // `exp <= now + ceiling + leeway`. It is implied by these two -
        // `exp = iat + lifetime <= (now + leeway) + ceiling` - so no input can
        // trip it that the pair does not already reject, and no test could make
        // it fail on its own. Worse, while it was present it ALSO caught both
        // of the cases below, so deleting either real guard left the suite
        // green. Measured: mutating away the lifetime bound with the third one
        // present gave "26 passed"; with it gone the ceiling test fails. A
        // redundant guard that hides the failure of a real one is not defence
        // in depth.
        let lifetime = claims.exp.checked_sub(claims.iat).ok_or("lifetime overflow")?;
        if lifetime <= 0 || lifetime > ceiling {
            return Err("assertion lifetime exceeds the ceiling");
        }
        if claims.iat > now_secs.saturating_add(leeway) {
            return Err("iat is in the future");
        }

        if claims.jti.is_empty()
            || claims.jti.len() > MAX_JTI_LEN
            || !claims.jti.chars().all(is_jti_char)
        {
            return Err("jti is missing or malformed");
        }

        Ok((claims, key_id))
    }
}

/// Read `iss` out of a JWT payload that has NOT been verified yet.
fn unverified_issuer(assertion: &str) -> Result<String, &'static str> {
    let mut parts = assertion.split('.');
    let _header = parts.next().ok_or("malformed JWT")?;
    let payload = parts.next().ok_or("malformed JWT")?;
    if parts.next().is_none() {
        return Err("malformed JWT");
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "unparseable JWT payload")?;
    let value: Value = serde_json::from_slice(&decoded).map_err(|_| "unparseable JWT payload")?;
    value
        .get("iss")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or("no iss claim")
}

impl IdentityVerifier for ServiceAssertionVerifier {
    fn verify<'a>(&'a self, credentials: &'a PresentedCredentials<'a>) -> VerifyFuture<'a> {
        Box::pin(async move {
            let now = SystemTime::now();
            let (claims, key_id) = match self.verify_claims(
                credentials.bearer_assertion(),
                credentials.expected_audience(),
                now,
            ) {
                Ok(verified) => verified,
                Err(reason) => {
                    tracing::debug!(reason, "service assertion rejected");
                    return Err(AuthError::CredentialRejected);
                }
            };

            // Retention runs to `exp` plus the same skew the acceptance window
            // uses, so a claim is never evicted while the assertion it covers
            // can still be accepted. A shorter TTL would make the assertion
            // replayable the moment the record went away.
            let retain_until = UNIX_EPOCH
                + Duration::from_secs(
                    u64::try_from(claims.exp).unwrap_or(0).saturating_add(self.leeway.as_secs()),
                );
            // Scoped by issuer so one service cannot burn another service's
            // `jti`, and so the store is safe to share across callees.
            let replay_key = format!("{}|{}", claims.iss, claims.jti);
            match self.replay.claim(&replay_key, retain_until).await {
                Ok(ReplayClaim::Accepted) => {}
                Ok(ReplayClaim::AlreadyUsed) => {
                    tracing::debug!(reason = "jti replayed", "service assertion rejected");
                    return Err(AuthError::CredentialRejected);
                }
                Err(error) => {
                    // Fail closed. A store that cannot answer has not told us
                    // the assertion is fresh.
                    tracing::warn!(%error, "service assertion replay store unavailable");
                    return Err(AuthError::CredentialRejected);
                }
            }

            let mut attributes = BTreeMap::new();
            attributes.insert("kid".to_owned(), json!(key_id));
            attributes.insert("exp".to_owned(), json!(claims.exp));
            attributes.insert("jti".to_owned(), json!(claims.jti));
            // `aud` is deliberately absent. It is verifier INPUT, and an mTLS
            // adapter arriving later would have to fabricate one to fill it.

            let issuer = ServiceIssuer::parse(&claims.iss)
                .map_err(|_| AuthError::CredentialRejected)?;
            Ok(ServiceIdentity::new(
                issuer.principal().clone(),
                MechanismTag::new(JWT_ASSERTION_MECHANISM),
                attributes,
            ))
        })
    }
}
