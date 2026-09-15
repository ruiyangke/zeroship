//! What may open `/internal/apps/{id}/env` and `/internal/apps/{id}`, the
//! signer cascade that reaches them, and an instance retiring itself.
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
use zeroship_control::worker_join::{join, EnrolmentEnvelope, WorkerJoinRequest};
use zeroship_control::{
    internal, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::service_assertion::{
    ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey, ServiceTrustBundle,
};
use zeroship_core::service_peers::{
    load_peer_bundle, service_issuer, InstanceSigningKey, ServiceAuth, ServiceKeyring,
    CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN, WORKER_SERVICE_NAME,
};
use zeroship_core::typed_id::new_worker_instance_id;
use zeroship_core::worker_join::{
    join_proof_message, mint_join_token, JoinTokenGrant, DEFAULT_EXECUTION_ZONE,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const CONTROL_KEY: &str = "test-control-key";

/// The join declaration this fixture carries, and a peer inside it.
///
/// The instance arms need a row written by the PRODUCTION writer rather than an
/// INSERT of their own, so the key control resolves is the one a real worker
/// presented and the id is the one control minted.
const ENROLMENT_NETWORKS: &str = "10.7.0.0/16";
const ENROLMENT_PORTS: &str = "8080-8090";
const ENROLMENT_PEER: &str = "10.7.3.9:51314";
const ADVERTISED_PORT: u16 = 8080;

/// The deployment's single execution zone, seeded by
/// `db/migrations-ts/20260914000450_execution_zones_default_zone.ts`.
const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";

/// An instance identifier the OPERATOR'S peer document publishes a key for, and
/// that nothing ever joins.
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
/// - A key under the bare `svc/worker` ROLE: the stale file of a deployment
///   that once shipped a shared worker role key. No process holds one any more,
///   and Control must refuse it at role arity even though the file publishes
///   it - which, again, is only measurable while the key is really there.
fn write_service_keys(dir: &Path) -> (PathBuf, ServiceSigningKey) {
    let mut entries = Vec::new();
    for name in [
        CONTROL_SERVICE_NAME,
        WORKER_SERVICE_NAME,
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

/// A TRUSTED SIGNER as Control records it, with the private half this suite
/// mints join tokens with.
///
/// The row is inserted directly - the import that would write it is measured in
/// `join_signer_import_test` - so an arm here can choose a signer's status
/// without going through a file.
struct Signer {
    id: String,
    key: ServiceSigningKey,
}

impl Signer {
    /// A token this signer may mint, for the deployment's one zone.
    fn token(&self, uses: u32) -> String {
        mint_join_token(
            &self.id,
            &self.key,
            &service_issuer(CONTROL_SERVICE_NAME).expect("control issuer"),
            &JoinTokenGrant {
                zone: DEFAULT_EXECUTION_ZONE.to_owned(),
                lifetime: std::time::Duration::from_secs(300),
                uses,
                confirm: None,
            },
        )
        .expect("the grant mints")
    }
}

/// Insert one ACTIVE signer permitted for the deployment's one zone.
async fn seed_signer(pg: &compio_postgres::Client) -> Signer {
    let id = zeroship_core::typed_id::new_join_signer_id();
    let key = ServiceSigningKey::generate();
    pg.execute(
        "INSERT INTO zeroship.worker_join_signers (id, public_key, status) \
         VALUES ($1, $2, 'active')",
        &[&id, &key.verifying_key_bytes().as_slice()],
    )
    .await
    .expect("insert signer row");
    pg.execute(
        "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
         VALUES ($1, $2)",
        &[&id, &DEFAULT_ZONE_ID],
    )
    .await
    .expect("permit the signer for the default zone");
    Signer { id, key }
}

async fn forget_signer(pg: &compio_postgres::Client, signer_id: &str) {
    pg.execute(
        "DELETE FROM zeroship.worker_join_signer_zones WHERE signer_id = $1",
        &[&signer_id],
    )
    .await
    .expect("probe zone grants removed");
    pg.execute(
        "DELETE FROM zeroship.worker_join_signers WHERE id = $1",
        &[&signer_id],
    )
    .await
    .expect("probe signer row removed");
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
    /// A joined, ACTIVE worker instance: what every worker presents after its
    /// boot-time join, written by the production join path under
    /// [`Fixture::signer`].
    worker: ServiceKeyring,
    worker_instance_id: String,
    signer: Signer,
    /// A keyring minting under the bare `svc/worker` ROLE, on the key the
    /// operator's peer document publishes for it. The credential a shared role
    /// key used to be, and one Control must now refuse outright.
    stale_worker_role: ServiceKeyring,
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
    /// The header a joined worker presents. A FRESH assertion each call,
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

    /// Remove the rows this fixture created. Called at the end of each arm:
    /// `live_db.rs` shares one database, and an `active` instance left behind
    /// would be probed by every later liveness sweep.
    async fn release(&self) {
        forget(&self.state.control_pg, &self.worker_instance_id).await;
        forget_signer(&self.state.control_pg, &self.signer.id).await;
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
            control_key: SecretString::new(CONTROL_KEY.to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "https://api.stripe.com".to_string(),
            worker_urls: vec![],
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
            // DECLARED, not closed: the instance arms below join through the
            // production writer, so this fixture has to be a control plane that
            // can join at all.
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

    // The worker every arm presents is a REAL joined instance: a signer row, a
    // token that signer minted, then the production join path, then the keyring
    // the worker builds on the key it registered.
    let signer = seed_signer(&state.control_pg).await;
    let joiner = Joiner::random();
    let worker_instance_id = join_instance(&state, &signer.token(1), &joiner).await;
    let worker = joiner
        .into_keyring(
            instance_issuer(&worker_instance_id),
            load_peer_bundle(&peers).expect("peer bundle loads"),
        );

    Fixture {
        state,
        worker,
        worker_instance_id,
        signer,
        stale_worker_role: keyring_for(WORKER_SERVICE_NAME, &key_dir, &peers),
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
                    web::resource("/internal/workers/join")
                        .route(web::post().to(internal::join_worker_instance)),
                )
                .service(
                    web::resource("/internal/workers/renew")
                        .route(web::post().to(internal::renew_worker_instance)),
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
        "a joined worker instance's assertion must reach the handler"
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
// A worker mints under `svc/worker/<wkr_id>` after it joins, on a key that
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

/// The response body of an admitted join, drained so the fixture's
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

/// A worker's boot-drawn keypair, as this suite needs it: reusable, so one key
/// can both be presented at the join and mint assertions afterwards.
///
/// `InstanceSigningKey` deliberately cannot be cloned or re-read, which is right
/// for the worker and wrong for a fixture that must build the join proof and
/// then a keyring from one key. The wire shape is the same:
/// `crates/zeroship-worker/src/join.rs` builds the identical body from
/// `join_proof_message`.
struct Joiner {
    key: ed25519_dalek::SigningKey,
}

impl Joiner {
    fn random() -> Self {
        let mut seed = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        Self {
            key: ed25519_dalek::SigningKey::from_bytes(&seed),
        }
    }

    fn public(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    fn request(&self, token: &str) -> WorkerJoinRequest {
        use ed25519_dalek::Signer as _;
        let message = join_proof_message(token, &self.public(), ADVERTISED_PORT);
        WorkerJoinRequest {
            port: ADVERTISED_PORT,
            public_key: URL_SAFE_NO_PAD.encode(self.public()),
            proof: URL_SAFE_NO_PAD.encode(self.key.sign(&message).to_bytes()),
        }
    }

    /// The keyring this process would mint under once Control admitted it.
    fn into_keyring(self, issuer: ServiceIssuer, bundle: ServiceTrustBundle) -> ServiceKeyring {
        let der = self.key.to_pkcs8_der().expect("encode PKCS#8 DER");
        ServiceKeyring::from_parts(
            issuer,
            ServiceSigningKey::from_pkcs8_der(der.as_bytes())
                .expect("the same key, read back by the assertion layer"),
            bundle,
        )
        .expect("a boot-drawn key the document does not publish builds a keyring")
    }
}

/// Join one instance through the PRODUCTION writer with `token`, and return its
/// id.
///
/// Not an INSERT of this suite's own: the id, the ring key and the row shape
/// are control's, and a hand-written row would let these arms pass against a
/// registry the join path never produces.
async fn join_instance(state: &Arc<AppState>, token: &str, joiner: &Joiner) -> String {
    let response = join(
        state,
        Some(ENROLMENT_PEER.parse().expect("peer socket parses")),
        token,
        joiner.request(token),
    )
    .await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "the fixture must be able to join at all"
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

/// THE BEFORE/AFTER PAIR. One joined instance, one key, one route; the only
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
    let signer = seed_signer(&fixture.state.control_pg).await;

    let joiner = Joiner::random();
    let instance_id = join_instance(&fixture.state, &signer.token(1), &joiner).await;
    let keyring = joiner.into_keyring(
        instance_issuer(&instance_id),
        load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
    );

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
    forget_signer(pg, &signer.id).await;
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
    // THE CONTROL: a joined instance of the same role, on the same route.
    let joined = get(fixture.worker_header()).await;
    fixture.release().await;

    assert_eq!(
        stranger,
        StatusCode::UNAUTHORIZED,
        "an instance that never joined holds no credential here"
    );
    assert_eq!(
        planted,
        StatusCode::UNAUTHORIZED,
        "an instance key published in the OPERATOR'S peer document must not \
         authenticate without a row: the file carries no status, so a key \
         resolved from it could never be revoked"
    );
    assert_eq!(
        joined,
        StatusCode::BAD_REQUEST,
        "an instance answered from the registry must still authenticate, or the \
         two refusals above prove only that instances never do"
    );
}

/// The worker role has NO role arity: a `svc/worker` assertion minted under the
/// BARE role is refused, even though the operator's peer document publishes a
/// key for it.
///
/// This is the fence that makes "no process holds a `svc/worker` role key" a
/// property of Control rather than of every deployment's key hygiene. Before
/// it, a stale peer document still carrying the old shared role key let the
/// holder of its private half read any app's environment with no join, no
/// status, no lease and nothing a revocation could reach.
///
/// Each refusal is paired with the joined instance of the same role on the same
/// route, and the gateway's role-arity assertion is the control for the fence
/// itself: a role that does authenticate at role arity still verifies against
/// the SAME document (and is refused only by its missing grant), so the
/// refusals are about the worker role and not about role arity in general.
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
    let renew_with = |header: String| {
        let app = &app;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/internal/workers/renew")
                    .header("authorization", header)
                    .to_request(),
            )
            .await
            .status()
        }
    };

    let stale_role_env = read_env(fixture.stale_worker_role_header()).await;
    let joined_env = read_env(fixture.worker_header()).await;
    let stale_role_renew = renew_with(fixture.stale_worker_role_header()).await;
    // THE PAIRED CONTROL for renewal: the fixture's own live instance renews on
    // the same route, so the refusal above is the role arity rather than a
    // route that refuses everyone.
    let joined_renew = renew_with(fixture.worker_header()).await;
    let gateway = read_env(fixture.gateway_header()).await;
    fixture.release().await;

    assert_eq!(
        stale_role_env,
        StatusCode::UNAUTHORIZED,
        "a bare svc/worker assertion must be refused even with its key published"
    );
    assert_eq!(joined_env, StatusCode::BAD_REQUEST);
    assert_eq!(
        stale_role_renew,
        StatusCode::UNAUTHORIZED,
        "a bare svc/worker assertion must not renew an instance lease"
    );
    assert_eq!(joined_renew, StatusCode::OK);
    assert_eq!(
        gateway,
        StatusCode::UNAUTHORIZED,
        "the gateway verifies at role arity and holds no grant here"
    );
}

/// The JOIN route is not behind the assertion allowlist at all, and a service
/// assertion is not a join token.
///
/// A joining process has no service identity yet, so the route reads a bearer
/// join token under its own `typ`. That makes two refusals worth binding: a
/// verified service assertion presented as a token, and the shared control key.
/// The control is the same route admitting a real token.
#[ntex::test]
async fn the_join_route_takes_a_token_and_never_a_service_assertion() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let signer = seed_signer(&fixture.state.control_pg).await;
    let token = signer.token(2);

    let attempt = |header: String, joiner: &Joiner| {
        let app = &app;
        let request = joiner.request(&token);
        let body = serde_json::json!({
            "port": request.port,
            "public_key": request.public_key,
            "proof": request.proof,
        })
        .to_string();
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/internal/workers/join")
                    .header("authorization", header)
                    .header("content-type", "application/json")
                    .set_payload(body)
                    .to_request(),
            )
            .await
            .status()
        }
    };

    let as_assertion = attempt(fixture.gateway_header(), &Joiner::random()).await;
    let as_shared_key = attempt(format!("Bearer {CONTROL_KEY}"), &Joiner::random()).await;
    // THE CONTROL: the same route, the same body shape, a real token. This
    // request carries no observable peer (`TestRequest` drops it), so a token
    // that PASSES the token checks is refused 403 on the ADDRESS instead - a
    // verdict distinct from a refused credential, and reached only after the
    // token verified.
    let with_token = attempt(format!("Bearer {token}"), &Joiner::random()).await;
    let body = {
        let joiner = Joiner::random();
        let request = joiner.request(&token);
        let response = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/internal/workers/join")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .set_payload(
                    serde_json::json!({
                        "port": request.port,
                        "public_key": request.public_key,
                        "proof": request.proof,
                    })
                    .to_string(),
                )
                .to_request(),
        )
        .await;
        let bytes = test::read_body(response).await;
        serde_json::from_slice::<serde_json::Value>(&bytes).expect("json")
    };
    forget_signer(&fixture.state.control_pg, &signer.id).await;
    fixture.release().await;

    assert_eq!(
        as_assertion,
        StatusCode::FORBIDDEN,
        "a service assertion carries the wrong typ and is not a join token"
    );
    assert_eq!(as_shared_key, StatusCode::FORBIDDEN);
    assert_eq!(
        with_token,
        StatusCode::FORBIDDEN,
        "a real token reaches the address check"
    );
    assert_eq!(
        body["reason"], "peer_address_unobservable",
        "the token verified and the refusal is the address: {body}"
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
/// The retirement is SELF-SCOPED: a sibling instance admitted by the same
/// signer, joined beside it, is the paired control and must be untouched, which
/// is what shows the endpoint retires the caller rather than the signer's whole
/// fleet.
#[ntex::test]
async fn an_instance_retires_itself_and_only_itself() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;

    let sibling_joiner = Joiner::random();
    let sibling_id =
        join_instance(&fixture.state, &fixture.signer.token(1), &sibling_joiner).await;
    let sibling = sibling_joiner.into_keyring(
        instance_issuer(&sibling_id),
        load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
    );
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
        "retirement is the caller's alone, never its signer's whole fleet"
    );
    assert_eq!(sibling_read, StatusCode::BAD_REQUEST);
}

/// Only a joined INSTANCE may retire, and only itself: every other
/// credential that verifies here is refused at the guard, and the instance row
/// is untouched. The control is the fixture's own instance retiring
/// successfully at the end, so the refusals are about the credentials and not
/// a route that refuses everyone.
#[ntex::test]
async fn no_other_credential_can_retire_an_instance() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;
    let signer = seed_signer(pg).await;
    let join_token = signer.token(1);

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
        ("a join token", format!("Bearer {join_token}")),
        ("an unrecorded instance key", fixture.planted_instance_header()),
    ] {
        refused.push((label, retire(header).await));
    }
    let untouched = instance_status(pg, &fixture.worker_instance_id).await;
    let own = retire(fixture.worker_header()).await;
    forget_signer(pg, &signer.id).await;
    fixture.release().await;

    for (label, status) in refused {
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{label} must not retire an instance");
    }
    assert_eq!(untouched.as_deref(), Some("active"));
    assert_eq!(own, StatusCode::NO_CONTENT);
}

