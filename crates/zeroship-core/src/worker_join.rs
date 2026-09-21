//! The worker JOIN contract: trusted signers, join tokens, proof of possession.
//!
//! A worker does not hold a credential of its own before it joins. It holds a
//! JOIN TOKEN somebody handed it, and it proves possession of the keypair it is
//! registering by signing its own join request with it. The signing authority -
//! the thing that decides a worker should exist - keeps its private key
//! somewhere that is not the machine running creator code.
//!
//! Three documents and one wire format live here, so the writer and the reader
//! of each are one definition:
//!
//! - the SIGNER IMPORT ([`parse_join_signer_import`]), Control's trust anchor:
//!   an id, an Ed25519 public key, and the execution zones that signer may mint
//!   for. `zeroship dev init` writes it and Control reads it.
//! - the SIGNER CREDENTIAL ([`parse_join_signer_credential`]), the private half,
//!   held by whoever mints. `zeroship join-token` reads it, and so does a
//!   single-host Control, which mints for its own zone.
//! - the JOIN TOKEN ([`mint_join_token`], [`verify_join_token`]), a JWT with its
//!   own `typ` so it can neither be presented where a service assertion is
//!   expected nor accept one in its place.
//! - the JOIN PROOF ([`join_proof_message`]), a detached Ed25519 signature over
//!   the token, the presented public key and the claimed port. It is NOT a JWT:
//!   the signer of it has no identity yet, so there is no `iss` to put in one.
//!
//! # What a captured token can and cannot do
//!
//! It can admit workers the captor controls - up to the uses that remain, until
//! its `exp`, in the one zone it names. It cannot register a public key whose
//! private half the presenter does not hold, because the request is signed by
//! that key; and when the token carries `cnf`, it cannot register any key but
//! the one the issuer already had in mind. That is the whole of the bound, and
//! the bound is the reason `exp` is short and `uses` is a cap rather than a
//! formality.

use std::collections::BTreeSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use jsonwebtoken::{decode, decode_header, encode, Algorithm, DecodingKey, Header, Validation};
use serde::{Deserialize, Serialize};

use crate::service_assertion::{
    thumbprint_key_id, AssertionError, ServiceIssuer, ServiceSigningKey, CLOCK_SKEW_TOLERANCE,
    MAX_JTI_LEN,
};

/// The name of the execution zone every deployment declares, seeded by
/// `db/migrations-ts/20260914000450_execution_zones_default_zone.ts`.
pub const DEFAULT_EXECUTION_ZONE: &str = "default";

/// The route Control serves the join on, and the route a joining worker
/// addresses.
///
/// A bare path and NOT a [`crate::service_identity::ServiceEndpoint`], because
/// there is nothing here to authorize. Every declaration in that table pairs a
/// route with the service principals allowed to reach it, and a joining process
/// holds no service identity yet: it presents a join token a trusted signer
/// minted, verified by [`verify_join_token`] under Control's own signer
/// registry and its own `typ`. Declaring it would assert a machine-identity
/// grant that no caller of this route can hold.
///
/// It lives here rather than in either process because both spell it: Control
/// registers this path and the worker builds its URL from it. Two spellings
/// would be a route Control serves and a route the worker never reaches, with
/// every test on either side still green.
pub const WORKER_JOIN_PATH: &str = "/internal/workers/join";

/// The `typ` header every join token carries, and no other token in this stack
/// does.
///
/// It is the separation that keeps two mechanisms from being one. A service
/// assertion is `svc-assertion+jwt` and its verifier requires that exact value,
/// so a join token cannot be presented as a service assertion; this verifier
/// requires this value, so a service assertion - which a worker's peers can
/// obtain - cannot be presented as a join token. Neither refusal is a
/// convention a later edit can widen without deleting a line that says why.
pub const JOIN_TOKEN_TYP: &str = "worker-join+jwt";

/// The context string the join proof is domain-separated by.
///
/// A raw Ed25519 signature is a signature over bytes and nothing else, so two
/// protocols that sign unprefixed bytes with one key can be made to sign for
/// each other. This prefix is what makes a join proof unusable anywhere else.
pub const JOIN_PROOF_CONTEXT: &str = "zeroship-worker-join-proof/v1";

/// The longest join token this verifier accepts, measured `exp - iat`.
///
/// The MINTER picks `exp`, so the VERIFIER caps it - the same split the service
/// assertion ceiling already takes, for the same reason: a bound only the
/// minting side applies is a bound nothing checks. It is deliberately far
/// looser than either minter's default, because the quantity it defends against
/// is not "longer than we meant" but "a token that outlives the incident that
/// leaked it". Both minters in this tree default to minutes.
pub const MAX_JOIN_TOKEN_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

/// The largest `uses` a join token may claim.
///
/// A cap on a cap: it bounds a mistyped `--uses` and an issuer that meant to
/// write a fleet size. It is not a fleet limit - a deployment larger than this
/// mints more than one token, which it has to do for zones anyway.
pub const MAX_JOIN_TOKEN_USES: u32 = 4096;

/// The longest execution-zone name a token may name.
const MAX_ZONE_LEN: usize = 64;

/// How long an admitted instance identity stays live without renewal.
///
/// This is what makes revocation stop being the only way a credential ever
/// stops working. A worker that crashes, is killed, or is simply forgotten
/// leaves a row that stops satisfying Control's instance read on its own, with
/// nothing observing liveness to make it happen.
pub const INSTANCE_LEASE_TTL: Duration = Duration::from_secs(600);

/// How many renewal attempts must fit inside one lease.
///
/// The renewal interval is DERIVED from the lease through this factor rather
/// than chosen beside it, so the two cannot be put out of order. A factor of
/// one would mean a single attempt with no room to retry; anything above two
/// leaves at least one whole spare attempt after a failure.
pub const INSTANCE_RENEWALS_PER_LEASE: u32 = 3;

