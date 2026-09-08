//! Peer key distribution: what the two configured files must hold, and what a
//! service can do once it holds them.
//!
//! Every refusal here is paired with a control differing in exactly one member
//! of the document, because a loader that refuses everything and a loader that
//! refuses the right thing look identical from a single failing case.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use rand::RngCore as _;

use zeroship_core::service_assertion::{
    thumbprint_key_id, InMemoryReplayStore, ServiceAssertionVerifier, ServiceIssuer,
    ServiceTrustBundle, TransportAssertionVerifier,
};
use zeroship_core::service_identity::{
    endpoints, verify_service_call, AuthError, ServiceName, ServicePrincipal,
    TrustDomain,
};
use zeroship_core::service_peers::{
    load_peer_bundle, load_signing_key, service_issuer, PeerKeyError, ServiceAuth, ServiceKeyring,
    AUTH_SERVICE_NAME, CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, WORKER_SERVICE_NAME,
};

/// A generated ed25519 keypair written to a 0600 PKCS#8 PEM file.
struct KeyFile {
    path: std::path::PathBuf,
    public: [u8; 32],
}

/// Write the key in the exact shape `openssl genpkey -algorithm ed25519`
/// produces: a PKCS#8 DER body, base64 in 64-column PEM armor, mode 0600. The
/// loader sniffs on that armor, so writing it by hand is what exercises the
/// branch an operator's file actually takes.
fn write_key(dir: &std::path::Path, name: &str) -> KeyFile {
    fs::create_dir_all(dir).expect("create the scratch directory");
    let mut seed = [0_u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut seed);
    let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
    let der = signing.to_pkcs8_der().expect("encode PKCS#8 DER");
    let body = base64::engine::general_purpose::STANDARD.encode(der.as_bytes());
    let mut pem = String::from("-----BEGIN PRIVATE KEY-----\n");
    for chunk in body.as_bytes().chunks(64) {
        pem.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        pem.push('\n');
    }
    pem.push_str("-----END PRIVATE KEY-----\n");
    let path = dir.join(name);
    let mut file = fs::File::create(&path).expect("create the key file");
    file.write_all(pem.as_bytes()).expect("write the key file");
    drop(file);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .expect("restrict the key file");
    KeyFile {
        path,
        public: signing.verifying_key().to_bytes(),
    }
}

fn write_peers(dir: &std::path::Path, entries: &[String]) -> std::path::PathBuf {
    fs::create_dir_all(dir).expect("create the scratch directory");
    let path = dir.join("service-peers.json");
    fs::write(&path, format!("{{\"keys\":[{}]}}", entries.join(","))).expect("write the document");
    path
}

fn entry(name: &str, public: &[u8; 32]) -> String {
    format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://zeroship.ai/{name}","x":"{}"}}"#,
        URL_SAFE_NO_PAD.encode(public)
    )
}

#[compio::test]
async fn a_configured_pair_lets_one_service_call_another_on_its_granted_endpoint() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let peers = write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]);

    // The worker mints for control.
    let keyring = ServiceKeyring::load(
        service_issuer(WORKER_SERVICE_NAME).expect("worker issuer"),
        &worker.path,
        &peers,
    )
    .expect("load the worker's keyring");
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let assertion = keyring.mint_for(&control).expect("mint for control");

    // Control verifies under the same document.
    let bundle = load_peer_bundle(&peers).expect("control loads the peer bundle");
    let verifier =
        ServiceAssertionVerifier::new(bundle, Arc::new(InMemoryReplayStore::new()));
    let identity = verify_service_call(
        &verifier,
        Some(&format!("Bearer {assertion}")),
        control.as_str(),
        endpoints::CONTROL_APP_ENV,
    )
    .await
    .expect("the worker is granted the app-env endpoint");
    assert!(identity.matches_principal(&worker_principal()));

    // Same credential, an endpoint the worker holds no grant on. The
    // verification succeeds and the AUTHORIZATION is what refuses, which is the
    // property a shared bearer cannot express.
    let refused = verify_service_call(
        &verifier,
        Some(&format!(
            "Bearer {}",
            keyring.mint_for(&control).expect("mint a second assertion")
        )),
        control.as_str(),
        endpoints::CONTROL_BILLING_RECONCILE,
    )
    .await;
    assert_eq!(refused.err(), Some(AuthError::CredentialRejected));
}

