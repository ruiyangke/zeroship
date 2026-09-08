//! What may open `/internal/apps/{id}/env` and `/internal/apps/{id}`.
//!
//! These are the privileged internal reads: the first returns an app's
//! DECRYPTED environment. Before this suite they were opened by a bearer equal
//! to `control_key` - a secret four binaries hold, so every holder of it was
//! every other holder and control had nothing to tell them apart with.
//!
//! Every refusal here is paired with an acceptance differing in exactly ONE
//! variable, because the endpoint answers a bare 401 by design and a single
//! failing case cannot say which check produced it.
//!
//! The 400 in the accepted arms is the point: it is the handler's OWN answer to
//! a malformed app id, reached only after the guard admitted the caller. Using
//! a malformed id keeps these arms off the app tables entirely, so what they
//! measure is the guard and nothing else.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use rand::RngCore as _;
use uuid::Uuid;

use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::worker_enrolment::{enrol, EnrolmentEnvelope, WorkerEnrolmentRequest};
use zeroship_control::{
    internal, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::service_assertion::{
    ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
};
use zeroship_core::service_peers::{
    load_peer_bundle, service_issuer, InstanceSigningKey, ServiceAuth, ServiceKeyring,
    CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN, WORKER_SERVICE_NAME,
};
use zeroship_core::typed_id::new_worker_instance_id;

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const CONTROL_KEY: &str = "test-control-key";

/// The enrolment declaration this fixture carries, and a peer inside it.
///
/// The instance arms need a row written by the PRODUCTION writer rather than an
/// INSERT of their own, so the key control resolves is the one a real worker
/// presented and the id is the one control minted.
const ENROLMENT_NETWORKS: &str = "10.7.0.0/16";
const ENROLMENT_PORTS: &str = "8080-8090";
const ENROLMENT_PEER: &str = "10.7.3.9:51314";
const ADVERTISED_PORT: u16 = 8080;

/// An instance identifier the OPERATOR'S peer document publishes a key for, and
/// that nothing ever enrols.
///
/// It exists so one arm can rule on the half of this design a registry lookup
/// alone cannot state: the operator file is the source of ROLE keys and of
/// nothing else. An issuer naming an instance must be answered from the
/// registry or refused - never answered from the file, which carries no status
/// and so cannot be revoked.
const PLANTED_INSTANCE_ID: &str = "wkr_PlantedInOperatorFileX";

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-internal-authn-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

/// Write one ed25519 key per service and one peer document naming every public
/// half - the same shape `tests/lib/runtime_secrets.sh` writes for the
/// end-to-end harnesses, so this suite and those harnesses exercise one format.
///
/// It also publishes ONE key under an INSTANCE issuer
/// ([`PLANTED_INSTANCE_ID`]), and returns its private half. That entry is
/// well formed - `load_peer_bundle` admits any issuer identifier, and an
/// instance identifier is one - so nothing in the loader refuses a deployment
/// whose operator files an instance key by hand. What must refuse it is the
/// verification path, and it cannot be shown to unless the key is really there.
fn write_service_keys(dir: &Path) -> (PathBuf, ServiceSigningKey) {
    let mut entries = Vec::new();
    for name in [CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME, GATEWAY_SERVICE_NAME] {
        let mut seed = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let der = signing.to_pkcs8_der().expect("encode PKCS#8 DER");
        let path = dir.join(format!("{}.der", name.replace('/', "-")));
        let mut file = fs::File::create(&path).expect("create the key file");
        file.write_all(der.as_bytes()).expect("write the key file");
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 600");
        entries.push(format!(
            r#"{{"kty":"OKP","crv":"Ed25519","iss":"spiffe://zeroship.ai/{name}","x":"{}"}}"#,
            URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes())
        ));
    }
    let planted = ServiceSigningKey::generate();
    entries.push(format!(
        r#"{{"kty":"OKP","crv":"Ed25519","iss":"{}","x":"{}"}}"#,
        instance_issuer(PLANTED_INSTANCE_ID).as_str(),
        planted.public_jwk_x()
    ));
    let peers = dir.join("service-peers.json");
    fs::write(&peers, format!("{{\"keys\":[{}]}}", entries.join(","))).expect("write peers");
    (peers, planted)
}

/// The identifier one instance of the worker role mints under.
fn instance_issuer(instance_id: &str) -> ServiceIssuer {
    ServiceIssuer::parse(&format!(
        "spiffe://{SERVICE_TRUST_DOMAIN}/{WORKER_SERVICE_NAME}/{instance_id}"
    ))
    .expect("an instance issuer parses")
}

fn keyring_for(name: &str, dir: &Path, peers: &Path) -> ServiceKeyring {
    ServiceKeyring::load(
        service_issuer(name).expect("service issuer parses"),
        &dir.join(format!("{}.der", name.replace('/', "-"))),
        peers,
    )
    .expect("load the keyring")
}