/// One attempt is not a schedule: a worker whose single renewal fails has no
/// second chance before its identity lapses. Checked at COMPILE time, because
/// the claim is about two constants and a build is the only place a claim about
/// constants can be checked without ever being skipped.
const _: () = assert!(
    INSTANCE_RENEWALS_PER_LEASE >= 2,
    "a lease must leave room for a failed renewal to be retried"
);

/// How often a joined worker renews its instance identity.
///
/// [`INSTANCE_LEASE_TTL`] divided by [`INSTANCE_RENEWALS_PER_LEASE`]. The margin
/// the renewal has to play with is the remainder, which is what a failed
/// attempt is retried inside.
#[must_use]
pub const fn instance_renewal_interval() -> Duration {
    Duration::from_secs(INSTANCE_LEASE_TTL.as_secs() / INSTANCE_RENEWALS_PER_LEASE as u64)
}

/// The algorithm every join token is signed with.
const JOIN_TOKEN_ALGORITHM: Algorithm = Algorithm::EdDSA;

// ---------------------------------------------------------------------------
// The signer import document
// ---------------------------------------------------------------------------

/// One trusted signer as Control's import names it, validated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinSignerRecord {
    /// The signer's `wjs_` typed id.
    pub id: String,
    /// The NAMES of the execution zones this signer may mint join tokens for,
    /// sorted and deduplicated so two spellings of one set compare equal.
    pub zones: Vec<String>,
    /// The raw Ed25519 public key.
    pub public_key: [u8; 32],
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportDocument {
    signers: Vec<ImportEntry>,
}

/// Unknown members are refused rather than ignored, so a misspelt `zones` is a
/// refused document and not a signer trusted for no zone.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportEntry {
    id: String,
    zones: Vec<String>,
    public_key: String,
}

/// Parse and validate Control's trusted-signer import document.
///
/// # Errors
///
/// Returns a message naming the offending entry when the bytes are not the
/// document, the list is empty, an id is not a `wjs_` typed id, the zone list
/// is empty or holds a name that is empty, padded or overlong, a public key is
/// not a usable Ed25519 key, or two entries share an id or a public key. An
/// empty list is refused because a configured document naming no signer is far
/// likelier a wrong file than an intent.
pub fn parse_join_signer_import(bytes: &[u8]) -> Result<Vec<JoinSignerRecord>, String> {
    let document: ImportDocument =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if document.signers.is_empty() {
        return Err("the document names no join signers".to_owned());
    }
    let mut ids = BTreeSet::new();
    let mut keys = BTreeSet::new();
    let mut records = Vec::with_capacity(document.signers.len());
    for entry in document.signers {
        let record = validate_import_entry(entry)?;
        if !ids.insert(record.id.clone()) {
            return Err(format!("join signer {} is named twice", record.id));
        }
        if !keys.insert(record.public_key) {
            return Err(format!(
                "join signer {} has a public key another entry already names; every signer \
                 needs a key of its own",
                record.id
            ));
        }
        records.push(record);
    }
    Ok(records)
}

/// Render an import document, one entry per record, in the order given.
///
/// # Panics
///
/// Never: the document is plain strings, which always serialize.
#[must_use]
pub fn render_join_signer_import(records: &[JoinSignerRecord]) -> String {
    let document = ImportDocument {
        signers: records
            .iter()
            .map(|record| ImportEntry {
                id: record.id.clone(),
                zones: record.zones.clone(),
                public_key: URL_SAFE_NO_PAD.encode(record.public_key),
            })
            .collect(),
    };
    let mut text = serde_json::to_string_pretty(&document).expect("the document serializes");
    text.push('\n');
    text
}

fn validate_import_entry(entry: ImportEntry) -> Result<JoinSignerRecord, String> {
    crate::typed_id::parse_with_prefix(&entry.id, crate::typed_id::JOIN_SIGNER_PREFIX)
        .map_err(|error| format!("{:?} is not a join signer id: {error}", entry.id))?;
    if entry.zones.is_empty() {
        return Err(format!(
            "join signer {} names no execution zone, so it could mint nothing",
            entry.id
        ));
    }
    let mut zones = BTreeSet::new();
    for zone in &entry.zones {
        validate_zone_name(zone)
            .map_err(|reason| format!("join signer {}: {reason}", entry.id))?;
        zones.insert(zone.clone());
    }
    let raw = URL_SAFE_NO_PAD
        .decode(entry.public_key.as_bytes())
        .map_err(|_| format!("join signer {}: public_key is not base64url", entry.id))?;
    let public_key = <[u8; 32]>::try_from(raw.as_slice())
        .map_err(|_| format!("join signer {}: public_key is not a raw Ed25519 key", entry.id))?;
    // A 32-byte string is not thereby a key. A point off the curve can verify
    // nothing, and a small-order one verifies signatures nobody made, so both
    // are refused here where the operator can still see which line it was.
    match ed25519_dalek::VerifyingKey::from_bytes(&public_key) {
        Ok(key) if !key.is_weak() => {}
        _ => {
            return Err(format!(
                "join signer {}: public_key is not a usable Ed25519 public key",
                entry.id
            ));
        }
    }
    Ok(JoinSignerRecord {
        id: entry.id,
        zones: zones.into_iter().collect(),
        public_key,
    })
}

