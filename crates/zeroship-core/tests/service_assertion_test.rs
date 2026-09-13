//! The security properties of the JWT service-assertion mechanism.
//!
//! Every attack case in this file is paired with a CONTROL that differs from it
//! in exactly one variable and is expected to pass. A rejection test on its own
//! proves only that the verifier said no; it does not prove the verifier said no
//! for the reason the test is named after, because
//! [`AuthError::CredentialRejected`] is deliberately the only rejection the
//! verifier can return - it is not an oracle. The pair is what pins the cause:
//! same builder, same key, same clock, one field changed.
//!
//! What the pairing still cannot prove is that the named GUARD is the thing
//! doing the rejecting. That is established by mutation instead - break one
//! guard, watch the matching test and only that test go red, restore it - and
//! the transcripts live in the change's report, not here.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use jsonwebtoken::{decode_header, Algorithm, EncodingKey, Header};
use rand::RngCore as _;
use serde_json::{json, Value};

use zeroship_core::service_assertion::{
    AssertionError, ClaimFuture, InMemoryReplayStore, ReplayClaim, ReplayStore, ReplayStoreError,
    ServiceAssertionMinter, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
    ServiceTrustBundle, TransportAssertionVerifier, CLOCK_SKEW_TOLERANCE,
    JWT_ASSERTION_MECHANISM, JWT_ASSERTION_TRANSPORT_MECHANISM, MAX_ASSERTION_LIFETIME,
    MAX_JTI_LEN, MAX_REPLAY_STORE_CLOCK_SKEW, SERVICE_ASSERTION_TYP,
};
use zeroship_core::service_identity::{
    authorize, endpoints, verify_identity, AuthError, PeerCredentials, ServiceIdentity,
    ServiceName, ServicePrincipal, TrustDomain,
};

const CALLER: &str = "spiffe://zeroship.ai/svc/gateway";
const CALLEE: &str = "spiffe://zeroship.ai/svc/control";
const THIRD_PARTY: &str = "spiffe://zeroship.ai/svc/worker";
const CALLER_KID: &str = "gateway-2026-08";

// ─── Fixtures ────────────────────────────────────────────────────────────

/// One ed25519 key, in every form the tests need: the typed wrapper for the
/// minter, the raw public bytes for the trust bundle, and a `jsonwebtoken`
/// encoding key for hand-forged tokens the minter would never produce.
struct TestKey {
    der: Vec<u8>,
    public: [u8; 32],
}

impl TestKey {
    fn generate() -> Self {
        let mut seed = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        Self {
            der: signing
                .to_pkcs8_der()
                .expect("encode a generated ed25519 key as PKCS#8")
                .as_bytes()
                .to_vec(),
            public: signing.verifying_key().to_bytes(),
        }
    }

    fn service_key(&self) -> ServiceSigningKey {
        ServiceSigningKey::from_pkcs8_der(&self.der).expect("load the PKCS#8 key back")
    }

    fn encoding_key(&self) -> EncodingKey {
        EncodingKey::from_ed_der(&self.der)
    }
}

fn issuer(uri: &str) -> ServiceIssuer {
    ServiceIssuer::parse(uri).expect("a well-formed service issuer identifier")
}

/// A gateway minter, and a control verifier that trusts exactly that gateway.
struct Fixture {
    caller_key: TestKey,
    minter: ServiceAssertionMinter,
    verifier: ServiceAssertionVerifier,
}

impl Fixture {
    fn new() -> Self {
        Self::with_replay_store(Arc::new(InMemoryReplayStore::new()))
    }

    fn with_replay_store(replay: Arc<dyn ReplayStore + Send + Sync>) -> Self {
        let caller_key = TestKey::generate();
        let minter = ServiceAssertionMinter::new(
            issuer(CALLER),
            CALLER_KID,
            &caller_key.service_key(),
        )
        .expect("build a minter from a freshly generated key");
        let mut bundle = ServiceTrustBundle::new();
        bundle
            .trust(&issuer(CALLER), CALLER_KID, caller_key.public)
            .expect("trust the caller's key");
        Self {
            caller_key,
            minter,
            verifier: ServiceAssertionVerifier::new(bundle, replay),
        }
    }

    async fn verify(&self, assertion: &str) -> Result<ServiceIdentity, AuthError> {
        let observed = PeerCredentials::new(Some(assertion), None, CALLEE);
        verify_identity(&self.verifier, &observed).await
    }

    async fn verify_with_audience(
        &self,
        assertion: &str,
        audience: &str,
    ) -> Result<ServiceIdentity, AuthError> {
        let observed = PeerCredentials::new(Some(assertion), None, audience);
        verify_identity(&self.verifier, &observed).await
    }
}

fn caller_principal() -> ServicePrincipal {
    ServicePrincipal::new(
        TrustDomain::new("zeroship.ai"),
        ServiceName::new("svc/gateway"),
    )
}

fn now_secs() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after the Unix epoch")
            .as_secs(),
    )
    .expect("a Unix timestamp inside i64")
}

/// Assemble a token from a literal header and payload, signing it with `key`.
///
/// The minter cannot produce any of the shapes these tests need to reject - it
/// always writes the right `typ`, the right algorithm, and a `jti` - so the
/// attacker's side of every pair is built here instead.
fn forge(header: &Value, payload: &Value, key: Option<(&EncodingKey, Algorithm)>) -> String {
    let encoded_payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(payload).expect("a serialisable JWT payload"),
    );
    match key {
        None => {
            let encoded_header = URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(header).expect("a serialisable JWT header"),
            );
            format!("{encoded_header}.{encoded_payload}.")
        }
        Some((key, algorithm)) => {
            let mut jwt_header = Header::new(algorithm);
            jwt_header.typ = header
                .get("typ")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            jwt_header.kid = header
                .get("kid")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            jsonwebtoken::encode(&jwt_header, payload, key).expect("sign a forged token")
        }
    }
}

/// The claim set a conforming assertion carries, so a test can change one field.
fn conforming_payload() -> Value {
    let issued = now_secs();
    json!({
        "iss": CALLER,
        "sub": CALLER,
        "aud": CALLEE,
        "iat": issued,
        "exp": issued + 60,
        "jti": "conforming-jti-0001",
    })
}

fn conforming_header() -> Value {
    json!({ "typ": SERVICE_ASSERTION_TYP, "kid": CALLER_KID })
}

// ─── Recording and adversarial replay stores ─────────────────────────────