struct Fixture {
    state: Arc<AppState>,
    worker: ServiceKeyring,
    gateway: ServiceKeyring,
    /// A keyring minting under [`PLANTED_INSTANCE_ID`] on the key the peer
    /// document publishes for it. Built with `from_parts` rather than
    /// `InstanceSigningKey::into_keyring`, because that constructor refuses a
    /// key the bundle publishes at all - which is exactly the arrangement this
    /// keyring exists to present.
    planted_instance: ServiceKeyring,
    peers: PathBuf,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    key_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
        let _ = std::fs::remove_dir_all(&self.key_dir);
    }
}

impl Fixture {
    /// The header a worker presents. A FRESH assertion each call, because the
    /// full profile burns the `jti` and a cached one is exactly what the store
    /// refuses.
    fn worker_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.worker.mint_for(&control).map(|a| format!("Bearer {a}")).expect("mint")
    }

    fn gateway_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.gateway.mint_for(&control).map(|a| format!("Bearer {a}")).expect("mint")
    }

    fn planted_instance_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.planted_instance
            .mint_for(&control)
            .map(|a| format!("Bearer {a}"))
            .expect("mint")
    }
}

async fn build_fixture() -> Fixture {
    let db_url = common::require_control_db();
    let blob_root = tmpdir("blob");
    let deploy_tmp_dir = tmpdir("deploy");
    let key_dir = tmpdir("keys");
    let (peers, planted_key) = write_service_keys(&key_dir);

    let registry = Registry::new(&db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    // The REAL verifier over the REAL table: `service_authn.service_assertion_replay`
    // in the live database this suite is pointed at. An in-memory store would
    // make the single-use arm below pass without proving the statement, the
    // grants, or the schema qualification are right.
    let mut control_keyring = keyring_for(CONTROL_SERVICE_NAME, &key_dir, &peers);
    let bundle = control_keyring.take_bundle().expect("peer bundle");
    let replay = Arc::new(SharedClientReplayStore::new(Arc::clone(&control_pg)));
    let service_auth = Arc::new(ServiceAuth::new(
        control_keyring,
        Arc::new(ServiceAssertionVerifier::new(bundle, replay)),
    ));

    Fixture {
        state: Arc::new(AppState {
            service_auth,
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new(CONTROL_KEY.to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            gateway_url: "http://127.0.0.1:9".to_string(),
            worker_urls: vec![],
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            // DECLARED, not closed: the instance arms below enrol through the
            // production writer, so this fixture has to be a control plane that
            // can enrol at all.
            worker_enrolment: EnrolmentEnvelope::parse(
                ENROLMENT_NETWORKS,
                ENROLMENT_PORTS,
                false,
            )
            .expect("the declaration parses"),
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg,
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            auth_provider: zeroship_control::platform_auth_provider(
                "https://auth.zeroship.test/oauth2",
                Some(common::platform_jwks_url()),
            ),
            provider_registry: zeroship_control::metering::provider::builtin_registry(),
            billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
            billing_stream: None,
            tax_provider: zeroship_control::tax::build_tax_provider(
                &zeroship_control::tax::TaxProviderConfig::native(),
            )
            .expect("native tax provider builds"),
            notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: std::sync::Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        }),
        worker: keyring_for(WORKER_SERVICE_NAME, &key_dir, &peers),
        gateway: keyring_for(GATEWAY_SERVICE_NAME, &key_dir, &peers),
        planted_instance: ServiceKeyring::from_parts(
            instance_issuer(PLANTED_INSTANCE_ID),
            planted_key,
            load_peer_bundle(&peers).expect("peer bundle loads"),
        )
        .expect("a key published under its OWN issuer builds a keyring"),
        peers,
        blob_root,
        deploy_tmp_dir,
        key_dir,
    }
}

/// `not-a-uuid` is deliberate: an admitted caller gets the handler's own 400
/// without the request ever reaching an app row, so these arms rule on the
/// guard alone.
const BAD_APP_ID: &str = "not-a-uuid";

macro_rules! internal_app {
    ($state:expr) => {
        test::init_service(
            web::App::new()
                .state($state)
                .service(
                    web::resource("/internal/apps/{app_id}/env")
                        .route(web::get().to(internal::get_app_env)),
                )
                .service(
                    web::resource("/internal/apps/{app_id}")
                        .route(web::get().to(internal::get_app_version)),
                ),
        )
        .await
    };
}

#[ntex::test]
async fn the_shared_control_key_no_longer_opens_the_app_environment() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    // THE REFUSAL. `control_key` is the secret the worker, the gateway and
    // control all hold, so presenting it proves membership of a group rather
    // than an identity.
    let refused = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", format!("Bearer {CONTROL_KEY}"))
            .to_request(),
    )
    .await;
    assert_eq!(
        refused.status(),
        StatusCode::UNAUTHORIZED,
        "a shared-secret bearer must not open the decrypted-environment read"
    );

    // THE CONTROL, one variable apart: same route, same process, a credential
    // only the worker can produce. It reaches the handler, which answers its own
    // 400 for the malformed id - so the 401 above is the guard's verdict and not
    // an artifact of the request being malformed.
    let admitted = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", fixture.worker_header())
            .to_request(),
    )
    .await;
    assert_eq!(
        admitted.status(),
        StatusCode::BAD_REQUEST,
        "a worker assertion must reach the handler"
    );
}