/// Whether `zone` is a spelling of an execution zone name at all.
///
/// # Errors
///
/// Returns a message naming what is wrong with it.
fn validate_zone_name(zone: &str) -> Result<(), String> {
    if zone.is_empty() || zone.trim() != zone {
        return Err(format!(
            "{zone:?} is not an execution zone name; it is empty or padded"
        ));
    }
    if zone.len() > MAX_ZONE_LEN {
        return Err(format!("{zone:?} is longer than an execution zone name may be"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The signer credential document
// ---------------------------------------------------------------------------

/// The signer's private half, as whoever mints writes it.
///
/// ```json
/// { "signer_id": "wjs_...",
///   "private_key": "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----\n" }
/// ```
///
/// ONE document rather than a key file and a separate id setting, because the
/// two are one credential: Control verifies a token against the key it recorded
/// FOR THAT ID, so an id and a key that drifted apart would refuse every join
/// while each half looked configured.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CredentialDocument {
    signer_id: String,
    private_key: String,
}

/// Parse a signer credential document into its id and signing key.
///
/// # Errors
///
/// Returns a message when the bytes are not the document, the id is not a
/// `wjs_` typed id, or the private key is not a PKCS#8 PEM Ed25519 key.
pub fn parse_join_signer_credential(
    bytes: &[u8],
) -> Result<(String, ServiceSigningKey), String> {
    let document: CredentialDocument =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    crate::typed_id::parse_with_prefix(&document.signer_id, crate::typed_id::JOIN_SIGNER_PREFIX)
        .map_err(|error| {
            format!(
                "signer_id {:?} is not a join signer id: {error}",
                document.signer_id
            )
        })?;
    let der = pem_private_key_body(&document.private_key)
        .ok_or_else(|| "private_key is not a PKCS#8 PEM private key".to_owned())?;
    let key = ServiceSigningKey::from_pkcs8_der(&der)
        .map_err(|error| format!("private_key is not an ed25519 PKCS#8 key: {error}"))?;
    Ok((document.signer_id, key))
}

/// Render a signer credential document.
///
/// # Errors
///
/// Returns a message when the document does not serialize.
pub fn render_join_signer_credential(signer_id: &str, private_key_pem: &str) -> Result<String, String> {
    let document = CredentialDocument {
        signer_id: signer_id.to_owned(),
        private_key: private_key_pem.to_owned(),
    };
    let mut text =
        serde_json::to_string_pretty(&document).map_err(|error| error.to_string())?;
    text.push('\n');
    Ok(text)
}

/// Decode the DER body of a PKCS#8 PEM private key.
///
/// A second, deliberately narrow copy of the armor reader `service_peers` keeps
/// private. It is narrow on purpose: it accepts only the PRIVATE KEY label, so
/// a certificate or a public key pasted into the field is refused here rather
/// than producing a key-material error that names the wrong thing.
fn pem_private_key_body(text: &str) -> Option<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let start = text.find(BEGIN)? + BEGIN.len();
    let end = text[start..].find(END)? + start;
    let body: String = text[start..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok()
}

// ---------------------------------------------------------------------------
// The join token
// ---------------------------------------------------------------------------

/// The claims of a join token.
///
/// Every field but `cnf` is non-optional, which is what makes each REQUIRED: a
/// token missing any of them fails to deserialize and is refused before a guard
/// runs. `cnf` is the one claim whose ABSENCE is meaningful - an issuer that
/// does not yet know the joining key cannot state one.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct JoinTokenClaims {
    iss: String,
    sub: String,
    aud: String,
    exp: i64,
    iat: i64,
    jti: String,
    zone: String,
    uses: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cnf: Option<Confirmation>,
}

/// RFC 7800 confirmation: the JWK thumbprint of the key the token admits.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Confirmation {
    jkt: String,
}

/// What an issuer asks for when it mints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JoinTokenGrant {
    /// The execution zone the token admits into.
    pub zone: String,
    /// How long the token stays valid.
    pub lifetime: Duration,
    /// How many workers it may admit.
    pub uses: u32,
    /// The joining key's raw public half, when the issuer knows it in advance.
    pub confirm: Option<[u8; 32]>,
}

/// A join token whose signature, audience, expiry and claims have all held.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedJoinToken {
    /// The `wjs_` id of the signer that minted it.
    pub signer_id: String,
    /// The token's own `jti`, recorded on the instance it admits and used as
    /// the key one use is consumed under.
    pub token_id: String,
    /// The execution zone NAME the token admits into. The zone comes from here
    /// and from nowhere a joining worker can reach.
    pub zone: String,
    /// How many workers this token may admit in total.
    pub uses: u32,
    /// The thumbprint the joining key must match, when the issuer stated one.
    pub confirmation: Option<String>,
    /// `exp` plus the verifier's skew tolerance: the instant after which no
    /// presentation of this token can be accepted, and therefore the instant
    /// its use accounting may be reclaimed.
    pub expires_at: SystemTime,
}

/// Why a join token was refused.
///
/// A closed set with a stable machine-readable spelling, because the reason
/// travels to the joining worker: an operator reading a refused boot needs to
/// know whether the signer is unknown, the token expired, or the zone is one
/// that signer may not mint for. None of it tells a caller anything a
/// success-versus-failure probe would not.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JoinTokenRefusal {
    /// Not a JWT, not this `typ`, or the payload does not hold the claims.
    Malformed,
    /// The `iss` is not an identifier naming a `wjs_` signer.
    SignerMalformed,
    /// No trusted key verified the signature.
    Signature,
    /// `aud` is not this control plane.
    Audience,
    /// `exp` has passed, or `iat` is in the future, under the skew tolerance.
    Expired,
    /// `exp - iat` is longer than a join token may live.
    Lifetime,
    /// `jti` is absent, overlong, or not a replay-store-safe string.
    TokenId,
    /// `zone` is not an execution zone name.
    Zone,
    /// `uses` is zero or above the ceiling.
    Uses,
    /// `cnf` is present but does not carry a usable thumbprint.
    Confirmation,
}

impl JoinTokenRefusal {
    /// The stable machine-readable reason, carried in the response body.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "token_malformed",
            Self::SignerMalformed => "token_signer_malformed",
            Self::Signature => "token_signature",
            Self::Audience => "token_audience",
            Self::Expired => "token_expired",
            Self::Lifetime => "token_lifetime",
            Self::TokenId => "token_id",
            Self::Zone => "token_zone",
            Self::Uses => "token_uses",
            Self::Confirmation => "token_confirmation",
        }
    }
}

