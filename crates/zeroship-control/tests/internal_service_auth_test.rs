//! What may open `/internal/apps/{id}/env` and `/internal/apps/{id}`, the
//! enroller cascade that reaches them, and an instance retiring itself.
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
    ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey, ServiceTrustBundle,
};
use zeroship_core::service_peers::{
    load_peer_bundle, service_issuer, InstanceSigningKey, ServiceAuth, ServiceKeyring,
    CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN, WORKER_ENROLLER_SERVICE_NAME,
    WORKER_SERVICE_NAME,
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

/// The deployment's single execution zone, seeded by
/// `db/migrations-ts/20260914000400_execution_zones_and_worker_enrollers.ts`.
const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";

/// An instance identifier the OPERATOR'S peer document publishes a key for, and
/// that nothing ever enrols.
///
/// It exists so one arm can rule on the half of this design a registry lookup
/// alone cannot state: the operator file is the source of ROLE keys and of
/// nothing else. An issuer naming an instance must be answered from the
/// registry or refused - never answered from the file, which carries no status
/// and so cannot be revoked.
const PLANTED_INSTANCE_ID: &str = "wkr_plantedinoperatorfilex000";

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
/// It ALSO publishes keys this deployment must never trust, and returns what
/// the arms need to present them:
///
/// - ONE key under an INSTANCE issuer ([`PLANTED_INSTANCE_ID`]). That entry is
///   well formed - `load_peer_bundle` admits any issuer identifier, and an
///   instance identifier is one - so nothing in the loader refuses a deployment
///   whose operator files an instance key by hand. What must refuse it is the
///   verification path, and it cannot be shown to unless the key is really
///   there.
/// - A key under the bare `svc/worker` ROLE and one under the bare
///   `svc/worker-enroller` ROLE: the stale file of a deployment that once
///   shipped a shared worker role key. No process holds either any more, and
///   Control must refuse both at role arity even though the file publishes
///   them - which, again, is only measurable while the keys are really there.
fn write_service_keys(dir: &Path) -> (PathBuf, ServiceSigningKey) {
    let mut entries = Vec::new();
    for name in [
        CONTROL_SERVICE_NAME,
        WORKER_SERVICE_NAME,
        WORKER_ENROLLER_SERVICE_NAME,
        GATEWAY_SERVICE_NAME,
    ] {
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

/// The identifier one enroller of `svc/worker-enroller` mints under.
fn enroller_issuer(enroller_id: &str) -> ServiceIssuer {
    ServiceIssuer::parse(&format!(
        "spiffe://{SERVICE_TRUST_DOMAIN}/{WORKER_ENROLLER_SERVICE_NAME}/{enroller_id}"
    ))
    .expect("an instance issuer parses")
}

fn new_enroller_id() -> String {
    format!("wen_{}", &Uuid::new_v4().simple().to_string()[..25])
}

/// Insert one `zeroship.worker_enrollers` row directly, the row an operator's
/// import file would leave, and return a keyring that mints under its instance
/// identifier. The import itself is measured in `worker_enroller_import_test`.
async fn seed_enroller_keyring(pg: &compio_postgres::Client) -> (String, ServiceKeyring) {
    let id = new_enroller_id();
    let key = InstanceSigningKey::generate();
    let public = *key.public_key();
    pg.execute(
        "INSERT INTO zeroship.worker_enrollers (id, public_key, execution_zone_id, status) \
         VALUES ($1, $2, $3, 'active')",
        &[&id, &public.as_slice(), &DEFAULT_ZONE_ID],
    )
    .await
    .expect("insert enroller row");
    let keyring = key
        .into_keyring(enroller_issuer(&id), ServiceTrustBundle::new())
        .expect("a boot-drawn key an empty bundle does not publish builds a keyring");
    (id, keyring)
}

async fn forget_enroller(pg: &compio_postgres::Client, enroller_id: &str) {
    pg.execute(
        "DELETE FROM zeroship.worker_enrollers WHERE id = $1",
        &[&enroller_id],
    )
    .await
    .expect("probe enroller row removed");
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
    /// An enrolled, ACTIVE worker instance: what every worker presents after
    /// its boot-time enrolment, written by the production enrolment path under
    /// [`Fixture::enroller_id`].
    worker: ServiceKeyring,
    worker_instance_id: String,
    enroller_id: String,
    /// A keyring minting under the bare `svc/worker` ROLE, on the key the
    /// operator's peer document publishes for it. The credential a shared role
    /// key used to be, and one Control must now refuse outright.
    stale_worker_role: ServiceKeyring,
    /// The same, for the bare `svc/worker-enroller` role.
    stale_enroller_role: ServiceKeyring,
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
    /// The header an enrolled worker presents. A FRESH assertion each call,
    /// because the full profile burns the `jti` and a cached one is exactly
    /// what the store refuses.
    fn worker_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.worker.mint_for(&control).map(|a| format!("Bearer {a}")).expect("mint")
    }

    fn stale_worker_role_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.stale_worker_role
            .mint_for(&control)
            .map(|a| format!("Bearer {a}"))
            .expect("mint")
    }

    fn stale_enroller_role_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.stale_enroller_role
            .mint_for(&control)
            .map(|a| format!("Bearer {a}"))
            .expect("mint")
    }

    /// Remove the rows this fixture enrolled. Called at the end of each arm:
    /// `live_db.rs` shares one database, and an `active` instance left behind
    /// would be probed by every later liveness sweep.
    async fn release(&self) {
        forget(&self.state.control_pg, &self.worker_instance_id).await;
        forget_enroller(&self.state.control_pg, &self.enroller_id).await;
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

    let state = Arc::new(AppState {
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
        });

    // The worker every arm presents is a REAL enrolled instance: an enroller
    // row, then the production enrolment path, then the keyring the worker
    // builds on the key it enrolled.
    let (enroller_id, _enroller_keyring) = seed_enroller_keyring(&state.control_pg).await;
    let key = InstanceSigningKey::generate();
    let public = *key.public_key();
    let worker_instance_id = enrol_instance(&state, &enroller_id, &public).await;
    let worker = key
        .into_keyring(
            instance_issuer(&worker_instance_id),
            load_peer_bundle(&peers).expect("peer bundle loads"),
        )
        .expect("a boot-drawn key the document does not publish builds a keyring");

    Fixture {
        state,
        worker,
        worker_instance_id,
        enroller_id,
        stale_worker_role: keyring_for(WORKER_SERVICE_NAME, &key_dir, &peers),
        stale_enroller_role: keyring_for(WORKER_ENROLLER_SERVICE_NAME, &key_dir, &peers),
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
                )
                .service(
                    web::resource("/internal/workers/enrol")
                        .route(web::post().to(internal::enrol_worker_instance)),
                )
                .service(
                    web::resource("/internal/workers/retire")
                        .route(web::post().to(internal::retire_worker_instance)),
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
    fixture.release().await;
    assert_eq!(
        admitted.status(),
        StatusCode::BAD_REQUEST,
        "an enrolled worker instance's assertion must reach the handler"
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
    fixture.release().await;
    assert_eq!(admitted.status(), StatusCode::BAD_REQUEST);
}

#[ntex::test]
async fn an_absent_credential_is_refused() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    let mut statuses = Vec::new();
    for header in [None, Some(String::new()), Some("Bearer ".to_string())] {
        let mut request = test::TestRequest::get().uri(&format!("/internal/apps/{BAD_APP_ID}/env"));
        if let Some(value) = header.clone() {
            request = request.header("authorization", value);
        }
        statuses.push((header, test::call_service(&app, request.to_request()).await.status()));
    }
    fixture.release().await;
    for (header, status) in statuses {
        assert_eq!(status, StatusCode::UNAUTHORIZED, "header {header:?} must be refused");
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
    fixture.release().await;
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
    fixture.release().await;
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

/// Enrol one instance through the PRODUCTION writer, under `enroller_id`, and
/// return its id.
///
/// Not an INSERT of this suite's own: the id, the ring key and the row shape
/// are control's, and a hand-written row would let these arms pass against a
/// registry the enrolment path never produces.
async fn enrol_instance(state: &Arc<AppState>, enroller_id: &str, public_key: &[u8; 32]) -> String {
    let response = enrol(
        state,
        Some(ENROLMENT_PEER.parse().expect("peer socket parses")),
        enroller_id,
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
    let (enroller_id, _enroller_keyring) = seed_enroller_keyring(&fixture.state.control_pg).await;

    let key = InstanceSigningKey::generate();
    let public = *key.public_key();
    let instance_id = enrol_instance(&fixture.state, &enroller_id, &public).await;
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
    forget_enroller(pg, &enroller_id).await;
    fixture.release().await;

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

    let stranger = get(stranger_header).await;
    let planted = get(fixture.planted_instance_header()).await;
    // THE CONTROL: an enrolled instance of the same role, on the same route.
    let enrolled = get(fixture.worker_header()).await;
    fixture.release().await;

    assert_eq!(
        stranger,
        StatusCode::UNAUTHORIZED,
        "an instance that never enrolled holds no credential here"
    );
    assert_eq!(
        planted,
        StatusCode::UNAUTHORIZED,
        "an instance key published in the OPERATOR'S peer document must not \
         authenticate without a row: the file carries no status, so a key \
         resolved from it could never be revoked"
    );
    assert_eq!(
        enrolled,
        StatusCode::BAD_REQUEST,
        "an instance answered from the registry must still authenticate, or the \
         two refusals above prove only that instances never do"
    );
}

/// The two worker roles have NO role arity: a `svc/worker` or
/// `svc/worker-enroller` assertion minted under the BARE role is refused, even
/// though the operator's peer document publishes a key for it.
///
/// This is the fence that makes "no process holds a `svc/worker` role key" a
/// property of Control rather than of every deployment's key hygiene. Before
/// it, a stale peer document still carrying the old shared role key let the
/// holder of its private half read any app's environment with no enrolment,
/// no status and nothing a revocation could reach.
///
/// Each refusal is paired with the enrolled instance of the same role on the
/// same route, and the gateway's role-arity assertion is the control for the
/// fence itself: a role that does authenticate at role arity still verifies
/// against the SAME document (and is refused only by its missing grant), so
/// the refusals are about the two roles and not about role arity in general.
#[ntex::test]
async fn a_bare_worker_role_assertion_is_refused_although_the_peer_file_publishes_its_key() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));

    let read_env = |header: String| {
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
    let enrol_with = |header: String| {
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/internal/workers/enrol")
                    .header("authorization", header)
                    .header("content-type", "application/json")
                    .set_payload(
                        serde_json::json!({
                            "port": ADVERTISED_PORT,
                            "public_key": URL_SAFE_NO_PAD.encode([0x5a_u8; 32]),
                        })
                        .to_string(),
                    )
                    .to_request(),
            )
            .await
            .status()
        }
    };

    let stale_role_env = read_env(fixture.stale_worker_role_header()).await;
    let enrolled_env = read_env(fixture.worker_header()).await;
    let stale_enroller = enrol_with(fixture.stale_enroller_role_header()).await;
    // The request carries no observable peer (`TestRequest` drops it), so an
    // enroller that PASSES the guard is refused 403 on the address instead -
    // a verdict distinct from the guard's 401.
    let (enroller_id, enroller_keyring) = seed_enroller_keyring(&fixture.state.control_pg).await;
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let real_enroller = enrol_with(format!(
        "Bearer {}",
        enroller_keyring.mint_for(&control).expect("the enroller mints")
    ))
    .await;
    let gateway = read_env(fixture.gateway_header()).await;
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    fixture.release().await;

    assert_eq!(
        stale_role_env,
        StatusCode::UNAUTHORIZED,
        "a bare svc/worker assertion must be refused even with its key published"
    );
    assert_eq!(enrolled_env, StatusCode::BAD_REQUEST);
    assert_eq!(
        stale_enroller,
        StatusCode::UNAUTHORIZED,
        "a bare svc/worker-enroller assertion must be refused even with its key published"
    );
    assert_eq!(
        real_enroller,
        StatusCode::FORBIDDEN,
        "an enroller instance passes the guard and is judged on the address \
         (this fixture's in-process request has no peer to derive one from)"
    );
    assert_eq!(
        gateway,
        StatusCode::UNAUTHORIZED,
        "the gateway verifies at role arity and holds no grant here"
    );
}

// ---------------------------------------------------------------------------
// An instance retiring itself
// ---------------------------------------------------------------------------

async fn instance_status(pg: &compio_postgres::Client, instance_id: &str) -> Option<String> {
    pg.query_opt(
        "SELECT status FROM zeroship.worker_instances WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("read instance status")
    .map(|row| row.get(0))
}

/// A worker's graceful exit: the instance declares itself `gone`, and from
/// then on its key authenticates nothing - including a second retirement.
///
/// The retirement is SELF-SCOPED: a sibling instance of the same enroller,
/// enrolled beside it, is the paired control and must be untouched, which is
/// what shows the endpoint retires the caller rather than the unit.
#[ntex::test]
async fn an_instance_retires_itself_and_only_itself() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;

    let sibling_key = InstanceSigningKey::generate();
    let sibling_public = *sibling_key.public_key();
    let sibling_id = enrol_instance(&fixture.state, &fixture.enroller_id, &sibling_public).await;
    let sibling = sibling_key
        .into_keyring(
            instance_issuer(&sibling_id),
            load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
        )
        .expect("sibling keyring");
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let sibling_header = || format!("Bearer {}", sibling.mint_for(&control).expect("mint"));

    let retire = |header: String| {
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/internal/workers/retire")
                    .header("authorization", header)
                    .to_request(),
            )
            .await
            .status()
        }
    };
    let read_env = |header: String| {
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

    let before = instance_status(pg, &fixture.worker_instance_id).await;
    let retired = retire(fixture.worker_header()).await;
    let after = instance_status(pg, &fixture.worker_instance_id).await;
    let read_after = read_env(fixture.worker_header()).await;
    let second = retire(fixture.worker_header()).await;
    let sibling_status = instance_status(pg, &sibling_id).await;
    let sibling_read = read_env(sibling_header()).await;
    forget(pg, &sibling_id).await;
    fixture.release().await;

    assert_eq!(before.as_deref(), Some("active"));
    assert_eq!(retired, StatusCode::NO_CONTENT);
    assert_eq!(
        after.as_deref(),
        Some("gone"),
        "the retirement must be recorded on the caller's own row"
    );
    assert_eq!(
        read_after,
        StatusCode::UNAUTHORIZED,
        "a retired instance's key must stop authenticating at once"
    );
    assert_eq!(
        second,
        StatusCode::UNAUTHORIZED,
        "a retired instance cannot retire again: its key no longer verifies"
    );
    assert_eq!(
        sibling_status.as_deref(),
        Some("active"),
        "retirement is the caller's alone, never its unit's"
    );
    assert_eq!(sibling_read, StatusCode::BAD_REQUEST);
}