// ---------------------------------------------------------------------------
// The signer cascade: what rotate reaches and what purge reaches
// ---------------------------------------------------------------------------

/// The two signer verbs do NOT imply each other, measured on the credential
/// that matters: whether the instances a signer admitted can still read an
/// app's environment.
///
/// ROTATE is the hygiene path. It stops further tokens being minted under the
/// key and leaves the fleet serving, because one signer covers many deployment
/// units and retiring a key must not take all of them down.
///
/// PURGE is the incident path, for a key believed to have leaked: it retires
/// every instance the signer admitted in one transaction, so an operator is not
/// retiring instances one at a time while an attacker's workers keep serving.
///
/// Signer F is the paired control throughout - untouched, and unaffected by
/// either verb - which is what shows each refusal is about the signer acted on
/// rather than a fault that would refuse everyone.
#[ntex::test]
async fn rotating_a_signer_spares_its_fleet_and_purging_one_retires_it() {
    let fixture = build_fixture().await;
    let app = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;

    let signer_e = seed_signer(pg).await;
    let signer_f = seed_signer(pg).await;
    let rotate_token = signer_e.token(2);

    let joiner_e = Joiner::random();
    let instance_e = join_instance(&fixture.state, &rotate_token, &joiner_e).await;
    let keyring_e = joiner_e.into_keyring(
        instance_issuer(&instance_e),
        load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
    );

    let joiner_f = Joiner::random();
    let instance_f = join_instance(&fixture.state, &signer_f.token(1), &joiner_f).await;
    let keyring_f = joiner_f.into_keyring(
        instance_issuer(&instance_f),
        load_peer_bundle(&fixture.peers).expect("peer bundle loads"),
    );

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

    // BEFORE: both instances reach the handler (400 on the malformed id, which
    // is the point - the guard admitted them).
    assert_eq!(read_env(&keyring_e).await, StatusCode::BAD_REQUEST);
    assert_eq!(read_env(&keyring_f).await, StatusCode::BAD_REQUEST);

    pg.execute(
        "SELECT zeroship.rotate_worker_join_signer($1)",
        &[&signer_e.id],
    )
    .await
    .expect("rotate_worker_join_signer runs");

    // AFTER ROTATE: E's instance is UNAFFECTED. This is the half a reader is
    // most likely to get wrong, so it is asserted rather than left implied.
    assert_eq!(
        read_env(&keyring_e).await,
        StatusCode::BAD_REQUEST,
        "rotating a signer must not take down the fleet it admitted"
    );
    // But a token already minted under the rotated key admits nobody new: the
    // signer no longer resolves, whatever uses that token had left.
    let refused = join(
        &fixture.state,
        Some(ENROLMENT_PEER.parse().expect("peer socket parses")),
        &rotate_token,
        Joiner::random().request(&rotate_token),
    )
    .await;
    assert_eq!(
        refused.status(),
        StatusCode::FORBIDDEN,
        "a rotated signer must not be able to admit a fresh instance"
    );

    pg.execute(
        "SELECT zeroship.purge_worker_join_signer($1)",
        &[&signer_e.id],
    )
    .await
    .expect("purge_worker_join_signer runs");

    // AFTER PURGE: E's instance loses the credential, and F's is exactly as it
    // was throughout.
    assert_eq!(
        read_env(&keyring_e).await,
        StatusCode::UNAUTHORIZED,
        "a purged signer's instances must lose CONTROL_APP_ENV access"
    );
    assert_eq!(
        read_env(&keyring_f).await,
        StatusCode::BAD_REQUEST,
        "an untouched signer's instance must be unaffected by a sibling's purge"
    );

    forget(pg, &instance_e).await;
    forget(pg, &instance_f).await;
    forget_signer(pg, &signer_e.id).await;
    forget_signer(pg, &signer_f.id).await;
    fixture.release().await;
}