/// Mint one join token.
///
/// # Errors
///
/// Returns a message when the grant is not mintable (an unusable zone name, a
/// `uses` of zero or above [`MAX_JOIN_TOKEN_USES`], a lifetime above
/// [`MAX_JOIN_TOKEN_LIFETIME`]) or when the key cannot sign. The minter refuses
/// what the verifier would refuse, so a token this function returns is one the
/// verifier can accept - a minter that could produce a token nothing accepts is
/// a fault an operator finds at the worker instead of at the mint.
pub fn mint_join_token(
    signer_id: &str,
    key: &ServiceSigningKey,
    audience: &ServiceIssuer,
    grant: &JoinTokenGrant,
) -> Result<String, String> {
    mint_join_token_at(signer_id, key, audience, grant, SystemTime::now())
}

/// Mint one join token as of `now`. The seam every test that needs a clock
/// uses, so no test reaches for a sleep.
///
/// # Errors
///
/// See [`mint_join_token`].
pub fn mint_join_token_at(
    signer_id: &str,
    key: &ServiceSigningKey,
    audience: &ServiceIssuer,
    grant: &JoinTokenGrant,
    now: SystemTime,
) -> Result<String, String> {
    validate_zone_name(&grant.zone)?;
    if grant.uses == 0 || grant.uses > MAX_JOIN_TOKEN_USES {
        return Err(format!(
            "uses must be between 1 and {MAX_JOIN_TOKEN_USES}, not {}",
            grant.uses
        ));
    }
    if grant.lifetime.is_zero() || grant.lifetime > MAX_JOIN_TOKEN_LIFETIME {
        return Err(format!(
            "a join token may live between one second and {} seconds",
            MAX_JOIN_TOKEN_LIFETIME.as_secs()
        ));
    }
    let issuer = crate::service_peers::join_signer_issuer(signer_id)
        .map_err(|_| format!("{signer_id:?} is not a join signer id"))?;
    let issued_at = unix_seconds(now).ok_or_else(|| "clock out of range".to_owned())?;
    let lifetime =
        i64::try_from(grant.lifetime.as_secs()).map_err(|_| "lifetime out of range".to_owned())?;
    let claims = JoinTokenClaims {
        iss: issuer.as_str().to_owned(),
        sub: issuer.as_str().to_owned(),
        aud: audience.as_str().to_owned(),
        exp: issued_at
            .checked_add(lifetime)
            .ok_or_else(|| "expiry out of range".to_owned())?,
        iat: issued_at,
        jti: new_token_id(),
        zone: grant.zone.clone(),
        uses: grant.uses,
        cnf: grant.confirm.map(|public| Confirmation {
            jkt: thumbprint_key_id(&public),
        }),
    };
    let mut header = Header::new(JOIN_TOKEN_ALGORITHM);
    header.typ = Some(JOIN_TOKEN_TYP.to_owned());
    header.kid = Some(key.key_id());
    encode(&header, &claims, &key.encoding_key().map_err(|error: AssertionError| error.to_string())?)
        .map_err(|error| format!("could not mint the join token: {error}"))
}

/// A fresh 128-bit token id, base64url encoded.
fn new_token_id() -> String {
    use rand::RngCore as _;
    let mut raw = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    URL_SAFE_NO_PAD.encode(raw)
}

/// The `wjs_` signer id a token CLAIMS, read from the UNVERIFIED payload.
///
/// Read unverified for one purpose: choosing which recorded key may verify it.
/// Nothing is trusted on the strength of it - the signature still has to hold
/// under the key recorded for that id, and [`verify_join_token`] re-checks the
/// verified `iss` against the same identifier, so a lie here can only select a
/// key that fails to verify.
#[must_use]
pub fn unverified_join_signer_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    if token.split('.').count() != 3 {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let issuer = ServiceIssuer::parse(value.get("iss")?.as_str()?).ok()?;
    let expected = crate::service_peers::service_issuer(
        crate::service_peers::WORKER_JOIN_SIGNER_SERVICE_NAME,
    )
    .ok()?;
    if issuer.principal() != expected.principal() {
        return None;
    }
    let id = issuer.instance()?;
    crate::typed_id::parse_with_prefix(id, crate::typed_id::JOIN_SIGNER_PREFIX).ok()?;
    Some(id.to_owned())
}