#[compio::test]
async fn an_absent_credential_is_refused_before_any_verification() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let peers = write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]);
    let bundle = load_peer_bundle(&peers).expect("load the bundle");
    let verifier =
        ServiceAssertionVerifier::new(bundle, Arc::new(InMemoryReplayStore::new()));
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");

    for header in [None, Some(""), Some("Bearer "), Some("Basic abc")] {
        assert_eq!(
            verify_service_call(&verifier, header, control.as_str(), endpoints::CONTROL_APP_ENV)
                .await
                .err(),
            Some(AuthError::NoCredentialPresented),
            "header {header:?} must not reach the verifier at all"
        );
    }
}

#[compio::test]
async fn a_peer_key_verifies_only_assertions_from_the_issuer_it_is_published_under() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");

    // The control: published under the worker's issuer, the worker's assertion
    // verifies.
    let honest = write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]);
    let keyring = ServiceKeyring::load(
        service_issuer(WORKER_SERVICE_NAME).expect("worker issuer"),
        &worker.path,
        &honest,
    )
    .expect("load the keyring");
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let assertion = keyring.mint_for(&control).expect("mint");
    let verifier = ServiceAssertionVerifier::new(
        load_peer_bundle(&honest).expect("load"),
        Arc::new(InMemoryReplayStore::new()),
    );
    assert!(verify_service_call(
        &verifier,
        Some(&format!("Bearer {assertion}")),
        control.as_str(),
        endpoints::CONTROL_APP_ENV
    )
    .await
    .is_ok());

    // One variable changed: the SAME key published under the GATEWAY's issuer.
    // The worker's assertion no longer verifies, because the key is resolved
    // from `iss` rather than from a flat pool - RFC 8725 section 3.8.
    let misfiled = write_peers(
        &dir.path().join("misfiled"),
        &[entry(GATEWAY_SERVICE_NAME, &worker.public)],
    );
    let misfiled_verifier = ServiceAssertionVerifier::new(
        load_peer_bundle(&misfiled).expect("load"),
        Arc::new(InMemoryReplayStore::new()),
    );
    let second = keyring.mint_for(&control).expect("mint a second assertion");
    assert_eq!(
        verify_service_call(
            &misfiled_verifier,
            Some(&format!("Bearer {second}")),
            control.as_str(),
            endpoints::CONTROL_APP_ENV
        )
        .await
        .err(),
        Some(AuthError::CredentialRejected)
    );
}

#[test]
fn the_document_is_refused_when_it_does_not_hold_ed25519_public_keys() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let x = URL_SAFE_NO_PAD.encode(worker.public);
    let iss = format!("spiffe://zeroship.ai/{WORKER_SERVICE_NAME}");

    // Control first: the well-formed entry loads.
    assert!(
        load_peer_bundle(&write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]))
            .is_ok()
    );

    let cases: [(&str, String); 5] = [
        ("wrong kty", format!(r#"{{"kty":"EC","crv":"Ed25519","iss":"{iss}","x":"{x}"}}"#)),
        ("wrong crv", format!(r#"{{"kty":"OKP","crv":"P-256","iss":"{iss}","x":"{x}"}}"#)),
        ("short key", format!(r#"{{"kty":"OKP","crv":"Ed25519","iss":"{iss}","x":"AAAA"}}"#)),
        (
            "issuer is an endpoint url",
            format!(r#"{{"kty":"OKP","crv":"Ed25519","iss":"https://control/x","x":"{x}"}}"#),
        ),
        (
            "stated kid disagrees with the thumbprint",
            format!(
                r#"{{"kty":"OKP","crv":"Ed25519","iss":"{iss}","x":"{x}","kid":"not-the-thumbprint"}}"#
            ),
        ),
    ];
    // The floor is every case above, so a case added without an assertion is a
    // compile-visible mistake rather than a silently unrun row.
    assert_eq!(cases.len(), 5);
    for (name, body) in cases {
        let scratch = dir.path().join(name.replace(' ', "-"));
        let path = write_peers(&scratch, &[body]);
        assert!(
            matches!(load_peer_bundle(&path), Err(PeerKeyError::Document { .. })),
            "{name} must be refused"
        );
    }

    // And the kid the document MAY state is the one the loader derives.
    let stated = format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"{iss}","x":"{x}","kid":"{}"}}"#,
        thumbprint_key_id(&worker.public)
    );
    assert!(load_peer_bundle(&write_peers(&dir.path().join("agreeing"), &[stated])).is_ok());
}

#[test]
fn a_group_readable_private_key_is_refused() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let peers = write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]);
    let issuer = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");

    // Control: at 0600 the pair loads.
    assert!(ServiceKeyring::load(issuer.clone(), &worker.path, &peers).is_ok());

    fs::set_permissions(&worker.path, fs::Permissions::from_mode(0o640))
        .expect("widen the key file");
    assert!(matches!(
        ServiceKeyring::load(issuer, &worker.path, &peers),
        Err(PeerKeyError::InsecurePermissions { .. })
    ));
}

