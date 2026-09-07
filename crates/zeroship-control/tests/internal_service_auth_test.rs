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
use zeroship_control::{
    internal, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::service_assertion::ServiceAssertionVerifier;
use zeroship_core::service_peers::{
    service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME,
    WORKER_SERVICE_NAME,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const CONTROL_KEY: &str = "test-control-key";

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
fn write_service_keys(dir: &Path) -> PathBuf {
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
    let peers = dir.join("service-peers.json");
    fs::write(&peers, format!("{{\"keys\":[{}]}}", entries.join(","))).expect("write peers");
    peers
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
}

async fn build_fixture() -> Fixture {
    let db_url = common::require_control_db();
    let blob_root = tmpdir("blob");
    let deploy_tmp_dir = tmpdir("deploy");
    let key_dir = tmpdir("keys");
    let peers = write_service_keys(&key_dir);

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
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Https,
            trust_proxy: false,
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
            pairwise_salt: [0u8; 32],
            projected_charge_cache: std::sync::Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        }),
        worker: keyring_for(WORKER_SERVICE_NAME, &key_dir, &peers),
        gateway: keyring_for(GATEWAY_SERVICE_NAME, &key_dir, &peers),
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