#[ntex::test]
async fn the_shared_control_key_no_longer_opens_the_app_version_read() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    let refused = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}"))
            .header("authorization", format!("Bearer {CONTROL_KEY}"))
            .to_request(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    let admitted = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}"))
            .header("authorization", fixture.worker_header())
            .to_request(),
    )
    .await;
    assert_eq!(admitted.status(), StatusCode::BAD_REQUEST);
}

#[ntex::test]
async fn an_absent_credential_is_refused() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    for header in [None, Some(String::new()), Some("Bearer ".to_string())] {
        let mut request = test::TestRequest::get().uri(&format!("/internal/apps/{BAD_APP_ID}/env"));
        if let Some(value) = header.clone() {
            request = request.header("authorization", value);
        }
        let response = test::call_service(&app, request.to_request()).await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "header {header:?} must be refused"
        );
    }
}

#[ntex::test]
async fn a_captured_assertion_cannot_be_presented_twice() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let header = fixture.worker_header();

    // First presentation: admitted, and the `jti` is claimed in the live
    // `service_authn.service_assertion_replay` row this call writes.
    let first = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", header.clone())
            .to_request(),
    )
    .await;
    assert_eq!(first.status(), StatusCode::BAD_REQUEST);

    // Second presentation of the SAME bytes: refused. This is the property the
    // full profile buys and the reason this edge pays a store write - an
    // attacker who captures one privileged read cannot repeat it.
    let replayed = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", header)
            .to_request(),
    )
    .await;
    assert_eq!(replayed.status(), StatusCode::UNAUTHORIZED);

    // And a FRESH assertion from the same worker is still admitted, so the
    // refusal above is the single-use claim rather than the worker being
    // locked out.
    let fresh = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", fixture.worker_header())
            .to_request(),
    )
    .await;
    assert_eq!(fresh.status(), StatusCode::BAD_REQUEST);
}