/// Fence F4's startup half, at the one function every `main` loads through.
///
/// The three refusals are one case in the shape that matters - a process that
/// cannot verify what it is told - and they are asserted TOGETHER because the
/// defect this fence removes was a deployment distinguishing between them: an
/// unset path used to boot into a process refusing every guarded edge, while a
/// wrong path exited. Both are the same operator mistake and both now exit.
///
/// The MISSING and MALFORMED arms additionally require the message to carry the
/// PATH. An operator reading a boot log needs the file, and the unset arm cannot
/// supply one, which is exactly why it is a separate variant naming the setting
/// instead.
#[test]
fn a_peer_document_that_is_unset_missing_or_malformed_refuses_to_load() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let issuer = || service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");

    // THE ONE-VARIABLE CONTROL, first: the same key and a well-formed document
    // load. Every refusal below changes the peer document and nothing else, so
    // a loader that refused everything cannot print what this test prints.
    let good = write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]);
    assert!(ServiceKeyring::load(issuer(), &worker.path, &good).is_ok());

    // UNSET. The default of both settings is an empty `PathBuf`, which reaches
    // the filesystem as `""` and returns a not-found naming no file at all.
    let unset = ServiceKeyring::load(issuer(), &worker.path, std::path::Path::new(""));
    assert!(
        matches!(unset, Err(PeerKeyError::NotConfigured { which }) if which.contains("peer")),
        "an unconfigured peer document must refuse, naming the setting: {unset:?}"
    );
    // And the same for this service's own key, so the pair cannot end up with
    // one half guarded.
    let unset_key = ServiceKeyring::load(issuer(), std::path::Path::new(""), &good);
    assert!(
        matches!(unset_key, Err(PeerKeyError::NotConfigured { which }) if which.contains("key")),
        "an unconfigured service key must refuse, naming the setting: {unset_key:?}"
    );

    // MISSING: a path the operator did configure, pointing at nothing.
    let absent = dir.path().join("no-such-service-peers.json");
    assert!(!absent.exists(), "the fixture path must really be absent");
    let missing = ServiceKeyring::load(issuer(), &worker.path, &absent);
    let message = missing.expect_err("a missing peer document refuses").to_string();
    assert!(
        message.contains(&absent.display().to_string()),
        "the refusal must name the file it could not read: {message}"
    );

    // MALFORMED: the path resolves, the bytes are not a peer document.
    let malformed = dir.path().join("malformed-service-peers.json");
    fs::write(&malformed, "{ this is not a JWKS document").expect("write the malformed document");
    let bad = ServiceKeyring::load(issuer(), &worker.path, &malformed);
    let message = bad.expect_err("a malformed peer document refuses").to_string();
    assert!(
        message.contains(&malformed.display().to_string()),
        "the refusal must name the file it could not parse: {message}"
    );

    // Does NOT cover whether the three `main`s CALL this loader rather than
    // building a keyring some other way. That link is
    // `tests/service_peer_boot_gate.sh`, against the real binaries.
}