/// Verify a join token against the key recorded for the signer it names.
///
/// The checks run in the order the design states them - signature, audience,
/// expiry, then the claims the join contract adds - and each is its own
/// statement so each can be mutated on its own and refused with its own reason.
/// In particular the signature is verified with `exp` and `aud` validation
/// DISABLED inside the decoder and re-applied here: leaving them to the decoder
/// would fold three independent verdicts into whichever one it happened to
/// reach first, and a refusal that names the wrong check is a refusal an
/// operator acts on wrongly.
///
/// The ZONE is not checked against a signer's permitted set here. That set
/// lives in Control's registry beside the key, and this function takes key
/// material rather than a registry; the caller checks it, and the database
/// checks it again at insert.
///
/// # Errors
///
/// Returns the [`JoinTokenRefusal`] that fired.
pub fn verify_join_token(
    token: &str,
    signer_public_key: &[u8; 32],
    expected_audience: &ServiceIssuer,
    now: SystemTime,
) -> Result<VerifiedJoinToken, JoinTokenRefusal> {
    let header = decode_header(token).map_err(|_| JoinTokenRefusal::Malformed)?;
    if header.typ.as_deref() != Some(JOIN_TOKEN_TYP) {
        return Err(JoinTokenRefusal::Malformed);
    }
    if header.kid.as_deref() != Some(thumbprint_key_id(signer_public_key).as_str()) {
        return Err(JoinTokenRefusal::Signature);
    }
    let signer_id =
        unverified_join_signer_id(token).ok_or(JoinTokenRefusal::SignerMalformed)?;
    let issuer = crate::service_peers::join_signer_issuer(&signer_id)
        .map_err(|_| JoinTokenRefusal::SignerMalformed)?;

    let mut validation = Validation::new(JOIN_TOKEN_ALGORITHM);
    validation.set_issuer(&[issuer.as_str()]);
    validation.set_required_spec_claims(&["exp", "iat", "iss", "sub", "aud", "jti"]);
    // Both OFF here and both re-applied below, in the stated order.
    validation.validate_exp = false;
    validation.validate_aud = false;
    // `DecodingKey::from_ed_der` takes the RAW 32-byte public key despite its
    // name - the same call `ServiceTrustBundle` makes, so the two verifiers
    // cannot disagree about what a public key is.
    let claims = decode::<JoinTokenClaims>(
        token,
        &DecodingKey::from_ed_der(signer_public_key),
        &validation,
    )
    .map_err(|_| JoinTokenRefusal::Signature)?
    .claims;

    // RFC 7523 section 3: an issuer asserting its own identity is both the
    // issuer and the subject. Anything else is a caller speaking for someone.
    if claims.sub != claims.iss {
        return Err(JoinTokenRefusal::Signature);
    }
    if claims.aud != expected_audience.as_str() {
        return Err(JoinTokenRefusal::Audience);
    }

    let now_secs = unix_seconds(now).ok_or(JoinTokenRefusal::Expired)?;
    let leeway =
        i64::try_from(CLOCK_SKEW_TOLERANCE.as_secs()).map_err(|_| JoinTokenRefusal::Expired)?;
    if claims.exp <= now_secs.saturating_sub(leeway) {
        return Err(JoinTokenRefusal::Expired);
    }
    if claims.iat > now_secs.saturating_add(leeway) {
        return Err(JoinTokenRefusal::Expired);
    }
    let ceiling = i64::try_from(MAX_JOIN_TOKEN_LIFETIME.as_secs())
        .map_err(|_| JoinTokenRefusal::Lifetime)?;
    let lifetime = claims
        .exp
        .checked_sub(claims.iat)
        .ok_or(JoinTokenRefusal::Lifetime)?;
    if lifetime <= 0 || lifetime > ceiling {
        return Err(JoinTokenRefusal::Lifetime);
    }

    if claims.jti.is_empty()
        || claims.jti.len() > MAX_JTI_LEN
        || !claims.jti.chars().all(crate::service_assertion::is_jti_char)
    {
        return Err(JoinTokenRefusal::TokenId);
    }
    if validate_zone_name(&claims.zone).is_err() {
        return Err(JoinTokenRefusal::Zone);
    }
    if claims.uses == 0 || claims.uses > MAX_JOIN_TOKEN_USES {
        return Err(JoinTokenRefusal::Uses);
    }
    let confirmation = match claims.cnf {
        None => None,
        Some(ref confirmation) => {
            if confirmation.jkt.is_empty() || confirmation.jkt.len() > MAX_JTI_LEN {
                return Err(JoinTokenRefusal::Confirmation);
            }
            Some(confirmation.jkt.clone())
        }
    };

    let expires_at = UNIX_EPOCH
        .checked_add(Duration::from_secs(
            u64::try_from(claims.exp).map_err(|_| JoinTokenRefusal::Expired)?,
        ))
        .ok_or(JoinTokenRefusal::Expired)?
        .checked_add(CLOCK_SKEW_TOLERANCE)
        .ok_or(JoinTokenRefusal::Expired)?;

    Ok(VerifiedJoinToken {
        signer_id,
        token_id: claims.jti,
        zone: claims.zone,
        uses: claims.uses,
        confirmation,
        expires_at,
    })
}

fn unix_seconds(instant: SystemTime) -> Option<i64> {
    instant
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
}

// ---------------------------------------------------------------------------
// The join proof
// ---------------------------------------------------------------------------

