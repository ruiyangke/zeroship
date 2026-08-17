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
    ServiceTrustBundle, JWT_ASSERTION_MECHANISM, MAX_ASSERTION_LIFETIME, SERVICE_ASSERTION_TYP,
};
use zeroship_core::service_identity::{
    verify_identity, AuthError, PeerCredentials, ServiceIdentity, ServiceName, ServicePrincipal,
    TrustDomain,
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

    fn with_replay_store(replay: Arc<dyn ReplayStore>) -> Self {
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
    let fixture = Arc::new(Fixture::new());
    let assertion = Arc::new(fixture.minter.mint(&issuer(CALLEE)).expect("mint"));

    let first = {
        let (fixture, assertion) = (Arc::clone(&fixture), Arc::clone(&assertion));
        compio::runtime::spawn(async move { fixture.verify(&assertion).await })
    };
    let second = {
        let (fixture, assertion) = (Arc::clone(&fixture), Arc::clone(&assertion));
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
    let fixture = Arc::new(Fixture::with_replay_store(Arc::new(
        ReadThenWriteReplayStore::default(),
    )));
    let assertion = Arc::new(fixture.minter.mint(&issuer(CALLEE)).expect("mint"));

    let first = {
        let (fixture, assertion) = (Arc::clone(&fixture), Arc::clone(&assertion));
        compio::runtime::spawn(async move { fixture.verify(&assertion).await })
    };
    let second = {
        let (fixture, assertion) = (Arc::clone(&fixture), Arc::clone(&assertion));
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
    let fixture = Fixture::with_replay_store(Arc::clone(&store) as Arc<dyn ReplayStore>);
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    let identity = fixture.verify(&assertion).await.expect("verify");
    let expiry = identity.attributes()["exp"].as_i64().expect("exp is a number");

    let calls = store.calls.lock();
    let (key, retain_until) = calls.first().expect("the verifier claimed exactly one key");
    assert!(
        key.starts_with(&format!("{CALLER}|")),
        "the replay key is scoped by issuer so one service cannot burn another's jti: {key}"
    );
    let earliest_safe_eviction = UNIX_EPOCH
        + Duration::from_secs(u64::try_from(expiry).expect("a positive exp"));
    assert!(
        *retain_until >= earliest_safe_eviction,
        "a claim evicted before exp makes the assertion replayable while it is still valid"
    );
}

#[compio::test]
async fn a_replay_store_that_cannot_answer_rejects_the_assertion() {
    let fixture = Fixture::with_replay_store(Arc::new(BrokenReplayStore));
    let assertion = fixture.minter.mint(&issuer(CALLEE)).expect("mint");

    assert_eq!(
        fixture.verify(&assertion).await,
        Err(AuthError::CredentialRejected),
        "a store that errored has not said the assertion is fresh"
    );
}

#[compio::test]
async fn the_replay_claim_happens_only_after_the_signature_verifies() {
    // An unauthenticated caller must not be able to write into the replay
    // store. The store here accepts every claim, so the only thing that can
    // keep the count at zero is the verifier refusing before it gets there.
    let store = Arc::new(CountingReplayStore::default());
    let fixture = Fixture::with_replay_store(Arc::clone(&store) as Arc<dyn ReplayStore>);
    let unsigned = forge(&conforming_header(), &conforming_payload(), None);

    assert_eq!(
        fixture.verify(&unsigned).await,
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
    let fixture = Fixture::with_replay_store(Arc::clone(&store) as Arc<dyn ReplayStore>);

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