/// The DOCUMENT-only half of the one-key refusal: whoever loads it, refuses.
///
/// This is the shape `zeroship dev init` emits when one secret is mounted onto
/// all four `*_SERVICE_KEY_FILE` paths, and it is worse than a defeated fence:
/// with every issuer resolving to the same key, holding ANY one private half is
/// the ability to present as EVERY service, which is the shared secret the
/// asymmetric design replaced.
#[test]
fn a_document_publishing_one_key_under_two_issuers_refuses_to_load() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let gateway = write_key(dir.path(), "gateway.pem");
    let worker_iss = format!("spiffe://zeroship.ai/{WORKER_SERVICE_NAME}");
    let gateway_iss = format!("spiffe://zeroship.ai/{GATEWAY_SERVICE_NAME}");

    // THE ONE-VARIABLE CONTROL: two issuers, two DIFFERENT keys. Every case
    // below changes which key the second entry carries and nothing else, so a
    // loader that refused every multi-entry document cannot print this.
    let distinct = write_peers(
        dir.path(),
        &[
            entry(WORKER_SERVICE_NAME, &worker.public),
            entry(GATEWAY_SERVICE_NAME, &gateway.public),
        ],
    );
    assert!(
        load_peer_bundle(&distinct).is_ok(),
        "distinct keys under distinct issuers are the shape an operator must produce"
    );

    // A SECOND CONTROL: the same key repeated under the SAME issuer. That is
    // idempotent re-publication, not a shared identity, and it must still load
    // - otherwise the refusal below would be about repetition rather than about
    // issuers.
    let repeated = write_peers(
        &dir.path().join("repeated"),
        &[
            entry(WORKER_SERVICE_NAME, &worker.public),
            entry(WORKER_SERVICE_NAME, &worker.public),
        ],
    );
    assert!(
        load_peer_bundle(&repeated).is_ok(),
        "one key twice under one issuer is idempotent, not a second identity"
    );

    let shared = write_peers(
        &dir.path().join("shared"),
        &[
            entry(WORKER_SERVICE_NAME, &worker.public),
            entry(GATEWAY_SERVICE_NAME, &worker.public),
        ],
    );
    let message = load_peer_bundle(&shared)
        .expect_err("one key under two issuers must refuse")
        .to_string();
    for token in [
        worker_iss.as_str(),
        gateway_iss.as_str(),
        &shared.display().to_string(),
    ] {
        assert!(
            message.contains(token),
            "the refusal must name {token} so an operator knows what to change: {message}"
        );
    }

    // The `zeroship dev init` shape, in full: ONE key under all four issuers.
    let one_key_everywhere = write_peers(
        &dir.path().join("one-key-everywhere"),
        &[
            entry(WORKER_SERVICE_NAME, &worker.public),
            entry(GATEWAY_SERVICE_NAME, &worker.public),
            entry(CONTROL_SERVICE_NAME, &worker.public),
            entry(AUTH_SERVICE_NAME, &worker.public),
        ],
    );
    assert!(load_peer_bundle(&one_key_everywhere).is_err());
}

/// The KEYRING half: this process's own public key published under somebody
/// else's issuer.
///
/// Independent of the document-only refusal above rather than implied by it.
/// The document here names ONE issuer and repeats no key, so it is well-formed
/// on its own; what is wrong is only visible where the PRIVATE half and the
/// document are held together. Under it, `UserEnvelopeVerifier::for_issuer`
/// resolves the gateway issuer to this process's own key - and the envelope
/// wire format carries no issuer, only a thumbprint `kid` - so the worker's own
/// signer stamps exactly the `kid` its own verifier accepts, and fence F4 is
/// defeated by configuration.
#[test]
fn a_keyring_whose_own_key_is_published_under_a_foreign_issuer_refuses_to_load() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let worker = write_key(dir.path(), "worker.pem");
    let issuer = || service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
    let worker_iss = format!("spiffe://zeroship.ai/{WORKER_SERVICE_NAME}");
    let gateway_iss = format!("spiffe://zeroship.ai/{GATEWAY_SERVICE_NAME}");

    // THE ONE-VARIABLE CONTROL: the same private file, the same key material,
    // published under the worker's OWN issuer. Only `iss` differs below.
    let own = write_peers(dir.path(), &[entry(WORKER_SERVICE_NAME, &worker.public)]);
    assert!(ServiceKeyring::load(issuer(), &worker.path, &own).is_ok());

    let misfiled = write_peers(
        &dir.path().join("misfiled"),
        &[entry(GATEWAY_SERVICE_NAME, &worker.public)],
    );
    assert!(
        load_peer_bundle(&misfiled).is_ok(),
        "the document alone is well-formed, so the document-only refusal cannot catch this"
    );

    let message = ServiceKeyring::load(issuer(), &worker.path, &misfiled)
        .expect_err("a keyring whose own key is published as a peer must refuse")
        .to_string();
    for token in [
        worker_iss.as_str(),
        gateway_iss.as_str(),
        &misfiled.display().to_string(),
        &worker.path.display().to_string(),
    ] {
        assert!(
            message.contains(token),
            "the refusal must name {token} so an operator knows what to change: {message}"
        );
    }
}