#[ntex::test]
async fn a_valid_assertion_from_the_wrong_service_is_refused() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    // The gateway's assertion VERIFIES - same mechanism, same bundle, same
    // audience - and is still refused, because `svc/gateway` holds no grant on
    // the app-env endpoint. That separation is what a shared bearer cannot
    // express: it has no principal to hang a grant on.
    let refused = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", fixture.gateway_header())
            .to_request(),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);

    // Control: the worker holds the grant, same route, same instant.
    let admitted = test::call_service(
        &app,
        test::TestRequest::get()
            .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
            .header("authorization", fixture.worker_header())
            .to_request(),
    )
    .await;
    assert_eq!(admitted.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Worker INSTANCE assertions
// ---------------------------------------------------------------------------
//
// A worker mints under `svc/worker/<wkr_id>` after it enrols, on a key that
// exists nowhere but its own memory and the row control wrote. The operator's
// peer document therefore cannot carry that key, and `keys_for` is an exact
// string lookup, so control has to resolve it from `zeroship.worker_instances`
// before verification or refuse every instance outright.
//
// WHAT THE `status` FILTER BUYS, AND WHY IT IS THE ONE THING THESE ARMS EXIST
// TO BIND. Per-instance revocation is bought ENTIRELY by that filter. Resolve
// the key regardless of status and marking an instance `gone` does nothing at
// all, while looking exactly like a revocation mechanism that ran and approved.
// So each refusal below is paired with an acceptance one variable away, and the
// variable is the column.

/// The response body of an admitted enrolment, drained so the fixture's
/// Postgres client is not still borrowed when the test ends.
async fn body_json(mut response: web::HttpResponse) -> serde_json::Value {
    use ntex::util::{stream_recv, BytesMut};
    let mut body = response.take_body();
    let mut buf = BytesMut::new();
    while let Some(item) = stream_recv(&mut body).await {
        buf.extend_from_slice(&item.expect("body chunk"));
    }
    serde_json::from_slice(&buf).expect("body is JSON")
}

/// Enrol one instance through the PRODUCTION writer and return its id.
///
/// Not an INSERT of this suite's own: the id, the ring key and the row shape
/// are control's, and a hand-written row would let these arms pass against a
/// registry the enrolment path never produces.
async fn enrol_instance(state: &Arc<AppState>, public_key: &[u8; 32]) -> String {
    let response = enrol(
        state,
        Some(ENROLMENT_PEER.parse().expect("peer socket parses")),
        WorkerEnrolmentRequest {
            port: ADVERTISED_PORT,
            public_key: URL_SAFE_NO_PAD.encode(public_key),
        },
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "the fixture must be able to enrol at all"
    );
    body_json(response).await["instance_id"]
        .as_str()
        .expect("instance_id")
        .to_string()
}

async fn set_status(pg: &compio_postgres::Client, instance_id: &str, status: &str) {
    let moved = pg
        .execute(
            "UPDATE zeroship.worker_instances SET status = $2 WHERE id = $1",
            &[&instance_id, &status],
        )
        .await
        .expect("status progresses");
    assert_eq!(moved, 1, "exactly the probe row moves to {status}");
}

/// Remove rows this suite minted, by id so a concurrent sibling's rows survive.
async fn forget(pg: &compio_postgres::Client, instance_id: &str) {
    pg.execute(
        "DELETE FROM zeroship.worker_instances WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("probe row removed");
}

/// THE BEFORE/AFTER PAIR. One enrolled instance, one key, one route; the only
/// thing that moves is `status`.
///
/// EVERY PRESENTATION MINTS A FRESH ASSERTION, and that is not tidiness. The
/// full profile burns the `jti`, so re-presenting the admitted bytes would come
/// back 401 as a REPLAY whatever the status filter did - the arm would pass
/// against a control plane with no revocation at all, which is the exact shape
/// of failure this pair exists to rule out.
#[ntex::test]
async fn marking_an_instance_draining_or_gone_stops_its_assertions_verifying() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    let key = InstanceSigningKey::generate();
    let public = *key.public_key();
    let instance_id = enrol_instance(&fixture.state, &public).await;
    let keyring = key
        .into_keyring(
            instance_issuer(&instance_id),
            load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
        )
        .expect("a boot-drawn key the document does not publish builds a keyring");

    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let header = || {
        format!(
            "Bearer {}",
            keyring.mint_for(&control).expect("the instance mints")
        )
    };
    let get = |header: String| {
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::get()
                    .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
                    .header("authorization", header)
                    .to_request(),
            )
            .await
            .status()
        }
    };

    let pg = &fixture.state.control_pg;
    let active = get(header()).await;
    set_status(pg, &instance_id, "draining").await;
    let draining = get(header()).await;
    set_status(pg, &instance_id, "gone").await;
    let gone = get(header()).await;

    // Cleaned up BEFORE the assertions: a failing assertion panics past
    // anything after it, which is how a probe row survives exactly the runs
    // that matter.
    forget(pg, &instance_id).await;

    assert_eq!(
        active,
        StatusCode::BAD_REQUEST,
        "an active instance's assertion must reach the handler, or the two \
         refusals below prove only that instances never authenticate"
    );
    assert_eq!(
        draining,
        StatusCode::UNAUTHORIZED,
        "a draining instance must not authenticate"
    );
    assert_eq!(
        gone,
        StatusCode::UNAUTHORIZED,
        "a gone instance must not authenticate"
    );
}

/// An issuer naming an instance is answered from the REGISTRY or refused.
///
/// Both arms are unenrolled, and they differ in whether the operator's peer
/// document happens to publish a key for that instance issuer. The second is
/// the one that matters: a document entry carries no status, so a path that
/// fell back to it would hold a credential nothing can ever revoke, and the
/// registry lookup would be decoration.
#[ntex::test]
async fn an_instance_with_no_active_row_is_refused_and_never_resolved_from_the_peer_file() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    let stranger = InstanceSigningKey::generate()
        .into_keyring(
            instance_issuer(&new_worker_instance_id()),
            load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
        )
        .expect("instance keyring");
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let stranger_header = format!(
        "Bearer {}",
        stranger.mint_for(&control).expect("the instance mints")
    );

    let get = |header: String| {
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::get()
                    .uri(&format!("/internal/apps/{BAD_APP_ID}/env"))
                    .header("authorization", header)
                    .to_request(),
            )
            .await
            .status()
        }
    };

    assert_eq!(
        get(stranger_header).await,
        StatusCode::UNAUTHORIZED,
        "an instance that never enrolled holds no credential here"
    );
    assert_eq!(
        get(fixture.planted_instance_header()).await,
        StatusCode::UNAUTHORIZED,
        "an instance key published in the OPERATOR'S peer document must not \
         authenticate without a row: the file carries no status, so a key \
         resolved from it could never be revoked"
    );

    // THE CONTROL. The operator file is still the source of ROLE keys, and the
    // role path is untouched by any of the above.
    assert_eq!(
        get(fixture.worker_header()).await,
        StatusCode::BAD_REQUEST,
        "svc/worker itself must still authenticate from the operator file"
    );
}