/// Records every claim so a test can assert on the retention window.
#[derive(Default)]
struct RecordingReplayStore {
    inner: InMemoryReplayStore,
    calls: parking_lot::Mutex<Vec<(String, SystemTime)>>,
}

impl std::fmt::Debug for RecordingReplayStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("RecordingReplayStore").finish()
    }
}

impl ReplayStore for RecordingReplayStore {
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a> {
        self.calls.lock().push((key.to_owned(), expires_at));
        self.inner.claim(key, expires_at)
    }
}

/// Always fails. A store that cannot answer has not said the assertion is fresh.
#[derive(Debug)]
struct BrokenReplayStore;

impl ReplayStore for BrokenReplayStore {
    fn claim<'a>(&'a self, _key: &'a str, _expires_at: SystemTime) -> ClaimFuture<'a> {
        Box::pin(async { Err(ReplayStoreError("connection refused".to_owned())) })
    }
}

/// Counts claims without ever refusing one.
#[derive(Debug, Default)]
struct CountingReplayStore {
    claims: parking_lot::Mutex<usize>,
}

impl ReplayStore for CountingReplayStore {
    fn claim<'a>(&'a self, _key: &'a str, _expires_at: SystemTime) -> ClaimFuture<'a> {
        *self.claims.lock() += 1;
        Box::pin(async { Ok(ReplayClaim::Accepted) })
    }
}

/// The WRONG way to write a replay store: read, yield, then write.
///
/// This is not a store anything ships. It is the negative control for the
/// concurrency test. A race test that cannot tell a read-then-write store apart
/// from an atomic one proves nothing about atomicity - it may simply never have
/// interleaved the two verifications. This store makes that interleaving
/// observable: under a harness that really does run the two verifications
/// concurrently, it admits BOTH, and the atomic store under the same harness
/// admits one.
#[derive(Debug, Default)]
struct ReadThenWriteReplayStore {
    claimed: parking_lot::Mutex<BTreeMap<String, SystemTime>>,
}

impl ReplayStore for ReadThenWriteReplayStore {
    fn claim<'a>(&'a self, key: &'a str, expires_at: SystemTime) -> ClaimFuture<'a> {
        Box::pin(async move {
            let seen = self.claimed.lock().contains_key(key);
            compio::runtime::time::sleep(Duration::from_millis(20)).await;
            if seen {
                return Ok(ReplayClaim::AlreadyUsed);
            }
            self.claimed.lock().insert(key.to_owned(), expires_at);
            Ok(ReplayClaim::Accepted)
        })
    }
}

// ─── Issuer identifiers, and the audience rule ───────────────────────────

#[test]
fn issuer_identifiers_reject_everything_that_is_not_one() {
    assert_eq!(
        issuer(CALLER).principal(),
        &caller_principal(),
        "a parsed issuer names the same principal the allowlist is written against"
    );

    // The ADMITTED shapes, first, so the refusals below are a statement about
    // what is wrong with them rather than about a parser that takes nothing.
    // The instance path is what per-instance service identity rests on: a worker
    // instance mints under `svc/worker/<wkr_id>`, and a typed id is base36 with
    // an underscore-joined prefix. If either of these stopped parsing, that
    // identity would need a wire change rather than a name.
    for admitted in [
        "spiffe://zeroship.ai/svc/worker",
        "spiffe://zeroship.ai/svc/worker/wkr_0000000000000000000000001",
    ] {
        let parsed = ServiceIssuer::parse(admitted)
            .unwrap_or_else(|_| panic!("{admitted:?} must parse as a service issuer identifier"));
        assert_eq!(
            parsed.as_str(),
            admitted,
            "an issuer identifier travels on the wire exactly as it was written"
        );
    }

    for malformed in [
        // The shape an early draft of this design proposed for `aud`.
        "control/get_routes",
        // An endpoint URL, which draft-ietf-oauth-rfc7523bis forbids as an
        // audience after the 2025 audience-injection attacks.
        "https://control.zeroship.ai/internal/routes",
        "spiffe://",
        "spiffe://zeroship.ai",
        "spiffe://zeroship.ai/",
        "spiffe:///svc/control",
        "spiffe://zeroship.ai//svc/control",
        "spiffe://zeroship.ai/svc/../control",
        // Userinfo that would let an attacker's host read as a trusted one.
        "spiffe://zeroship.ai@attacker.example/svc/control",
        // The separator the replay-store key is built with.
        "spiffe://zeroship.ai/svc|control",
        "",
    ] {
        assert_eq!(
            ServiceIssuer::parse(malformed),
            Err(AssertionError::MalformedIssuer),
            "{malformed:?} must not parse as a service issuer identifier"
        );
    }

    // Well formed as a URI and still refused, because the parser has to decide
    // WHICH segment is the instance and a deeper path leaves that ambiguous.
    // Nothing in the tree mints one; admitting them would make the arity rule
    // undecidable in exchange for a shape no caller wants.
    for too_deep in [
        "spiffe://zeroship.ai/svc/worker/wkr_0000000000000000000000001/thread-7",
        "spiffe://zeroship.ai/a/b/c/d/e",
        // Fewer segments than a role is the same rule read the other way: admit
        // it and a two-segment path becomes ambiguous between a role and an
        // instance of a one-segment role.
        "spiffe://zeroship.ai/worker",
    ] {
        assert_eq!(
            ServiceIssuer::parse(too_deep),
            Err(AssertionError::IssuerNotRoleOrInstance),
            "{too_deep:?} names neither a role nor one instance of a role, and says so \
             rather than reporting a syntax fault in a legible URI"
        );
    }
}

// ─── Roles and their instances ───────────────────────────────────────────

/// The role a worker instance is one of, and two instances of it.
///
/// The role is the identifier `THIRD_PARTY` also names. Spelled again under its
/// own name because the two are read for different things - one is a service
/// the callee does not expect, the other is the role an instance belongs to -
/// and a test that renamed `THIRD_PARTY` would otherwise silently change what
/// the instance cases are about.
const WORKER_ROLE: &str = "spiffe://zeroship.ai/svc/worker";
const WORKER_INSTANCE: &str =
    "spiffe://zeroship.ai/svc/worker/wkr_0000000000000000000000001";
const OTHER_WORKER_INSTANCE: &str =
    "spiffe://zeroship.ai/svc/worker/wkr_0000000000000000000000002";

fn worker_role_principal() -> ServicePrincipal {
    ServicePrincipal::new(
        TrustDomain::new("zeroship.ai"),
        ServiceName::new("svc/worker"),
    )
}