/// `from_parts` is the one door to a keyring that skips `load`, so the refusal
/// has to live there rather than beside the two path reads.
#[test]
fn from_parts_is_not_a_way_around_the_own_key_refusal() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let file = write_key(dir.path(), "worker.pem");
    let worker = service_issuer(WORKER_SERVICE_NAME).expect("worker issuer");
    let gateway = service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer");
    // Two owned handles on ONE key, so the control and the case differ in the
    // issuer the bundle files it under and in nothing else.
    let held = || load_signing_key(&file.path).expect("load the private half");

    let mut own = ServiceTrustBundle::new();
    own.trust(&worker, thumbprint_key_id(&file.public), file.public)
        .expect("publish the key under its own issuer");
    assert!(ServiceKeyring::from_parts(worker.clone(), held(), own).is_ok());

    let mut foreign = ServiceTrustBundle::new();
    foreign
        .trust(&gateway, thumbprint_key_id(&file.public), file.public)
        .expect("publish the key under the gateway's issuer");
    let message = ServiceKeyring::from_parts(worker.clone(), held(), foreign)
        .expect_err("from_parts must apply the same refusal as load")
        .to_string();
    assert!(message.contains(worker.as_str()), "{message}");
    assert!(message.contains(gateway.as_str()), "{message}");
}

/// The identifier a worker instance mints under, once it has enrolled.
///
/// A `wkr_` typed id is base62 over a UUIDv7, and the literal here is one shaped
/// like the ones `worker_enrolment` returns. It is written out rather than
/// generated so the multi-segment path this whole separation rests on is visible
/// in the test that depends on it.
fn worker_instance_issuer() -> ServiceIssuer {
    ServiceIssuer::parse(&format!(
        "spiffe://zeroship.ai/{WORKER_SERVICE_NAME}/wkr_3Kd9QmZp2XvB"
    ))
    .expect("an instance path is a well-formed issuer identifier")
}