/// The exact bytes a joining worker signs with the key it is registering.
///
/// It binds THREE things, and each is load-bearing:
///
/// - the TOKEN, so a proof captured from one join cannot be replayed against a
///   different token the same key was offered;
/// - the PUBLIC KEY, so the proof is about the key being registered rather than
///   about the request in general - this is what makes presenting somebody
///   else's public key useless;
/// - the PORT, so the one value the registrant contributes to its own address
///   is covered rather than left free for an on-path party to change.
///
/// The fields are newline-separated and none of them may contain a newline: a
/// JWT is base64url, a public key is base64url, and a port is decimal digits.
/// Ambiguity between two field splittings is therefore impossible rather than
/// merely unlikely.
#[must_use]
pub fn join_proof_message(token: &str, joining_public_key: &[u8; 32], port: u16) -> Vec<u8> {
    let mut message = Vec::with_capacity(JOIN_PROOF_CONTEXT.len() + token.len() + 64);
    message.extend_from_slice(JOIN_PROOF_CONTEXT.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(token.as_bytes());
    message.push(b'\n');
    message.extend_from_slice(URL_SAFE_NO_PAD.encode(joining_public_key).as_bytes());
    message.push(b'\n');
    message.extend_from_slice(port.to_string().as_bytes());
    message
}

/// Whether `signature` is this key's signature over the join it claims.
///
/// `verify_strict` rather than `verify`: it rejects small-order public keys and
/// non-canonical signature encodings, which is what makes "this signature is
/// this key's" a statement about one key rather than about a class of them.
#[must_use]
pub fn verify_join_proof(
    joining_public_key: &[u8; 32],
    token: &str,
    port: u16,
    signature: &[u8; 64],
) -> bool {
    let Ok(key) = ed25519_dalek::VerifyingKey::from_bytes(joining_public_key) else {
        return false;
    };
    let message = join_proof_message(token, joining_public_key, port);
    key.verify_strict(&message, &ed25519_dalek::Signature::from_bytes(signature))
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service_peers::service_issuer;

    fn control_audience() -> ServiceIssuer {
        service_issuer(crate::service_peers::CONTROL_SERVICE_NAME).expect("control issuer")
    }

    fn grant() -> JoinTokenGrant {
        JoinTokenGrant {
            zone: DEFAULT_EXECUTION_ZONE.to_owned(),
            lifetime: Duration::from_secs(300),
            uses: 3,
            confirm: None,
        }
    }

    fn record() -> JoinSignerRecord {
        JoinSignerRecord {
            id: crate::typed_id::new_join_signer_id(),
            zones: vec![DEFAULT_EXECUTION_ZONE.to_owned()],
            public_key: ed25519_dalek::SigningKey::from_bytes(&[3_u8; 32])
                .verifying_key()
                .to_bytes(),
        }
    }

    /// The writer's output is the reader's input.
    #[test]
    fn a_rendered_import_parses_back_to_its_records() {
        let mut second = record();
        second.id = crate::typed_id::new_join_signer_id();
        second.zones = vec!["default".to_owned(), "eu".to_owned()];
        second.public_key = ed25519_dalek::SigningKey::from_bytes(&[4_u8; 32])
            .verifying_key()
            .to_bytes();
        let records = vec![record(), second];
        assert_eq!(
            parse_join_signer_import(render_join_signer_import(&records).as_bytes()),
            Ok(records)
        );
    }

    /// Two spellings of one zone set parse to one canonical record, so
    /// Control's "does this disagree with what is recorded" comparison cannot
    /// be defeated by reordering a list.
    #[test]
    fn a_zone_list_is_canonicalised_by_the_parser() {
        let key = URL_SAFE_NO_PAD.encode(record().public_key);
        let id = crate::typed_id::new_join_signer_id();
        let one = format!(
            r#"{{"signers":[{{"id":"{id}","zones":["eu","default","eu"],"public_key":"{key}"}}]}}"#
        );
        let other = format!(
            r#"{{"signers":[{{"id":"{id}","zones":["default","eu"],"public_key":"{key}"}}]}}"#
        );
        assert_eq!(
            parse_join_signer_import(one.as_bytes()),
            parse_join_signer_import(other.as_bytes())
        );
        assert_eq!(
            parse_join_signer_import(one.as_bytes()).expect("parses")[0].zones,
            vec!["default".to_owned(), "eu".to_owned()]
        );
    }

    /// Each refusal changes one thing about an otherwise valid entry. The
    /// control is the unchanged entry, which parses.
    /// An import document around whatever entries a case supplies.
    fn document(entries: &serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "signers": entries })).expect("json")
    }

    /// One entry, with every field a case might want to spoil.
    fn entry(id: &str, zones: &serde_json::Value, key: &str) -> serde_json::Value {
        serde_json::json!({"id": id, "zones": zones, "public_key": key})
    }

    #[test]
    fn an_import_that_is_wrong_in_any_one_way_is_refused() {
        let valid = record();
        let key = URL_SAFE_NO_PAD.encode(valid.public_key);
        let zones = serde_json::json!([DEFAULT_EXECUTION_ZONE]);
        assert!(parse_join_signer_import(&document(&serde_json::json!([entry(
            &valid.id,
            &zones,
            &key
        )])))
        .is_ok());

        let other_id = crate::typed_id::new_join_signer_id();
        for (label, bytes) in [
            ("not JSON", b"not json".to_vec()),
            ("no signers", document(&serde_json::json!([]))),
            (
                "an unknown member",
                document(&serde_json::json!([{
                    "id": valid.id, "zones": zones, "public_key": key, "status": "active"
                }])),
            ),
            (
                "a worker instance id",
                document(&serde_json::json!([entry(
                    "wkr_0000000000000000000000001",
                    &zones,
                    &key
                )])),
            ),
            (
                "no zones",
                document(&serde_json::json!([entry(
                    &valid.id,
                    &serde_json::json!([]),
                    &key
                )])),
            ),
            (
                "an empty zone",
                document(&serde_json::json!([entry(
                    &valid.id,
                    &serde_json::json!([""]),
                    &key
                )])),
            ),
            (
                "a padded zone",
                document(&serde_json::json!([entry(
                    &valid.id,
                    &serde_json::json!([" default"]),
                    &key
                )])),
            ),
            (
                "a short key",
                document(&serde_json::json!([entry(
                    &valid.id,
                    &zones,
                    &URL_SAFE_NO_PAD.encode([7_u8; 31])
                )])),
            ),
            (
                "a small-order key",
                document(&serde_json::json!([entry(
                    &valid.id,
                    &zones,
                    &URL_SAFE_NO_PAD.encode([0_u8; 32])
                )])),
            ),
            (
                "one id twice",
                document(&serde_json::json!([
                    entry(&valid.id, &zones, &key),
                    entry(
                        &valid.id,
                        &zones,
                        &URL_SAFE_NO_PAD.encode(
                            ed25519_dalek::SigningKey::from_bytes(&[5_u8; 32])
                                .verifying_key()
                                .to_bytes()
                        )
                    ),
                ])),
            ),
            (
                "one key twice",
                document(&serde_json::json!([
                    entry(&valid.id, &zones, &key),
                    entry(&other_id, &zones, &key),
                ])),
            ),
        ] {
            assert!(
                parse_join_signer_import(&bytes).is_err(),
                "{label} must be refused"
            );
        }
    }

    /// PKCS#8 PEM armor around a DER body, assembled here rather than through
    /// the `pem` feature so this test does not decide the crate's feature set.
    fn pkcs8_pem(key: &ed25519_dalek::SigningKey) -> String {
        use ed25519_dalek::pkcs8::EncodePrivateKey as _;
        let der = key.to_pkcs8_der().expect("der");
        let body = base64::engine::general_purpose::STANDARD.encode(der.as_bytes());
        let wrapped = body
            .as_bytes()
            .chunks(64)
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect::<Vec<_>>()
            .join("\n");
        format!("-----BEGIN PRIVATE KEY-----\n{wrapped}\n-----END PRIVATE KEY-----\n")
    }

    /// A rendered credential parses back, and a mismatched id is refused.
    #[test]
    fn a_signer_credential_round_trips_and_refuses_a_foreign_id() {
        let key = ed25519_dalek::SigningKey::from_bytes(&[9_u8; 32]);
        let pem = pkcs8_pem(&key);
        let id = crate::typed_id::new_join_signer_id();
        let document = render_join_signer_credential(&id, &pem).expect("renders");
        let (parsed_id, parsed_key) =
            parse_join_signer_credential(document.as_bytes()).expect("parses");
        assert_eq!(parsed_id, id);
        assert_eq!(parsed_key.verifying_key_bytes(), key.verifying_key().to_bytes());

        for bad in [
            render_join_signer_credential("wkr_0000000000000000000000001", &pem).expect("renders"),
            render_join_signer_credential(&id, "not a pem").expect("renders"),
        ] {
            assert!(parse_join_signer_credential(bad.as_bytes()).is_err());
        }
    }

    /// THE CONTROL for every refusal below: a freshly minted token verifies.
    #[test]
    fn a_freshly_minted_token_verifies() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let token =
            mint_join_token(&id, &key, &control_audience(), &grant()).expect("mints");
        let verified = verify_join_token(
            &token,
            &key.verifying_key_bytes(),
            &control_audience(),
            SystemTime::now(),
        )
        .expect("verifies");
        assert_eq!(verified.signer_id, id);
        assert_eq!(verified.zone, DEFAULT_EXECUTION_ZONE);
        assert_eq!(verified.uses, 3);
        assert_eq!(verified.confirmation, None);
        assert!(!verified.token_id.is_empty());
    }

    /// Two mints of one grant differ in their token id, so `uses` accounting
    /// keyed on it cannot conflate two tokens.
    #[test]
    fn two_mints_carry_different_token_ids() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let audience = control_audience();
        let first = verify_join_token(
            &mint_join_token(&id, &key, &audience, &grant()).expect("mints"),
            &key.verifying_key_bytes(),
            &audience,
            SystemTime::now(),
        )
        .expect("verifies");
        let second = verify_join_token(
            &mint_join_token(&id, &key, &audience, &grant()).expect("mints"),
            &key.verifying_key_bytes(),
            &audience,
            SystemTime::now(),
        )
        .expect("verifies");
        assert_ne!(first.token_id, second.token_id);
    }

    /// Another signer's key does not verify this signer's token, and the
    /// refusal names the signature rather than something downstream of it.
    #[test]
    fn a_token_signed_by_another_key_is_refused() {
        let key = ServiceSigningKey::generate();
        let other = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let token = mint_join_token(&id, &key, &control_audience(), &grant()).expect("mints");
        assert_eq!(
            verify_join_token(
                &token,
                &other.verifying_key_bytes(),
                &control_audience(),
                SystemTime::now()
            ),
            Err(JoinTokenRefusal::Signature)
        );
    }

    /// A token minted for another callee is refused, with the audience named.
    #[test]
    fn a_token_minted_for_another_audience_is_refused() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let elsewhere =
            service_issuer(crate::service_peers::GATEWAY_SERVICE_NAME).expect("gateway issuer");
        let token = mint_join_token(&id, &key, &elsewhere, &grant()).expect("mints");
        assert_eq!(
            verify_join_token(
                &token,
                &key.verifying_key_bytes(),
                &control_audience(),
                SystemTime::now()
            ),
            Err(JoinTokenRefusal::Audience)
        );
        // The one-variable control: the same token, verified against the
        // audience it was minted for, is accepted.
        assert!(verify_join_token(
            &token,
            &key.verifying_key_bytes(),
            &elsewhere,
            SystemTime::now()
        )
        .is_ok());
    }

    /// Expiry is ruled on against the instant the caller passes, not a sleep.
    #[test]
    fn an_expired_token_is_refused_and_a_live_one_is_not() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let minted_at = SystemTime::now();
        let token =
            mint_join_token_at(&id, &key, &control_audience(), &grant(), minted_at).expect("mints");
        let public = key.verifying_key_bytes();
        let past_expiry = minted_at + Duration::from_secs(300) + CLOCK_SKEW_TOLERANCE
            + Duration::from_secs(1);
        assert_eq!(
            verify_join_token(&token, &public, &control_audience(), past_expiry),
            Err(JoinTokenRefusal::Expired)
        );
        // Inside the window, including inside the skew tolerance: accepted.
        assert!(
            verify_join_token(
                &token,
                &public,
                &control_audience(),
                minted_at + Duration::from_secs(299)
            )
            .is_ok()
        );
        // A clock far enough BEHIND the mint sees an `iat` in the future.
        assert_eq!(
            verify_join_token(
                &token,
                &public,
                &control_audience(),
                minted_at - CLOCK_SKEW_TOLERANCE - Duration::from_secs(5)
            ),
            Err(JoinTokenRefusal::Expired)
        );
    }

    /// A grant the verifier would refuse is refused at the MINT, so a bad
    /// `--uses` or `--ttl` fails where the operator typed it.
    #[test]
    fn the_minter_refuses_what_the_verifier_would() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let audience = control_audience();
        for bad in [
            JoinTokenGrant { uses: 0, ..grant() },
            JoinTokenGrant {
                uses: MAX_JOIN_TOKEN_USES + 1,
                ..grant()
            },
            JoinTokenGrant {
                lifetime: Duration::ZERO,
                ..grant()
            },
            JoinTokenGrant {
                lifetime: MAX_JOIN_TOKEN_LIFETIME + Duration::from_secs(1),
                ..grant()
            },
            JoinTokenGrant {
                zone: String::new(),
                ..grant()
            },
            JoinTokenGrant {
                zone: " default".to_owned(),
                ..grant()
            },
        ] {
            assert!(
                mint_join_token(&id, &key, &audience, &bad).is_err(),
                "{bad:?} must not mint"
            );
        }
        // The control: the unchanged grant mints.
        assert!(mint_join_token(&id, &key, &audience, &grant()).is_ok());
        // And an id that is not a signer id does not mint either.
        assert!(mint_join_token("wkr_0000000000000000000000001", &key, &audience, &grant()).is_err());
    }

    /// A service assertion is not a join token, whatever else holds about it.
    ///
    /// The signer here mints the assertion with its OWN key, so every other
    /// check this verifier runs would pass. Only `typ` refuses it, which is what
    /// makes the separation the thing being measured rather than an accident of
    /// using the wrong key.
    #[test]
    fn a_service_assertion_from_the_signer_key_is_not_a_join_token() {
        use crate::service_assertion::{ServiceTrustBundle, SERVICE_ASSERTION_TYP};
        let signer = ed25519_dalek::SigningKey::from_bytes(&[11_u8; 32]);
        let public = signer.verifying_key().to_bytes();
        let issuer =
            crate::service_peers::join_signer_issuer(&crate::typed_id::new_join_signer_id())
                .expect("signer issuer");
        let keyring = crate::service_peers::ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::from_pkcs8_der(&{
                use ed25519_dalek::pkcs8::EncodePrivateKey as _;
                signer.to_pkcs8_der().expect("der").as_bytes().to_vec()
            })
            .expect("key"),
            ServiceTrustBundle::new(),
        )
        .expect("keyring");
        let assertion = keyring.mint_for(&control_audience()).expect("mints");
        assert_ne!(SERVICE_ASSERTION_TYP, JOIN_TOKEN_TYP);
        assert_eq!(
            verify_join_token(&assertion, &public, &control_audience(), SystemTime::now()),
            Err(JoinTokenRefusal::Malformed)
        );
    }

    /// `cnf` survives the round trip as the joining key's thumbprint.
    #[test]
    fn a_confirmed_token_carries_the_joining_key_thumbprint() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let joining = ServiceSigningKey::generate();
        let token = mint_join_token(
            &id,
            &key,
            &control_audience(),
            &JoinTokenGrant {
                confirm: Some(joining.verifying_key_bytes()),
                ..grant()
            },
        )
        .expect("mints");
        let verified = verify_join_token(
            &token,
            &key.verifying_key_bytes(),
            &control_audience(),
            SystemTime::now(),
        )
        .expect("verifies");
        assert_eq!(
            verified.confirmation.as_deref(),
            Some(thumbprint_key_id(&joining.verifying_key_bytes()).as_str())
        );
    }

    /// The proof holds for the exact join it was made for, and for no other.
    #[test]
    fn a_join_proof_binds_the_token_the_key_and_the_port() {
        let joining = ServiceSigningKey::generate();
        let public = joining.verifying_key_bytes();
        let token = "header.payload.signature";
        let signature = joining.sign_detached(&join_proof_message(token, &public, 8080));
        assert!(verify_join_proof(&public, token, 8080, &signature));

        // One variable at a time: another token, another port, another key.
        assert!(!verify_join_proof(&public, "other.token.value", 8080, &signature));
        assert!(!verify_join_proof(&public, token, 8081, &signature));
        let other = ServiceSigningKey::generate();
        assert!(!verify_join_proof(
            &other.verifying_key_bytes(),
            token,
            8080,
            &signature
        ));
    }

    /// A proof made for another CONTEXT does not verify here, so the domain
    /// separation is doing work rather than decorating the message.
    #[test]
    fn a_signature_over_the_unprefixed_join_is_not_a_proof() {
        let joining = ServiceSigningKey::generate();
        let public = joining.verifying_key_bytes();
        let token = "header.payload.signature";
        let unprefixed = format!("{token}\n{}\n8080", URL_SAFE_NO_PAD.encode(public));
        let signature = joining.sign_detached(unprefixed.as_bytes());
        assert!(!verify_join_proof(&public, token, 8080, &signature));
    }

    /// The renewal schedule is DERIVED from the lease, so the two cannot be
    /// configured into disagreement.
    ///
    /// This is the statement the worker's renewal loop rests on: after one
    /// failed attempt there is still at least one whole interval of lease left,
    /// so a transient refusal does not cost the identity.
    #[test]
    fn several_renewal_attempts_fit_inside_one_lease() {
        let interval = instance_renewal_interval();
        assert!(!interval.is_zero());
        assert!(
            interval * INSTANCE_RENEWALS_PER_LEASE <= INSTANCE_LEASE_TTL,
            "the derived schedule must not outrun the lease it is derived from"
        );
        // The margin a failed attempt is retried inside is the whole lease
        // minus the first interval, which is at least one more interval.
        let margin = INSTANCE_LEASE_TTL
            .checked_sub(interval)
            .expect("the lease outlasts one renewal interval");
        assert!(margin >= interval);
    }

    /// The unverified signer read admits exactly what it should select a key
    /// for, and nothing else.
    #[test]
    fn the_unverified_signer_read_admits_only_a_signer_identifier() {
        let key = ServiceSigningKey::generate();
        let id = crate::typed_id::new_join_signer_id();
        let token = mint_join_token(&id, &key, &control_audience(), &grant()).expect("mints");
        assert_eq!(unverified_join_signer_id(&token).as_deref(), Some(id.as_str()));
        for bad in ["", "not.a.jwt", "one.two", "a.b.c.d"] {
            assert_eq!(unverified_join_signer_id(bad), None, "{bad:?}");
        }
        // A token whose `iss` names a different role selects no signer key.
        let elsewhere = crate::service_peers::ServiceKeyring::from_parts(
            service_issuer(crate::service_peers::GATEWAY_SERVICE_NAME).expect("issuer"),
            ServiceSigningKey::generate(),
            crate::service_assertion::ServiceTrustBundle::new(),
        )
        .expect("keyring")
        .mint_for(&control_audience())
        .expect("mints");
        assert_eq!(unverified_join_signer_id(&elsewhere), None);
    }
}