/// A worker serves one execution zone, so Control narrows its host app reads
/// to that zone.
///
/// The refusal and the acceptance differ in exactly one variable: the same
/// joined instance, the same route and the same process, against two apps
/// that differ only in the zone they were created in. An app's environment is
/// its decrypted secrets and its project data key is a decryption capability,
/// so a read that crossed zones would hand both to a fleet that must never
/// reach them.
#[ntex::test]
async fn host_app_reads_are_narrowed_to_the_callers_execution_zone() {
    let fixture = build_fixture().await;
    let service = internal_app!(Arc::clone(&fixture.state));
    let pg = &fixture.state.control_pg;
    let away_zone = "ezn_awayzone0000000000000000";
    pg.execute(
        "INSERT INTO zeroship.execution_zones(id, name, status) \
         VALUES ($1, 'away', 'active') ON CONFLICT (id) DO NOTHING",
        &[&away_zone],
    )
    .await
    .expect("declare a second execution zone");
    let plan = zeroship_control::plan_catalog::free_plan_id();
    let home = common::seed_app_in_zone(
        pg,
        &format!("zone-home-{}", &Uuid::new_v4().simple().to_string()[..10]),
        &plan,
        common::DEFAULT_EXECUTION_ZONE_ID,
    )
    .await;
    let away = common::seed_app_in_zone(
        pg,
        &format!("zone-away-{}", &Uuid::new_v4().simple().to_string()[..10]),
        &plan,
        away_zone,
    )
    .await;

    // THE REFUSAL: the away app is in a zone this instance does not belong to,
    // so neither its environment nor its data key is answered.
    for path in [
        format!("/internal/apps/{}/env", away.as_str()),
        format!("/internal/apps/{}", away.as_str()),
    ] {
        let refused = test::call_service(
            &service,
            test::TestRequest::get()
                .uri(&path)
                .header("authorization", fixture.worker_header())
                .to_request(),
        )
        .await;
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "{path} must refuse an app outside the caller's execution zone"
        );
    }

    // THE CONTROL: the same instance, the same routes, an app in its own zone.
    // The handler answers for itself, so the 403 above is the zone check's
    // verdict rather than anything about this caller or these routes.
    for path in [
        format!("/internal/apps/{}/env", home.as_str()),
        format!("/internal/apps/{}", home.as_str()),
    ] {
        let admitted = test::call_service(
            &service,
            test::TestRequest::get()
                .uri(&path)
                .header("authorization", fixture.worker_header())
                .to_request(),
        )
        .await;
        assert_ne!(
            admitted.status(),
            StatusCode::FORBIDDEN,
            "{path} must admit an app in the caller's own execution zone"
        );
    }
    fixture.release().await;
}