/// A worker instance is ADDRESSED by its role and MINTS under its instance name.
///
/// The two were one value until this split: `ServiceAuth::verify` sourced the
/// audience it requires of inbound callers from `keyring.issuer()`, the
/// identifier the process mints under. Per-instance worker identity needs them
/// apart, because the gateway holds one name for the whole role - it dispatches
/// over a hash ring and cannot know which instance it reached - so a worker that
/// required its own minting name would refuse every caller.
///
/// This is a DISTINGUISHER and not a boundary. Enrolment authenticates with the
/// shared `svc/worker` role key, so a holder of that key can enrol as many
/// instances as it likes; what an instance name buys is attribution,
/// per-instance revocation and a countable event.
#[compio::test]
async fn a_service_requires_the_audience_it_is_addressed_by_not_the_one_it_mints_under() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let gateway_file = write_key(dir.path(), "gateway.pem");
    let instance_file = write_key(dir.path(), "worker-instance.pem");

    // The document both processes read. The instance's own public half is
    // deliberately absent from it: a process may not find its own key published
    // under a foreign issuer, and no peer needs to verify the instance here.
    let peers = write_peers(dir.path(), &[entry(GATEWAY_SERVICE_NAME, &gateway_file.public)]);

    let role = service_issuer(WORKER_SERVICE_NAME).expect("the worker ROLE issuer");
    let instance = worker_instance_issuer();
    assert_ne!(instance, role, "the instance name must be the finer of the two");

    let mut keyring = ServiceKeyring::from_parts(
        instance.clone(),
        load_signing_key(&instance_file.path).expect("the instance's private half"),
        load_peer_bundle(&peers).expect("the instance loads the peer bundle"),
    )
    .expect("an instance keyring")
    .addressed_as(role.clone());
    assert_eq!(keyring.issuer(), &instance, "it mints under the instance name");
    assert_eq!(keyring.audience(), &role, "it is addressed by the role name");

    let bundle = keyring.take_bundle().expect("the verifier takes the bundle");
    let worker_auth = ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(bundle)));

    let gateway = ServiceKeyring::from_parts(
        service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer"),
        load_signing_key(&gateway_file.path).expect("the gateway's private half"),
        load_peer_bundle(&peers).expect("the gateway loads the peer bundle"),
    )
    .expect("the gateway's keyring");

    // The gateway addresses the ROLE, which is the only worker name it holds.
    let identity = worker_auth
        .verify(
            Some(&format!(
                "Bearer {}",
                gateway.mint_for(&role).expect("mint for the worker role")
            )),
            endpoints::WORKER_DISPATCH,
        )
        .await
        .expect("an instance must accept a caller that addresses the role");
    assert!(identity.matches_principal(&gateway_principal()));

    // ONE VARIABLE: the same caller, the same key, the same endpoint, addressing
    // the INSTANCE path instead. Refused - the minting name is not an address,
    // so learning which instance answered buys a caller no second way in.
    let refused = worker_auth
        .verify(
            Some(&format!(
                "Bearer {}",
                gateway
                    .mint_for(&instance)
                    .expect("mint for the instance path")
            )),
            endpoints::WORKER_DISPATCH,
        )
        .await;
    assert_eq!(refused.err(), Some(AuthError::CredentialRejected));
}

/// The control for the pair above: a keyring that never separated the two.
///
/// Every service whose identity IS its role name - the gateway, control, the
/// auth service, and a worker until it enrols - keeps requiring its own issuer,
/// and this arm is what makes the refusal above a statement about the audience
/// rather than about a keyring that had stopped accepting anyone.
#[compio::test]
async fn a_role_keyring_is_addressed_by_the_name_it_mints_under() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let gateway_file = write_key(dir.path(), "gateway.pem");
    let worker_file = write_key(dir.path(), "worker.pem");
    let peers = write_peers(dir.path(), &[entry(GATEWAY_SERVICE_NAME, &gateway_file.public)]);

    let role = service_issuer(WORKER_SERVICE_NAME).expect("the worker ROLE issuer");
    let mut keyring = ServiceKeyring::from_parts(
        role.clone(),
        load_signing_key(&worker_file.path).expect("the worker's private half"),
        load_peer_bundle(&peers).expect("the worker loads the peer bundle"),
    )
    .expect("a role keyring");
    assert_eq!(
        keyring.issuer(),
        keyring.audience(),
        "without addressed_as the two are the same identifier"
    );

    let bundle = keyring.take_bundle().expect("the verifier takes the bundle");
    let worker_auth = ServiceAuth::new(keyring, Arc::new(TransportAssertionVerifier::new(bundle)));

    let gateway = ServiceKeyring::from_parts(
        service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer"),
        load_signing_key(&gateway_file.path).expect("the gateway's private half"),
        load_peer_bundle(&peers).expect("the gateway loads the peer bundle"),
    )
    .expect("the gateway's keyring");

    let identity = worker_auth
        .verify(
            Some(&format!(
                "Bearer {}",
                gateway.mint_for(&role).expect("mint for the worker role")
            )),
            endpoints::WORKER_DISPATCH,
        )
        .await
        .expect("the role-to-role call is unchanged by the separation");
    assert!(identity.matches_principal(&gateway_principal()));
}

fn worker_principal() -> ServicePrincipal {
    ServicePrincipal::new(
        TrustDomain::new("zeroship.ai"),
        ServiceName::new(WORKER_SERVICE_NAME),
    )
}

fn gateway_principal() -> ServicePrincipal {
    ServicePrincipal::new(
        TrustDomain::new("zeroship.ai"),
        ServiceName::new(GATEWAY_SERVICE_NAME),
    )
}