/// Only an enrolled INSTANCE may retire, and only itself: every other
/// credential that verifies here is refused at the guard, and the instance row
/// is untouched. The control is the fixture's own instance retiring
/// successfully at the end, so the refusals are about the credentials and not
/// a route that refuses everyone.
#[ntex::test]
async fn no_other_credential_can_retire_an_instance() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;
    let (enroller_id, enroller_keyring) = seed_enroller_keyring(pg).await;
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");

    let retire = |header: String| {
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/internal/workers/retire")
                    .header("authorization", header)
                    .to_request(),
            )
            .await
            .status()
        }
    };
    let mut refused = Vec::new();
    for (label, header) in [
        ("the shared control key", format!("Bearer {CONTROL_KEY}")),
        ("the gateway", fixture.gateway_header()),
        ("a bare svc/worker role key", fixture.stale_worker_role_header()),
        (
            "an enroller",
            format!(
                "Bearer {}",
                enroller_keyring.mint_for(&control).expect("mint")
            ),
        ),
        ("an unrecorded instance key", fixture.planted_instance_header()),
    ] {
        refused.push((label, retire(header).await));
    }
    let untouched = instance_status(pg, &fixture.worker_instance_id).await;
    let own = retire(fixture.worker_header()).await;
    forget_enroller(pg, &enroller_id).await;
    fixture.release().await;

    for (label, status) in refused {
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{label} must not retire an instance");
    }
    assert_eq!(untouched.as_deref(), Some("active"));
    assert_eq!(own, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// Option 1A: the enroller cascade (success criteria 1 and 3)
// ---------------------------------------------------------------------------

/// Success criterion 1 (the red control's target) and the "Control internal
/// routes" half of criterion 3.
///
/// Revoking enroller E must, in one operator transaction:
///   - refuse E's already-enrolled instance at `CONTROL_APP_ENV`;
///   - refuse a FRESH enrolment attempt presenting E's instance's SAME public
///     key (the red control this arm is named for: on the pre-1A tree, a
///     revoked INSTANCE's process could re-enrol with the shared `svc/worker`
///     role key and read `CONTROL_APP_ENV` again under a fresh identity - see
///     this crate's own module header history and the design's "Verified
///     starting point" for the mechanism this closes);
///
/// while enroller F's instance is completely unaffected, which is the paired
/// control proving the refusal is about E and not a fault that would refuse
/// everyone.
#[ntex::test]
async fn revoking_an_enroller_refuses_its_instances_env_reads_and_its_new_enrolments() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;

    let (enroller_e, _e_keyring) = seed_enroller_keyring(pg).await;
    let (enroller_f, _f_keyring) = seed_enroller_keyring(pg).await;

    let key_e = InstanceSigningKey::generate();
    let public_e = *key_e.public_key();
    let instance_e = enrol_instance(&fixture.state, &enroller_e, &public_e).await;
    let keyring_e = key_e
        .into_keyring(
            instance_issuer(&instance_e),
            load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
        )
        .expect("instance keyring for E");

    let key_f = InstanceSigningKey::generate();
    let public_f = *key_f.public_key();
    let instance_f = enrol_instance(&fixture.state, &enroller_f, &public_f).await;
    let keyring_f = key_f
        .into_keyring(
            instance_issuer(&instance_f),
            load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
        )
        .expect("instance keyring for F");

    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let read_env = |keyring: &ServiceKeyring| {
        let app = &app;
        let header = format!("Bearer {}", keyring.mint_for(&control).expect("mint"));
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

    // BEFORE revocation: both instances reach the handler (400 on the
    // malformed id, which is the point - the guard admitted them).
    assert_eq!(read_env(&keyring_e).await, StatusCode::BAD_REQUEST);
    assert_eq!(read_env(&keyring_f).await, StatusCode::BAD_REQUEST);

    pg.execute("SELECT zeroship.revoke_worker_enroller($1)", &[&enroller_e])
        .await
        .expect("revoke_worker_enroller runs");

    // AFTER: E's instance is refused; F's is exactly as before. One variable
    // moved (which enroller was revoked), and only the rows under it changed.
    assert_eq!(
        read_env(&keyring_e).await,
        StatusCode::UNAUTHORIZED,
        "a revoked enroller's instance must lose CONTROL_APP_ENV access"
    );
    assert_eq!(
        read_env(&keyring_f).await,
        StatusCode::BAD_REQUEST,
        "an untouched enroller's instance must be unaffected by a sibling's revocation"
    );

    // THE RED CONTROL'S TARGET: E's SAME public key, presented in a fresh
    // enrolment request under the now-revoked E, must not mint a fresh active
    // identity that could read the environment again.
    let retry = enrol(
        &fixture.state,
        Some(ENROLMENT_PEER.parse().expect("peer socket parses")),
        &enroller_e,
        WorkerEnrolmentRequest {
            port: ADVERTISED_PORT,
            public_key: URL_SAFE_NO_PAD.encode(public_e),
        },
    )
    .await;
    assert_eq!(
        retry.status(),
        StatusCode::FORBIDDEN,
        "a revoked enroller must not be able to mint a fresh active instance"
    );

    forget(pg, &instance_e).await;
    forget(pg, &instance_f).await;
    forget_enroller(pg, &enroller_e).await;
    forget_enroller(pg, &enroller_f).await;
    fixture.release().await;
}