/// An instance identifier names its ROLE as a principal and ITSELF on the wire.
///
/// The two halves are the whole of the design. Authorization compares
/// principals, so the principal has to be the role or no grant could ever
/// match; every other consumer - the trust-bundle index, `aud` equality, the
/// replay-store key - joins `as_str()`, so that has to stay the full identifier
/// or two instances collapse into one.
#[test]
fn an_instance_issuer_names_its_role_and_keeps_its_own_identifier() {
    let role = issuer(WORKER_ROLE);
    let instance = issuer(WORKER_INSTANCE);
    let sibling = issuer(OTHER_WORKER_INSTANCE);

    assert_eq!(
        instance.principal(),
        &worker_role_principal(),
        "an instance of a role is that role's principal, which is what the allowlist holds"
    );
    assert_eq!(
        role.principal(),
        instance.principal(),
        "a role and an instance of it are one principal"
    );
    assert_eq!(
        sibling.principal(),
        instance.principal(),
        "two instances of one role are one principal"
    );

    assert_eq!(
        instance.as_str(),
        WORKER_INSTANCE,
        "the identifier travels on the wire exactly as it was written"
    );
    assert_ne!(
        instance, sibling,
        "one principal, and still two identifiers: an instance is distinguishable"
    );
    assert_ne!(
        instance, role,
        "the instance identifier is the finer of the two"
    );

    assert_eq!(
        instance.instance(),
        Some("wkr_0000000000000000000000001"),
        "the instance segment is reachable on its own, away from authorization"
    );
    assert_eq!(
        sibling.instance(),
        Some("wkr_0000000000000000000000002"),
        "and it is the segment this identifier carries, not the other one's"
    );
    assert_eq!(role.instance(), None, "a role is an instance of nothing");
}

/// An instance authorizes on its ROLE's grants, end to end.
///
/// Minted under the instance name, verified as a credential, and put to
/// `authorize` - not a unit test of the parser. Paired with the one-variable
/// control below, because a true arm on its own cannot tell a working grant
/// from a table that authorizes everything.
#[compio::test]
async fn an_instance_issuer_authorizes_on_the_grants_of_its_role() {
    let key = TestKey::generate();
    let key_id = "worker-instance-kid";
    let minter = ServiceAssertionMinter::new(issuer(WORKER_INSTANCE), key_id, &key.service_key())
        .expect("a minter on the instance key");
    let mut bundle = ServiceTrustBundle::new();
    bundle
        .trust(&issuer(WORKER_INSTANCE), key_id, key.public)
        .expect("control trusts this instance's key under the instance issuer");
    let verifier = ServiceAssertionVerifier::new(bundle, Arc::new(InMemoryReplayStore::new()));

    let assertion = minter.mint(&issuer(CALLEE)).expect("mint for control");
    let observed = PeerCredentials::new(Some(&assertion), None, CALLEE);
    let identity = verify_identity(&verifier, &observed)
        .await
        .expect("an instance's assertion verifies against the key published for it");

    assert!(
        identity.matches_principal(&worker_role_principal()),
        "the verified principal is the ROLE, so the allowlist row for it applies"
    );
    assert!(
        authorize(&identity, endpoints::CONTROL_VERSIONS),
        "an instance holds the grants of svc/worker"
    );
    // CONTROL, differing in one variable: an endpoint the role does NOT hold.
    // Without it this test would pass just as well against a table that granted
    // every endpoint to every principal.
    assert!(
        !authorize(&identity, endpoints::CONTROL_ROUTES),
        "an instance holds the grants of its role and no others"
    );
}

/// Two instances of one role claim DIFFERENT replay keys.
///
/// The replay key is `<iss>|<jti>`, and `iss` is the full identifier rather
/// than the principal. If the two collapsed onto one key, an assertion captured
/// from one instance would be accepted as the other's - and worse, the first
/// instance's claim would burn the second's `jti` space.
#[compio::test]
async fn two_instances_of_one_role_claim_distinct_replay_keys() {
    let store = Arc::new(RecordingReplayStore::default());
    let mut bundle = ServiceTrustBundle::new();
    let mut minters = Vec::new();
    for name in [WORKER_INSTANCE, OTHER_WORKER_INSTANCE] {
        let key = TestKey::generate();
        let key_id = format!("{name}-kid");
        bundle
            .trust(&issuer(name), key_id.clone(), key.public)
            .expect("trust each instance under its own identifier");
        minters.push(
            ServiceAssertionMinter::new(issuer(name), key_id, &key.service_key())
                .expect("a minter per instance"),
        );
    }
    let verifier = ServiceAssertionVerifier::new(
        bundle,
        Arc::clone(&store) as Arc<dyn ReplayStore + Send + Sync>,
    );

    for minter in &minters {
        let assertion = minter.mint(&issuer(CALLEE)).expect("mint for control");
        let observed = PeerCredentials::new(Some(&assertion), None, CALLEE);
        verify_identity(&verifier, &observed)
            .await
            .expect("each instance's assertion verifies");
    }

    let calls = store.calls.lock();
    let keys: Vec<&str> = calls.iter().map(|(key, _)| key.as_str()).collect();
    assert_eq!(keys.len(), 2, "one claim per verification");
    assert!(
        keys[0].starts_with(&format!("{WORKER_INSTANCE}|")),
        "the key is scoped by the full instance identifier: {}",
        keys[0]
    );
    assert!(
        keys[1].starts_with(&format!("{OTHER_WORKER_INSTANCE}|")),
        "the key is scoped by the full instance identifier: {}",
        keys[1]
    );
    assert_ne!(
        keys[0], keys[1],
        "two instances must not share a replay key, or one replays as the other"
    );
}

#[compio::test]
async fn the_expected_audience_must_be_an_issuer_identifier_not_an_endpoint() {
    let fixture = Fixture::new();
    let assertion = fixture
        .minter
        .mint(&issuer(CALLEE))
        .expect("mint an assertion for the callee");

    // CONTROL: the callee's issuer identifier.
    assert!(fixture.verify(&assertion).await.is_ok());

    // Every audience a caller might reach for that is NOT an issuer
    // identifier is refused, so `aud: "control/get_routes"` cannot be revived
    // by a caller passing it in rather than by the minter emitting it.
    for endpoint in [
        "control/get_routes",
        "https://control.zeroship.ai/internal/routes",
        "control.zeroship.ai",
    ] {
        let forged = forge(
            &conforming_header(),
            &json!({
                "iss": CALLER, "sub": CALLER, "aud": endpoint,
                "iat": now_secs(), "exp": now_secs() + 60, "jti": "endpoint-aud-0001",
            }),
            Some((&fixture.caller_key.encoding_key(), Algorithm::EdDSA)),
        );
        assert_eq!(
            fixture.verify_with_audience(&forged, endpoint).await,
            Err(AuthError::CredentialRejected),
            "an endpoint-shaped audience must be refused: {endpoint}"
        );
    }
}

