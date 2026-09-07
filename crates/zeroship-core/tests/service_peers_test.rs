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
    thumbprint_key_id, InMemoryReplayStore, ServiceAssertionVerifier, ServiceSigningKey,
};
use zeroship_core::service_identity::{
    endpoints, verify_service_call, AuthError, ServiceName, ServicePrincipal,
    TrustDomain,
};
use zeroship_core::service_peers::{
    load_peer_bundle, service_issuer, PeerKeyError, ServiceKeyring, CONTROL_SERVICE_NAME,
    GATEWAY_SERVICE_NAME, WORKER_SERVICE_NAME,
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

fn worker_principal() -> ServicePrincipal {
    ServicePrincipal::new(
        TrustDomain::new("zeroship.ai"),
        ServiceName::new(WORKER_SERVICE_NAME),
    )
}