#[compio::test]
async fn an_assertion_for_another_callee_is_rejected() {
    let fixture = Fixture::new();

    // CONTROL: minted for this callee.
    let mine = fixture.minter.mint(&issuer(CALLEE)).expect("mint for the callee");
    assert!(fixture.verify(&mine).await.is_ok());

    // Same caller, same key, one field changed: minted for somebody else.
    let theirs = fixture
        .minter
        .mint(&issuer(THIRD_PARTY))
        .expect("mint for a third party");
    assert_eq!(
        fixture.verify(&theirs).await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn the_neutral_identity_carries_mechanism_facts_and_never_the_audience() {
    let fixture = Fixture::new();
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    let identity = fixture.verify(&assertion).await.expect("a conforming assertion verifies");

    assert!(identity.matches_principal(&caller_principal()));
    assert_eq!(identity.mechanism().as_ref(), JWT_ASSERTION_MECHANISM);

    let attributes = identity.attributes();
    assert_eq!(attributes["kid"], json!(CALLER_KID));
    assert!(attributes.contains_key("exp"));
    assert!(attributes.contains_key("jti"));
    assert_eq!(
        attributes.keys().collect::<Vec<_>>(),
        vec!["exp", "jti", "kid"],
        "mechanism facts and nothing else"
    );
    // `aud` is verifier INPUT. Carrying it out would force any later mechanism
    // without an audience concept to fabricate one.
    assert!(!attributes.contains_key("aud"));
    let rendered = format!("{identity:?}");
    assert!(
        !rendered.contains(CALLEE),
        "the expected audience must not survive into the neutral output: {rendered}"
    );
}

// ─── jti: required, and single use ───────────────────────────────────────

#[compio::test]
async fn an_assertion_without_a_jti_is_rejected() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    // CONTROL: identical claim set, with a jti.
    let with_jti = forge(
        &conforming_header(),
        &conforming_payload(),
        Some((&signing, Algorithm::EdDSA)),
    );
    assert!(fixture.verify(&with_jti).await.is_ok());

    let mut without_jti = conforming_payload();
    without_jti
        .as_object_mut()
        .expect("the conforming payload is a JSON object")
        .remove("jti");
    let forged = forge(
        &conforming_header(),
        &without_jti,
        Some((&signing, Algorithm::EdDSA)),
    );
    assert_eq!(
        fixture.verify(&forged).await,
        Err(AuthError::CredentialRejected),
        "bare RFC 7523 lists jti as MAY; this profile makes a missing one fatal"
    );
}

#[compio::test]
async fn a_jti_outside_the_length_and_charset_bound_is_rejected() {
    // The `jti` is the only attacker-controlled string that reaches the replay
    // store, joined to the issuer as `<iss>|<jti>`. Two properties therefore
    // have to hold and neither had a test: it is BOUNDED, so a hostile caller
    // cannot grow store keys without limit, and it excludes the `|` the key is
    // built with, so it cannot forge a key that reads as another issuer's.
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();
    let with_jti = |jti: String| {
        let mut payload = conforming_payload();
        payload["jti"] = json!(jti);
        forge(&conforming_header(), &payload, Some((&signing, Algorithm::EdDSA)))
    };

    // CONTROL: exactly at the bound, and the only difference from the first
    // rejection below is one character of length.
    assert!(
        fixture.verify(&with_jti("a".repeat(MAX_JTI_LEN))).await.is_ok(),
        "a jti of exactly MAX_JTI_LEN is admitted, so the boundary is where it says it is"
    );

    for hostile in [
        "a".repeat(MAX_JTI_LEN + 1),
        "a".repeat(4096),
        // The separator. Without the charset guard this claims the key
        // `spiffe://zeroship.ai/svc/gateway|spiffe://...` - a key belonging to
        // whatever issuer the caller names after the pipe.
        format!("{THIRD_PARTY}|stolen"),
        "has spaces".to_owned(),
        "dots.are.not.admitted".to_owned(),
        String::new(),
    ] {
        assert_eq!(
            fixture.verify(&with_jti(hostile.clone())).await,
            Err(AuthError::CredentialRejected),
            "a jti of {} characters starting {:?} must be refused",
            hostile.len(),
            hostile.chars().take(16).collect::<String>()
        );
    }
}

#[compio::test]
async fn a_verified_assertion_cannot_be_verified_a_second_time() {
    let fixture = Fixture::new();
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    assert!(fixture.verify(&assertion).await.is_ok());
    assert_eq!(
        fixture.verify(&assertion).await,
        Err(AuthError::CredentialRejected)
    );

    // CONTROL: a fresh assertion from the same minter still verifies, so the
    // first rejection is about this jti and not about the verifier latching.
    let fresh = fixture.minter.mint(&issuer(CALLEE)).expect("mint again");
    assert!(fixture.verify(&fresh).await.is_ok());
}

#[compio::test]
async fn concurrent_verifications_of_one_assertion_admit_exactly_one() {
    // `Rc`, not `Arc`: compio is thread-per-core, so both spawned tasks run on
    // this thread and an atomic refcount would buy nothing.
    let fixture = Rc::new(Fixture::new());
    let assertion = Arc::new(fixture.minter.mint(&issuer(CALLEE)).expect("mint"));

    let first = {
        let (fixture, assertion) = (Rc::clone(&fixture), Arc::clone(&assertion));
        compio::runtime::spawn(async move { fixture.verify(&assertion).await })
    };
    let second = {
        let (fixture, assertion) = (Rc::clone(&fixture), Arc::clone(&assertion));
        compio::runtime::spawn(async move { fixture.verify(&assertion).await })
    };
    let accepted = [first.await.expect("first task"), second.await.expect("second task")]
        .into_iter()
        .filter(Result::is_ok)
        .count();

    assert_eq!(accepted, 1, "exactly one racer may claim the jti");
}

#[compio::test]
async fn the_race_harness_can_tell_a_read_then_write_store_apart() {
    // The negative control for the test above. Same two concurrent
    // verifications, same single assertion, one variable changed: a store that
    // reads, yields, and only then writes. It admits BOTH, which is what proves
    // the harness genuinely interleaves - and therefore that the single
    // acceptance above is the store's atomicity and not a serialised harness.
    let fixture = Rc::new(Fixture::with_replay_store(Arc::new(
        ReadThenWriteReplayStore::default(),
    )));
    let assertion = Arc::new(fixture.minter.mint(&issuer(CALLEE)).expect("mint"));

    let first = {
        let (fixture, assertion) = (Rc::clone(&fixture), Arc::clone(&assertion));
        compio::runtime::spawn(async move { fixture.verify(&assertion).await })
    };
    let second = {
        let (fixture, assertion) = (Rc::clone(&fixture), Arc::clone(&assertion));
        compio::runtime::spawn(async move { fixture.verify(&assertion).await })
    };
    let accepted = [first.await.expect("first task"), second.await.expect("second task")]
        .into_iter()
        .filter(Result::is_ok)
        .count();

    assert_eq!(
        accepted, 2,
        "a read-then-write store must lose this race, or the harness is not racing"
    );
}

#[compio::test]
async fn a_replay_claim_is_retained_for_the_whole_acceptance_window() {
    let store = Arc::new(RecordingReplayStore::default());
    let fixture = Fixture::with_replay_store(Arc::clone(&store) as Arc<dyn ReplayStore + Send + Sync>);
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    let identity = fixture.verify(&assertion).await.expect("verify");
    let expiry = identity.attributes()["exp"].as_i64().expect("exp is a number");

    let calls = store.calls.lock();
    let (key, retain_until) = calls.first().expect("the verifier claimed exactly one key");
    assert!(
        key.starts_with(&format!("{CALLER}|")),
        "the replay key is scoped by issuer so one service cannot burn another's jti: {key}"
    );
    // `exp` alone is not the edge. This verifier keeps accepting until
    // `exp + CLOCK_SKEW_TOLERANCE` on its own clock, and the store decides
    // reclaimability on the DATABASE's clock, which may run ahead by up to
    // MAX_REPLAY_STORE_CLOCK_SKEW. Both terms are load-bearing and neither was
    // asserted: the previous bound was `>= exp`, so deleting the `+ leeway`
    // from the source left the whole suite green.
    let earliest_safe_eviction = UNIX_EPOCH
        + Duration::from_secs(u64::try_from(expiry).expect("a positive exp"))
        + CLOCK_SKEW_TOLERANCE
        + MAX_REPLAY_STORE_CLOCK_SKEW;
    assert!(
        *retain_until >= earliest_safe_eviction,
        "a claim evicted while any clock still accepts the assertion makes it replayable: \
         retained to {retain_until:?}, needed {earliest_safe_eviction:?}"
    );
}

#[compio::test]
async fn a_replay_store_that_cannot_answer_rejects_the_assertion() {
    let fixture = Fixture::with_replay_store(Arc::new(BrokenReplayStore));
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    // FAIL CLOSED, and say which failure it was. The refusal is the property
    // that matters and is asserted first; the variant is asserted because an
    // operator paged for a store outage and one alerted on a rise in rejected
    // credentials are looking at different incidents.
    let outcome = fixture.verify(&assertion).await;
    assert!(
        outcome.is_err(),
        "a store that errored has not said the assertion is fresh"
    );
    assert_eq!(
        outcome,
        Err(AuthError::StoreUnavailable),
        "a store outage must not be indistinguishable from a rejected credential"
    );

    // CONTROL: the same verifier shape over a store that answers refuses a
    // REPLAY with the other variant, so the two are really distinguished and
    // not merely renamed.
    let working = Fixture::new();
    let assertion = working.minter.mint(&issuer(CALLEE)).expect("mint");
    assert!(working.verify(&assertion).await.is_ok());
    assert_eq!(
        working.verify(&assertion).await,
        Err(AuthError::CredentialRejected),
        "a replayed jti is a rejected credential, not an unavailable store"
    );
}

#[compio::test]
async fn the_replay_claim_happens_only_after_the_signature_verifies() {
    // An unauthenticated caller must not be able to write into the replay
    // store. The store here accepts every claim, so the only thing that can
    // keep the count at zero is the verifier refusing before it gets there.
    //
    // The token is signed by a key the bundle does not hold, under the kid and
    // typ a real assertion carries. That matters: it is well-formed all the way
    // to the signature, so the verifier really does select a key and really
    // does fail the cryptographic check, which is the step the ordering claim
    // is about. An earlier version forged with no key at all, whose header
    // carries no `alg` and so dies in `decode_header` before key selection - it
    // would have stayed green even if the claim were moved ahead of signature
    // verification but left behind header parsing.
    let store = Arc::new(CountingReplayStore::default());
    let fixture = Fixture::with_replay_store(Arc::clone(&store) as Arc<dyn ReplayStore + Send + Sync>);
    let untrusted = TestKey::generate();
    let forged = forge(
        &conforming_header(),
        &conforming_payload(),
        Some((&untrusted.encoding_key(), Algorithm::EdDSA)),
    );

    assert_eq!(
        fixture.verify(&forged).await,
        Err(AuthError::CredentialRejected)
    );
    assert_eq!(*store.claims.lock(), 0);

    // CONTROL: a real assertion does reach the store.
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");
    assert!(fixture.verify(&assertion).await.is_ok());
    assert_eq!(*store.claims.lock(), 1);
}

// ─── Lifetime ceiling and clock skew ─────────────────────────────────────

#[compio::test]
async fn an_assertion_without_an_exp_is_rejected() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    let mut without_exp = conforming_payload();
    without_exp
        .as_object_mut()
        .expect("a JSON object")
        .remove("exp");
    assert_eq!(
        fixture
            .verify(&forge(
                &conforming_header(),
                &without_exp,
                Some((&signing, Algorithm::EdDSA))
            ))
            .await,
        Err(AuthError::CredentialRejected)
    );

    // CONTROL: the same claim set with exp restored.
    assert!(fixture
        .verify(&forge(
            &conforming_header(),
            &conforming_payload(),
            Some((&signing, Algorithm::EdDSA))
        ))
        .await
        .is_ok());
}

#[compio::test]
async fn an_assertion_whose_lifetime_exceeds_the_ceiling_is_rejected() {
    let fixture = Fixture::new();

    // CONTROL: the minter's default lifetime is exactly the ceiling.
    let at_ceiling = fixture.minter.mint(&issuer(CALLEE)).expect("mint");
    assert!(fixture.verify(&at_ceiling).await.is_ok());

    // A self-signed assertion means the CALLER chose this exp.
    let long_lived = ServiceAssertionMinter::new(
        issuer(CALLER),
        CALLER_KID,
        &fixture.caller_key.service_key(),
    )
    .expect("build a minter")
    .with_lifetime(Duration::from_secs(3600));
    assert_eq!(
        fixture
            .verify(&long_lived.mint(&issuer(CALLEE)).expect("mint a long-lived assertion"))
            .await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn an_assertion_whose_lifetime_is_zero_or_negative_is_rejected() {
    // The ceiling arm is `lifetime <= 0 || lifetime > ceiling`, and only the
    // upper half had a test. The lower half is what keeps `exp == iat` and
    // `exp < iat` out: both are inside the ceiling arithmetically, and both are
    // inside the skew tolerance, so nothing else in the verifier objects to
    // them. They are not assertions - a window that has no interior cannot have
    // been issued honestly, and admitting one would mean the store retained a
    // claim for a credential no clock ever considered live.
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();
    let window = |iat: i64, exp: i64, jti: &str| {
        forge(
            &conforming_header(),
            &json!({
                "iss": CALLER, "sub": CALLER, "aud": CALLEE,
                "iat": iat, "exp": exp, "jti": jti,
            }),
            Some((&signing, Algorithm::EdDSA)),
        )
    };

    let issued = now_secs();
    assert_eq!(
        fixture.verify(&window(issued, issued, "zero-lifetime-0001")).await,
        Err(AuthError::CredentialRejected),
        "exp == iat is a zero-second assertion"
    );
    assert_eq!(
        fixture.verify(&window(issued, issued - 5, "negative-lifetime-0001")).await,
        Err(AuthError::CredentialRejected),
        "exp < iat is a window that never opened"
    );

    // CONTROL: one second of interior, the same clock, everything else equal.
    assert!(
        fixture
            .verify(&window(issued, issued + 1, "one-second-lifetime-0001"))
            .await
            .is_ok(),
        "the shortest honest window is still admitted"
    );
}

#[compio::test]
async fn an_iat_in_the_future_cannot_shift_the_acceptance_window_forward() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    // `exp - iat` is a well-behaved 30 seconds, so the lifetime ceiling has
    // nothing to object to. The whole window has simply been moved an hour into
    // the future, which would keep the assertion live far longer than a
    // conforming one. Only the `iat` bound catches this shape.
    let far_future = now_secs() + 3600;
    let forged = forge(
        &conforming_header(),
        &json!({
            "iss": CALLER, "sub": CALLER, "aud": CALLEE,
            "iat": far_future - 30, "exp": far_future, "jti": "future-iat-0001",
        }),
        Some((&signing, Algorithm::EdDSA)),
    );
    assert_eq!(
        fixture.verify(&forged).await,
        Err(AuthError::CredentialRejected)
    );

    // CONTROL: the same 30-second window, sitting where it belongs.
    let honest = forge(
        &conforming_header(),
        &json!({
            "iss": CALLER, "sub": CALLER, "aud": CALLEE,
            "iat": now_secs(), "exp": now_secs() + 30, "jti": "present-iat-0001",
        }),
        Some((&signing, Algorithm::EdDSA)),
    );
    assert!(fixture.verify(&honest).await.is_ok());
}

#[compio::test]
async fn a_backdated_iat_cannot_stretch_the_lifetime_past_the_ceiling() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    // The mirror image, and the reason a bound on `exp` alone would not do:
    // `exp` is an ordinary 60 seconds away, but `iat` is an hour back, so the
    // assertion has already been replayable for an hour by the time it arrives.
    let forged = forge(
        &conforming_header(),
        &json!({
            "iss": CALLER, "sub": CALLER, "aud": CALLEE,
            "iat": now_secs() - 3600, "exp": now_secs() + 60, "jti": "backdated-iat-0001",
        }),
        Some((&signing, Algorithm::EdDSA)),
    );
    assert_eq!(
        fixture.verify(&forged).await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn expiry_is_tolerated_to_the_skew_bound_and_not_beyond_it() {
    let fixture = Fixture::new();

    // CONTROL: expired 10 seconds ago, inside the 15-second skew tolerance.
    let just_expired = fixture
        .minter
        .mint_at(
            &issuer(CALLEE),
            SystemTime::now() - MAX_ASSERTION_LIFETIME - Duration::from_secs(10),
        )
        .expect("mint a barely expired assertion");
    assert!(
        fixture.verify(&just_expired).await.is_ok(),
        "15 seconds of skew tolerance is the Keycloak reference number"
    );

    // Expired 40 seconds ago, well past it.
    let long_expired = fixture
        .minter
        .mint_at(
            &issuer(CALLEE),
            SystemTime::now() - MAX_ASSERTION_LIFETIME - Duration::from_secs(40),
        )
        .expect("mint a stale assertion");
    assert_eq!(
        fixture.verify(&long_expired).await,
        Err(AuthError::CredentialRejected)
    );
}

// ─── Key-to-issuer binding: RFC 8725 section 3.8, Storm-0558 ─────────────

#[compio::test]
async fn a_key_bound_to_one_service_does_not_verify_another_services_assertion() {
    let gateway_key = TestKey::generate();
    let worker_key = TestKey::generate();

    let mut bundle = ServiceTrustBundle::new();
    bundle
        .trust(&issuer(CALLER), "shared-kid", gateway_key.public)
        .expect("trust the gateway key");
    bundle
        .trust(&issuer(THIRD_PARTY), "shared-kid", worker_key.public)
        .expect("trust the worker key");
    let verifier =
        ServiceAssertionVerifier::new(bundle, Arc::new(InMemoryReplayStore::new()));

    let verify = |assertion: String| {
        let verifier = &verifier;
        async move {
            let observed = PeerCredentials::new(Some(assertion.as_str()), None, CALLEE);
            verify_identity(verifier, &observed).await
        }
    };

    // CONTROL: the gateway's key signing the gateway's own assertion.
    let honest = ServiceAssertionMinter::new(
        issuer(CALLER),
        "shared-kid",
        &gateway_key.service_key(),
    )
    .expect("build the gateway minter")
    .mint(&issuer(CALLEE))
    .expect("mint");
    assert!(verify(honest).await.is_ok());

    // The same key, the same kid, claiming to be the worker. A verifier that
    // tried every known key - or that matched on kid alone - would accept this.
    // That is Storm-0558.
    let impersonation = ServiceAssertionMinter::new(
        issuer(THIRD_PARTY),
        "shared-kid",
        &gateway_key.service_key(),
    )
    .expect("build an impersonating minter")
    .mint(&issuer(CALLEE))
    .expect("mint");
    assert_eq!(verify(impersonation).await, Err(AuthError::CredentialRejected));
}

#[compio::test]
async fn an_assertion_from_an_issuer_the_bundle_does_not_know_is_rejected() {
    let fixture = Fixture::new();
    let stranger_key = TestKey::generate();
    let stranger = ServiceAssertionMinter::new(
        issuer(THIRD_PARTY),
        "stranger-kid",
        &stranger_key.service_key(),
    )
    .expect("build a stranger's minter")
    .mint(&issuer(CALLEE))
    .expect("mint");

    assert_eq!(
        fixture.verify(&stranger).await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn a_kid_the_issuer_does_not_own_is_rejected() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    let wrong_kid = forge(
        &json!({ "typ": SERVICE_ASSERTION_TYP, "kid": "some-other-key" }),
        &conforming_payload(),
        Some((&signing, Algorithm::EdDSA)),
    );
    assert_eq!(
        fixture.verify(&wrong_kid).await,
        Err(AuthError::CredentialRejected)
    );

    // CONTROL: the same token under the kid the issuer actually holds.
    assert!(fixture
        .verify(&forge(
            &conforming_header(),
            &conforming_payload(),
            Some((&signing, Algorithm::EdDSA))
        ))
        .await
        .is_ok());
}

#[test]
fn a_trust_bundle_will_not_silently_replace_a_key() {
    let first = TestKey::generate();
    let second = TestKey::generate();
    let mut bundle = ServiceTrustBundle::new();

    bundle.trust(&issuer(CALLER), "k1", first.public).expect("first entry");
    // Idempotent: the same key under the same name is not a conflict.
    bundle.trust(&issuer(CALLER), "k1", first.public).expect("repeat entry");
    assert_eq!(
        bundle.trust(&issuer(CALLER), "k1", second.public),
        Err(AssertionError::DuplicateTrustEntry),
        "a later configuration line must not be able to revoke an earlier key by shadowing it"
    );
    // Rotation is expressed by adding, not by replacing.
    bundle.trust(&issuer(CALLER), "k2", second.public).expect("rotation entry");
}

// ─── Token-type confusion, both directions ───────────────────────────────

#[compio::test]
async fn a_user_access_token_cannot_be_presented_as_a_service_assertion() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    // Even signed by a trusted service key, with a perfect claim set, a token
    // typed as anything else is refused.
    for typ in ["JWT", "at+jwt", "logout+jwt"] {
        let access_token = forge(
            &json!({ "typ": typ, "kid": CALLER_KID }),
            &conforming_payload(),
            Some((&signing, Algorithm::EdDSA)),
        );
        assert_eq!(
            fixture.verify(&access_token).await,
            Err(AuthError::CredentialRejected),
            "a {typ} token is not a service assertion"
        );
    }

    // A token with no typ header at all is the most common access-token shape.
    let untyped = forge(
        &json!({ "kid": CALLER_KID }),
        &conforming_payload(),
        Some((&signing, Algorithm::EdDSA)),
    );
    assert_eq!(
        fixture.verify(&untyped).await,
        Err(AuthError::CredentialRejected)
    );

    // CONTROL: one field different - the right typ - and it verifies.
    assert!(fixture
        .verify(&forge(
            &conforming_header(),
            &conforming_payload(),
            Some((&signing, Algorithm::EdDSA))
        ))
        .await
        .is_ok());
}

#[test]
fn a_service_assertion_cannot_be_presented_as_a_user_access_token() {
    let fixture = Fixture::new();
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    let header = decode_header(&assertion).expect("a decodable header");
    assert_eq!(header.typ.as_deref(), Some(SERVICE_ASSERTION_TYP));

    // The other direction of the same defence, exercised the way an access
    // token verifier does it: it requires its own media type and this one is
    // not it. RFC 8725 section 3.11.
    for access_token_typ in ["JWT", "at+jwt"] {
        assert_ne!(
            header.typ.as_deref(),
            Some(access_token_typ),
            "a service assertion must not be accepted where a {access_token_typ} is expected"
        );
    }
}

// ─── Algorithm pinning ───────────────────────────────────────────────────

#[compio::test]
async fn an_alg_none_assertion_is_rejected() {
    let fixture = Fixture::new();

    let none_token = forge(
        &json!({ "alg": "none", "typ": SERVICE_ASSERTION_TYP, "kid": CALLER_KID }),
        &conforming_payload(),
        None,
    );
    assert_eq!(
        fixture.verify(&none_token).await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn algorithm_confusion_against_the_public_key_is_rejected() {
    let fixture = Fixture::new();

    // The classic confusion: present the trusted ed25519 PUBLIC key as an HMAC
    // secret. It is public, so anyone can compute this MAC.
    let public_as_secret = EncodingKey::from_secret(&fixture.caller_key.public);
    let confused = forge(
        &conforming_header(),
        &conforming_payload(),
        Some((&public_as_secret, Algorithm::HS256)),
    );
    assert_eq!(
        fixture.verify(&confused).await,
        Err(AuthError::CredentialRejected)
    );
}

// ─── The framework's own guard stays the framework's ─────────────────────

#[compio::test]
async fn absent_credentials_never_reach_the_verifier() {
    let store = Arc::new(CountingReplayStore::default());
    let fixture = Fixture::with_replay_store(Arc::clone(&store) as Arc<dyn ReplayStore + Send + Sync>);

    for absent in [None, Some("")] {
        let observed = PeerCredentials::new(absent, None, CALLEE);
        assert_eq!(
            verify_identity(&fixture.verifier, &observed).await,
            Err(AuthError::NoCredentialPresented),
            "the framework decides absence; the mechanism never sees it"
        );
    }
    assert_eq!(*store.claims.lock(), 0);
}

#[compio::test]
async fn sub_must_name_the_same_service_as_iss() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    let mut speaking_for_another = conforming_payload();
    speaking_for_another["sub"] = json!(THIRD_PARTY);
    assert_eq!(
        fixture
            .verify(&forge(
                &conforming_header(),
                &speaking_for_another,
                Some((&signing, Algorithm::EdDSA))
            ))
            .await,
        Err(AuthError::CredentialRejected),
        "a service asserts its own identity and nobody else's"
    );

    // CONTROL: sub restored to iss.
    assert!(fixture
        .verify(&forge(
            &conforming_header(),
            &conforming_payload(),
            Some((&signing, Algorithm::EdDSA))
        ))
        .await
        .is_ok());
}

#[compio::test]
async fn a_multi_audience_assertion_is_rejected() {
    let fixture = Fixture::new();
    let signing = fixture.caller_key.encoding_key();

    // draft-ietf-oauth-rfc7523bis mandates the single issuer identifier of the
    // callee, so an assertion good for several callees at once is refused.
    let mut many = conforming_payload();
    many["aud"] = json!([CALLEE, THIRD_PARTY]);
    assert_eq!(
        fixture
            .verify(&forge(
                &conforming_header(),
                &many,
                Some((&signing, Algorithm::EdDSA))
            ))
            .await,
        Err(AuthError::CredentialRejected)
    );
}

// ─── The transport-only profile ──────────────────────────────────────────
//
// Every case below is paired with the FULL profile over the same assertion and
// the same trust bundle, so the one variable is which verifier judged it.

/// Build a transport-only verifier trusting the same caller as `fixture`.
fn transport_verifier(fixture: &Fixture) -> TransportAssertionVerifier {
    let mut bundle = ServiceTrustBundle::new();
    bundle
        .trust(&issuer(CALLER), CALLER_KID, fixture.caller_key.public)
        .expect("trust the caller's key");
    TransportAssertionVerifier::new(bundle)
}

async fn verify_transport(
    verifier: &TransportAssertionVerifier,
    assertion: &str,
    audience: &str,
) -> Result<ServiceIdentity, AuthError> {
    let observed = PeerCredentials::new(Some(assertion), None, audience);
    verify_identity(verifier, &observed).await
}

#[compio::test]
async fn the_transport_profile_admits_a_second_presentation_and_the_full_profile_does_not() {
    let fixture = Fixture::new();
    let transport = transport_verifier(&fixture);
    let assertion = fixture
        .minter
        .mint(&issuer(CALLEE))
        .expect("mint one assertion");

    // FULL: first presentation accepted, second refused. That is the `jti`
    // claim, and it is the whole difference between the two profiles.
    assert!(fixture.verify(&assertion).await.is_ok());
    assert_eq!(
        fixture.verify(&assertion).await,
        Err(AuthError::CredentialRejected)
    );

    // TRANSPORT: the SAME already-burnt assertion is accepted, twice. This is
    // the property the dispatch hop buys by not writing to a shared store, and
    // it is stated as a measured fact rather than left implicit - a reader has
    // to be able to see exactly what the tier gives up.
    assert!(verify_transport(&transport, &assertion, CALLEE)
        .await
        .is_ok());
    assert!(verify_transport(&transport, &assertion, CALLEE)
        .await
        .is_ok());
}

#[compio::test]
async fn the_transport_profile_still_enforces_every_cryptographic_check() {
    let fixture = Fixture::new();
    let transport = transport_verifier(&fixture);
    let signing = fixture.caller_key.encoding_key();

    // The control: a conforming assertion is admitted, so each refusal below
    // differs from an accepted call in exactly one variable.
    assert!(verify_transport(
        &transport,
        &forge(
            &conforming_header(),
            &conforming_payload(),
            Some((&signing, Algorithm::EdDSA))
        ),
        CALLEE
    )
    .await
    .is_ok());

    // Wrong audience: this callee is not the one the assertion names.
    let good = fixture.minter.mint(&issuer(CALLEE)).expect("mint");
    assert_eq!(
        verify_transport(&transport, &good, THIRD_PARTY).await,
        Err(AuthError::CredentialRejected)
    );

    // Untrusted signer: a key the bundle does not carry for this issuer.
    let stranger = TestKey::generate();
    assert_eq!(
        verify_transport(
            &transport,
            &forge(
                &conforming_header(),
                &conforming_payload(),
                Some((&stranger.encoding_key(), Algorithm::EdDSA))
            ),
            CALLEE
        )
        .await,
        Err(AuthError::CredentialRejected)
    );

    // Wrong `typ`: a user access token presented as a service assertion.
    let mut user_token = conforming_header();
    user_token["typ"] = json!("at+jwt");
    assert_eq!(
        verify_transport(
            &transport,
            &forge(
                &user_token,
                &conforming_payload(),
                Some((&signing, Algorithm::EdDSA))
            ),
            CALLEE
        )
        .await,
        Err(AuthError::CredentialRejected)
    );

    // A malformed `jti` is STILL refused. The transport profile does not claim
    // the `jti`; it does not stop requiring one. Dropping the requirement would
    // make the two profiles accept different token shapes, and an assertion
    // minted for one edge could then be unusable on the other.
    let mut no_jti = conforming_payload();
    no_jti.as_object_mut().expect("payload object").remove("jti");
    assert_eq!(
        verify_transport(
            &transport,
            &forge(
                &conforming_header(),
                &no_jti,
                Some((&signing, Algorithm::EdDSA))
            ),
            CALLEE
        )
        .await,
        Err(AuthError::CredentialRejected)
    );
}

#[compio::test]
async fn the_two_profiles_stamp_distinguishable_mechanism_tags() {
    let fixture = Fixture::new();
    let transport = transport_verifier(&fixture);
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    let by_transport = verify_transport(&transport, &assertion, CALLEE)
        .await
        .expect("the transport profile admits it");
    let by_full = fixture
        .verify(&assertion)
        .await
        .expect("the full profile admits it too");

    // Same principal, different mechanism. A downstream check that requires the
    // full profile can therefore say so, instead of trusting that whoever wired
    // the edge picked the right verifier.
    assert!(by_transport.matches_principal(&caller_principal()));
    assert!(by_full.matches_principal(&caller_principal()));
    assert_eq!(
        by_transport.mechanism().as_ref(),
        JWT_ASSERTION_TRANSPORT_MECHANISM
    );
    assert_eq!(by_full.mechanism().as_ref(), JWT_ASSERTION_MECHANISM);
    assert_ne!(JWT_ASSERTION_TRANSPORT_MECHANISM, JWT_ASSERTION_MECHANISM);
}

#[compio::test]
async fn a_transport_verifier_never_touches_its_peers_replay_store() {
    // The store the FULL profile would consult, wired to fail every claim. A
    // transport verification that consulted it would be refused; it is not.
    struct AlwaysUnavailable;
    impl ReplayStore for AlwaysUnavailable {
        fn claim<'a>(&'a self, _key: &'a str, _expires_at: SystemTime) -> ClaimFuture<'a> {
            Box::pin(async { Err(ReplayStoreError("store is down".to_owned())) })
        }
    }

    let fixture = Fixture::with_replay_store(Arc::new(AlwaysUnavailable));
    let transport = transport_verifier(&fixture);
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    assert_eq!(
        fixture.verify(&assertion).await,
        Err(AuthError::StoreUnavailable)
    );
    assert!(verify_transport(&transport, &assertion, CALLEE)
        .await
        .is_ok());
}
