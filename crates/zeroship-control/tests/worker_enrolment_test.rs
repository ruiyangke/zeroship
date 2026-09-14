//! What may enrol a worker instance, from where, and what row it leaves.
//!
//! The pure half of the derivation - the envelope grammar, the loopback fence,
//! the port bound, the IPv4-mapped canonicalisation - is exercised in
//! `crates/zeroship-control/src/worker_enrolment.rs`'s own test module, which
//! needs no database and runs under a bare `cargo test -p zeroship-control`.
//! What lives HERE is everything that cannot be stated without a real
//! PostgreSQL and a real ntex transport:
//!
//!   * that an admitted enrolment writes the derived address and the presented
//!     key into `zeroship.worker_instances`, and mints its own id and ring key;
//!   * that two enrolments carrying BYTE-IDENTICAL caller input still land two
//!     different ring keys, which is the end-to-end form of "the registrant
//!     contributes nothing to it";
//!   * that a refused enrolment writes nothing at all;
//!   * that the HANDLER really reads `req.peer_addr()`;
//!   * option 1A of the worker-enrollment-bootstrap design: an enroller's
//!     lock, idempotent enrolment on the instance public key, and the race
//!     between an in-flight enrolment and a concurrent revocation.
//!
//! THAT LAST ONE IS WHY THERE IS A LIVE SERVER IN THIS FILE. ntex's
//! `TestRequest::peer_addr` is dropped by `to_request` - its own unit test in
//! `web/test.rs` asserts `req.peer_addr() == None` after setting it - so an
//! in-process request can only ever reach the unobservable-peer arm, and a
//! handler that ignored the transport entirely would pass such a test. Driving
//! one request over a real socket gets a genuine loopback peer, and the refusal
//! comes back naming LOOPBACK rather than an absent peer. The two arms differ in
//! exactly one variable - whether a socket exists - so together they say the
//! handler reads the transport rather than defaulting.

use std::fs;
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::pkcs8::EncodePrivateKey as _;
use ed25519_dalek::PUBLIC_KEY_LENGTH;
use ntex::http::StatusCode;
use ntex::web::{self, test};
use rand::RngCore as _;
use serde_json::json;
use uuid::Uuid;

use zeroship_authn::service_replay::SharedClientReplayStore;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::worker_enrolment::{
    enrol, EnrolmentEnvelope, WorkerEnrolmentRequest, RING_KEY_BYTES,
};
use zeroship_control::{
    internal, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::service_assertion::{ServiceAssertionVerifier, ServiceIssuer, ServiceTrustBundle};
use zeroship_core::service_peers::{
    service_issuer, InstanceSigningKey, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME,
    GATEWAY_SERVICE_NAME, SERVICE_TRUST_DOMAIN, WORKER_ENROLLER_SERVICE_NAME, WORKER_SERVICE_NAME,
};

use crate::common;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const CONTROL_KEY: &str = "test-control-key";

/// The one declaration every arm shares. A peer inside it and a port inside it
/// is the control; each refusal moves exactly one of those.
const ENROLMENT_NETWORKS: &str = "10.7.0.0/16";
const ENROLMENT_PORTS: &str = "8080-8090";

/// A peer inside `ENROLMENT_NETWORKS`, with the EPHEMERAL source port a real
/// worker would connect from - which is deliberately not a port the envelope
/// permits, because the advertised port comes from the body and the host comes
/// from the socket.
const IN_ENVELOPE_PEER: &str = "10.7.3.9:51314";
const OUT_OF_ENVELOPE_PEER: &str = "203.0.113.9:51314";
const ADVERTISED_PORT: u16 = 8080;

/// The deployment's single execution zone, seeded by
/// `db/migrations-ts/20260914000000_execution_zones_and_worker_enrollers.ts`.
const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-worker-enrol-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

/// One ed25519 key per service plus one peer document naming every public half,
/// the shape `tests/lib/runtime_secrets.sh` writes for the end-to-end harnesses.
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

/// The instance identifier one enroller of `svc/worker-enroller` mints under.
fn enroller_issuer(enroller_id: &str) -> ServiceIssuer {
    ServiceIssuer::parse(&format!(
        "spiffe://{SERVICE_TRUST_DOMAIN}/{WORKER_ENROLLER_SERVICE_NAME}/{enroller_id}"
    ))
    .expect("an instance issuer parses")
}

/// The instance identifier one enrolled worker of `svc/worker` mints under.
fn worker_instance_issuer(instance_id: &str) -> ServiceIssuer {
    ServiceIssuer::parse(&format!(
        "spiffe://{SERVICE_TRUST_DOMAIN}/{WORKER_SERVICE_NAME}/{instance_id}"
    ))
    .expect("an instance issuer parses")
}

fn new_enroller_id() -> String {
    format!("wen_{}", &Uuid::new_v4().simple().to_string()[..25])
}

/// Insert one `zeroship.worker_enrollers` row directly. There is no import
/// mechanism built in this PoC (the design defers it, along with worker/CLI
/// wiring, as a follow-up), so tests seed the row the way an operator's import
/// file would, with a public key nothing here needs to hold the private half
/// of.
async fn seed_enroller(pg: &compio_postgres::Client, status: &str) -> String {
    let id = new_enroller_id();
    let mut public = [0_u8; PUBLIC_KEY_LENGTH];
    rand::rngs::OsRng.fill_bytes(&mut public);
    pg.execute(
        "INSERT INTO zeroship.worker_enrollers (id, public_key, execution_zone_id, status) \
         VALUES ($1, $2, $3, $4)",
        &[&id, &public.as_slice(), &DEFAULT_ZONE_ID, &status],
    )
    .await
    .expect("insert enroller row");
    id
}

/// Like [`seed_enroller`], but also returns a keyring that mints under the
/// enroller's own instance identifier, for arms that drive enrolment over real
/// HTTP rather than calling [`enrol`] directly.
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

async fn revoke_enroller(pg: &compio_postgres::Client, enroller_id: &str) {
    pg.execute("SELECT zeroship.revoke_worker_enroller($1)", &[&enroller_id])
        .await
        .expect("revoke_worker_enroller runs");
}

async fn forget_enroller(pg: &compio_postgres::Client, enroller_id: &str) {
    pg.execute(
        "DELETE FROM zeroship.worker_enrollers WHERE id = $1",
        &[&enroller_id],
    )
    .await
    .expect("probe enroller row removed");
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
    /// A FRESH assertion each call: the full profile burns the `jti`, and a
    /// cached header is exactly what the replay store refuses.
    fn worker_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.worker
            .mint_for(&control)
            .map(|a| format!("Bearer {a}"))
            .expect("mint")
    }

    fn gateway_header(&self) -> String {
        let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
        self.gateway
            .mint_for(&control)
            .map(|a| format!("Bearer {a}"))
            .expect("mint")
    }
}

async fn build_fixture(envelope: EnrolmentEnvelope) -> Fixture {
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
            worker_enrolment: envelope,
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
        blob_root,
        deploy_tmp_dir,
        key_dir,
    }
}

fn declared_envelope() -> EnrolmentEnvelope {
    EnrolmentEnvelope::parse(ENROLMENT_NETWORKS, ENROLMENT_PORTS, false)
        .expect("the declaration parses")
}

fn peer(text: &str) -> Option<SocketAddr> {
    Some(text.parse().expect("peer socket parses"))
}

/// A distinct instance key per call, so two enrolments in one test are two
/// instances rather than one repeated.
fn instance_key(fill: u8) -> [u8; PUBLIC_KEY_LENGTH] {
    [fill; PUBLIC_KEY_LENGTH]
}

fn request(key: &[u8; PUBLIC_KEY_LENGTH], port: u16) -> WorkerEnrolmentRequest {
    WorkerEnrolmentRequest {
        port,
        public_key: URL_SAFE_NO_PAD.encode(key),
    }
}

async fn body_json(mut response: web::HttpResponse) -> serde_json::Value {
    use ntex::util::{stream_recv, BytesMut};
    let mut body = response.take_body();
    let mut buf = BytesMut::new();
    while let Some(item) = stream_recv(&mut body).await {
        buf.extend_from_slice(&item.expect("body chunk"));
    }
    serde_json::from_slice(&buf).expect("body is JSON")
}

/// Rows this test minted, removed by id so a concurrent sibling's rows survive.
///
/// The table carries NO DELETE grant for `zeroship_control` by design - `gone`
/// is terminal and an instance ends by being marked, not erased. This suite
/// connects as the test DSN's own role, not as `zeroship_control`, so the
/// cleanup here is a test-harness capability and not evidence that the control
/// plane has one.
async fn forget(pg: &compio_postgres::Client, instance_id: &str) {
    pg.execute(
        "DELETE FROM zeroship.worker_instances WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("probe row removed");
}

async fn count_instances(pg: &compio_postgres::Client) -> i64 {
    pg.query_one("SELECT count(*) FROM zeroship.worker_instances", &[])
        .await
        .expect("count worker instances")
        .get(0)
}

async fn instance_status(pg: &compio_postgres::Client, instance_id: &str) -> Option<String> {
    pg.query(
        "SELECT status FROM zeroship.worker_instances WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("read instance status")
    .first()
    .map(|row| row.get(0))
}

/// THE CONTROL. Without an arm that succeeds, every refusal below passes
/// against an endpoint that refuses everything.
#[ntex::test]
async fn an_admitted_enrolment_records_the_derived_address_and_a_minted_ring_key() {
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let key = instance_key(0x11);

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&key, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let instance_id = body["instance_id"].as_str().expect("instance_id").to_string();
    assert!(instance_id.starts_with("wkr_"), "got {instance_id}");

    let row = fixture
        .state
        .control_pg
        .query_one(
            "SELECT advertise_host, advertise_port, public_key, ring_key, status, enroller_id \
             FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await
        .expect("the enrolled row is readable");

    // CLEAN UP BEFORE ASSERTING, not after. A failing assertion panics past
    // whatever follows it, so a `forget` at the end of the body leaves the probe
    // row behind on exactly the runs that matter - which is how a mutation run
    // of this suite left two rows in the registry for a later reader to find.
    forget(&fixture.state.control_pg, &instance_id).await;
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;

    // THE HOST IS THE OBSERVED PEER'S, THE PORT IS THE CALLER'S. That split is
    // the whole design: reading the port off the socket would advertise the
    // worker's ephemeral source port, and reading the host off the body would
    // let a role-key holder point dispatch at an address it chose.
    let host: std::net::IpAddr = row.get("advertise_host");
    assert_eq!(host, "10.7.3.9".parse::<std::net::IpAddr>().expect("host"));
    assert_eq!(row.get::<_, i32>("advertise_port"), i32::from(ADVERTISED_PORT));

    let stored_key: Vec<u8> = row.get("public_key");
    assert_eq!(stored_key, key.to_vec());

    let ring_key: Vec<u8> = row.get("ring_key");
    assert_eq!(ring_key.len(), RING_KEY_BYTES);
    assert_ne!(ring_key, vec![0_u8; RING_KEY_BYTES]);
    // The registrant sent the public key and nothing else that could seed a
    // ring position; the two must not be the same bytes.
    assert_ne!(ring_key, key.to_vec());

    assert_eq!(row.get::<_, &str>("status"), "active");
    assert_eq!(row.get::<_, &str>("enroller_id"), enroller_id);

    drop(fixture);
    common::drain_pg().await;
}

/// The end-to-end form of "the registrant contributes nothing to the ring key".
#[ntex::test]
async fn two_enrolments_with_identical_input_land_different_ring_keys() {
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    // BYTE-IDENTICAL caller input: the same instance key, the same claimed
    // port, from the same peer. Anything derived from what the caller sent
    // would come out the same twice. Distinct KEYS across the loop, because
    // enrolment is now idempotent on the public key and a repeated key would
    // exercise the retry path this test does not intend to.
    let mut ids = Vec::new();
    let mut ring_keys = Vec::new();
    for fill in [0x22_u8, 0x23_u8] {
        let response = enrol(
            &fixture.state,
            peer(IN_ENVELOPE_PEER),
            &enroller_id,
            request(&instance_key(fill), ADVERTISED_PORT),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let id = body_json(response).await["instance_id"]
            .as_str()
            .expect("instance_id")
            .to_string();
        let ring_key: Vec<u8> = fixture
            .state
            .control_pg
            .query_one(
                "SELECT ring_key FROM zeroship.worker_instances WHERE id = $1",
                &[&id],
            )
            .await
            .expect("enrolled row")
            .get("ring_key");
        ids.push(id);
        ring_keys.push(ring_key);
    }

    // Both rows go before either assertion fires; see the note in the arm above.
    for id in &ids {
        forget(&fixture.state.control_pg, id).await;
    }
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;

    assert_ne!(ring_keys[0], ring_keys[1], "the ring key must not be derivable from the request");
    assert_ne!(ids[0], ids[1]);

    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn a_refused_enrolment_writes_no_row() {
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let before = count_instances(&fixture.state.control_pg).await;

    // Three refusals, each one variable away from the control above: the peer,
    // the claimed port, and the declaration itself.
    let outside = enrol(
        &fixture.state,
        peer(OUT_OF_ENVELOPE_PEER),
        &enroller_id,
        request(&instance_key(0x33), ADVERTISED_PORT),
    )
    .await;
    assert_eq!(outside.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(outside).await["reason"], "peer_outside_envelope");

    let bad_port = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&instance_key(0x33), 9999),
    )
    .await;
    assert_eq!(bad_port.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(bad_port).await["reason"], "port_outside_envelope");

    assert_eq!(
        count_instances(&fixture.state.control_pg).await,
        before,
        "a refused enrolment must leave the registry untouched"
    );
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn an_undeclared_envelope_refuses_every_enrolment() {
    // ABSENCE REFUSES. This is the deployment-state arm, so it answers 503 and
    // points an operator at the configuration rather than at a credential.
    let fixture = build_fixture(EnrolmentEnvelope::closed()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let before = count_instances(&fixture.state.control_pg).await;

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&instance_key(0x44), ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(response).await["reason"], "envelope_unset");
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    drop(fixture);
    common::drain_pg().await;
}

/// Success criterion 2 of the option-1A PoC: an ACTIVE WORKER INSTANCE's own
/// assertion is refused at `CONTROL_WORKER_ENROL`, and the ENROLLER's
/// assertion is admitted. This is also the RENAMED and REWRITTEN control for
/// what this test used to check (`only_the_worker_may_enrol_an_instance`,
/// before `svc/worker` held the grant at all): the shared control key and the
/// gateway's assertion are unchanged refusals, but the worker's own assertion
/// moves from a 403 (reached the handler, refused on address) to a 401 (never
/// reaches the handler - `svc/worker` carries no grant on this endpoint under
/// any assertion it can mint, instance included).
///
/// Driven over a REAL socket (`test::server`, not `test::call_service`),
/// because the enroller's own successful enrolment needs `req.peer_addr()` to
/// be a real observed loopback address - `test::call_service`'s request has
/// none (`TestRequest::peer_addr` is dropped by `to_request`), so it can only
/// ever reach `peer_address_unobservable` and never actually admit an
/// enrolment. The envelope here declares `127.0.0.0/8` for exactly that
/// reason, matching [`a_real_loopback_connection_is_admitted_when_the_envelope_declares_it`].
#[ntex::test]
async fn an_active_instance_cannot_enrol_but_its_enroller_can() {
    let single_host = EnrolmentEnvelope::parse("127.0.0.0/8", ENROLMENT_PORTS, false)
        .expect("the declaration parses");
    let fixture = build_fixture(single_host).await;
    let (enroller_id, enroller_keyring) = seed_enroller_keyring(&fixture.state.control_pg).await;
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");

    let state = Arc::clone(&fixture.state);
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource("/internal/workers/enrol")
                    .route(web::post().to(internal::enrol_worker_instance)),
            )
        }
    })
    .await;

    let call = |header: String, key: [u8; PUBLIC_KEY_LENGTH]| {
        let server = &server;
        async move {
            server
                .post("/internal/workers/enrol")
                .header("authorization", header)
                .send_json(&json!({
                    "port": ADVERTISED_PORT,
                    "public_key": URL_SAFE_NO_PAD.encode(key),
                }))
                .await
                .expect("the enrolment response arrives")
        }
    };
    // The shared control key proves membership of a group, not an identity.
    assert_eq!(
        call(format!("Bearer {CONTROL_KEY}"), instance_key(0x50))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    // A gateway assertion VERIFIES under the same bundle and the same audience
    // and is still refused: `svc/gateway` holds no grant on this endpoint.
    assert_eq!(
        call(fixture.gateway_header(), instance_key(0x51))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // The enroller enrols a real instance, exactly as production would.
    let admitted = call(
        format!(
            "Bearer {}",
            enroller_keyring.mint_for(&control).expect("enroller mints")
        ),
        instance_key(0x52),
    )
    .await;
    assert_eq!(admitted.status(), StatusCode::CREATED);
    let instance_id = {
        let bytes = admitted.body().await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        body["instance_id"].as_str().expect("instance_id").to_string()
    };

    // THE RED CONTROL'S SHAPE: build a keyring under that exact instance's own
    // issuer (as if it were the worker itself, minting outbound with the key
    // whose public half it just enrolled) and try to reach this endpoint AS
    // AN ACTIVE, JUST-ENROLLED INSTANCE. `svc/worker` never held this grant
    // under a bare role key either, in the same allowlist row - see the
    // bare-role control below for that.
    let instance_key_pair = InstanceSigningKey::generate();
    let instance_keyring = instance_key_pair
        .into_keyring(
            worker_instance_issuer(&instance_id),
            ServiceTrustBundle::new(),
        )
        .expect("a boot-drawn key an empty bundle does not publish builds a keyring");
    assert_eq!(
        call(
            format!(
                "Bearer {}",
                instance_keyring.mint_for(&control).expect("instance mints")
            ),
            instance_key(0x53),
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED,
        "an active worker instance must not be able to enrol another instance"
    );

    // A bare `svc/worker` role assertion is refused the same way: no process
    // holds this key any more in the design this PoC proves, but the
    // allowlist row itself is what is under test here.
    assert_eq!(
        call(fixture.worker_header(), instance_key(0x54))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // THE CONTROL, one variable apart (a fresh instance key): the SAME
    // enroller assertion succeeds AGAIN, end to end - proving the refusals
    // above are about the CREDENTIAL presented, not about the enroller being
    // spent, revoked, or the server having stopped admitting anyone.
    let second = call(
        format!(
            "Bearer {}",
            enroller_keyring.mint_for(&control).expect("enroller mints")
        ),
        instance_key(0x55),
    )
    .await;
    assert_eq!(second.status(), StatusCode::CREATED);
    let second_instance_id = {
        let bytes = second.body().await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        body["instance_id"].as_str().expect("instance_id").to_string()
    };

    forget(&fixture.state.control_pg, &instance_id).await;
    forget(&fixture.state.control_pg, &second_instance_id).await;
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    drop(server);
    drop(fixture);
    common::drain_pg().await;
}

/// The handler's read of the transport, bound over a REAL socket.
///
/// A loopback client gets `peer_outside_envelope` against a declaration that
/// does not name loopback, where the in-process arm above gets a 401 for
/// having no grant at all under a non-enroller credential. This arm therefore
/// authenticates as the ENROLLER (the only principal that can reach the
/// address check at all under 1A) and differs from its sibling below only in
/// the declared networks.
///
/// Its own paired control is
/// [`a_real_loopback_connection_is_admitted_when_the_envelope_declares_it`],
/// which differs only in the declared networks.
#[ntex::test]
async fn a_real_loopback_connection_is_refused_as_outside_the_envelope() {
    let fixture = build_fixture(declared_envelope()).await;
    let (enroller_id, enroller_keyring) = seed_enroller_keyring(&fixture.state.control_pg).await;
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let before = count_instances(&fixture.state.control_pg).await;

    let state = Arc::clone(&fixture.state);
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource("/internal/workers/enrol")
                    .route(web::post().to(internal::enrol_worker_instance)),
            )
        }
    })
    .await;

    let response = server
        .post("/internal/workers/enrol")
        .header(
            "authorization",
            format!(
                "Bearer {}",
                enroller_keyring.mint_for(&control).expect("enroller mints")
            ),
        )
        .send_json(&json!({
            "port": ADVERTISED_PORT,
            "public_key": URL_SAFE_NO_PAD.encode(instance_key(0x66)),
        }))
        .await
        .expect("the enrolment response arrives");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value =
        serde_json::from_slice(&response.body().await.expect("body")).expect("json");
    assert_eq!(
        body["reason"], "peer_outside_envelope",
        "a real connection must be judged on its observed peer, not on a default"
    );
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    drop(server);
    drop(fixture);
    common::drain_pg().await;
}

/// A SINGLE-HOST DEPLOYMENT CAN ENROL, over a real socket, end to end.
///
/// This is the arm that would have caught the defect fixed on 2026-09-07: the
/// loopback refusal sat above the network comparison, so this case was
/// unreachable no matter what the operator declared, and every developer
/// machine and every worker-launching harness in `tests/` was unenrollable.
///
/// It differs from the refusal above in the DECLARED NETWORKS and nothing else.
/// Together they prove the rule consults the declaration rather than carrying a
/// verdict of its own - which a suite of refusals alone cannot show, because a
/// fence that refuses everything passes every one of them.
#[ntex::test]
async fn a_real_loopback_connection_is_admitted_when_the_envelope_declares_it() {
    let single_host = EnrolmentEnvelope::parse("127.0.0.0/8", ENROLMENT_PORTS, false)
        .expect("the declaration parses");
    let fixture = build_fixture(single_host).await;
    let (enroller_id, enroller_keyring) = seed_enroller_keyring(&fixture.state.control_pg).await;
    let control = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");

    let state = Arc::clone(&fixture.state);
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource("/internal/workers/enrol")
                    .route(web::post().to(internal::enrol_worker_instance)),
            )
        }
    })
    .await;

    let response = server
        .post("/internal/workers/enrol")
        .header(
            "authorization",
            format!(
                "Bearer {}",
                enroller_keyring.mint_for(&control).expect("enroller mints")
            ),
        )
        .send_json(&json!({
            "port": ADVERTISED_PORT,
            "public_key": URL_SAFE_NO_PAD.encode(instance_key(0x77)),
        }))
        .await
        .expect("the enrolment response arrives");
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: serde_json::Value =
        serde_json::from_slice(&response.body().await.expect("body")).expect("json");
    let instance_id = body["instance_id"].as_str().expect("instance_id").to_string();

    let row = fixture
        .state
        .control_pg
        .query_one(
            "SELECT advertise_host, advertise_port FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await
        .expect("the enrolled row is readable");

    // Cleaned up BEFORE the assertions, for the reason the admitted arm above
    // records: a panic skips whatever follows it.
    forget(&fixture.state.control_pg, &instance_id).await;
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;

    // The stored host is the loopback address the transport OBSERVED, not
    // anything the caller sent - the request body carries no host field at all.
    let host: std::net::IpAddr = row.get("advertise_host");
    assert_eq!(host, "127.0.0.1".parse::<std::net::IpAddr>().expect("host"));
    assert_eq!(row.get::<_, i32>("advertise_port"), i32::from(ADVERTISED_PORT));

    drop(server);
    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn a_public_key_that_is_not_an_ed25519_key_is_refused_before_any_derivation() {
    // Refused with 400 rather than 403: the body is malformed, which is a
    // different fact from the address being inadmissible, and an operator
    // reading a worker's logs should not go looking at network policy for it.
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let before = count_instances(&fixture.state.control_pg).await;

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        WorkerEnrolmentRequest {
            port: ADVERTISED_PORT,
            public_key: URL_SAFE_NO_PAD.encode([9_u8; PUBLIC_KEY_LENGTH - 1]),
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    drop(fixture);
    common::drain_pg().await;
}

/// The frozen-column trigger, which is the ONLY thing making the registry's
/// "immutable" and "insert-once" true rather than prose.
///
/// Control must hold UPDATE so `status` can progress, and a table grant cannot
/// be column-selective, so without the trigger any control-side path could
/// rotate a worker's ring position or swap its public key. The trigger was
/// driven by hand against live Postgres when it landed and bound by nothing.
/// This is the arm that binds it. `enroller_id` joined the frozen set in
/// option 1A: an instance's enroller is fixed at enrolment exactly like its
/// key and address.
#[ntex::test]
async fn identity_and_address_are_frozen_after_enrolment() {
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let other_enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let key = instance_key(0x22);

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&key, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let instance_id = body_json(response).await["instance_id"]
        .as_str()
        .expect("instance_id")
        .to_string();

    let pg = &fixture.state.control_pg;

    // THE ONE-VARIABLE CONTROL, TAKEN FIRST. `status` is the column that MAY
    // move, so a trigger that refused every UPDATE would satisfy all four
    // refusal arms below while silently breaking the lifecycle the column
    // exists for. Without this the arms prove only that writes fail.
    let progressed = pg
        .execute(
            "UPDATE zeroship.worker_instances SET status = 'draining' WHERE id = $1",
            &[&instance_id],
        )
        .await;

    // Each frozen column attempted on its own. These are autocommit statements,
    // so a refusal does not poison the attempts after it.
    let rotated_ring = pg
        .execute(
            "UPDATE zeroship.worker_instances SET ring_key = $2 WHERE id = $1",
            &[&instance_id, &vec![0x99_u8; RING_KEY_BYTES]],
        )
        .await;
    let swapped_key = pg
        .execute(
            "UPDATE zeroship.worker_instances SET public_key = $2 WHERE id = $1",
            &[&instance_id, &vec![0x55_u8; PUBLIC_KEY_LENGTH]],
        )
        .await;
    let moved_host = pg
        .execute(
            "UPDATE zeroship.worker_instances SET advertise_host = $2 WHERE id = $1",
            &[
                &instance_id,
                &"10.7.3.250".parse::<std::net::IpAddr>().expect("host"),
            ],
        )
        .await;
    let moved_port = pg
        .execute(
            "UPDATE zeroship.worker_instances SET advertise_port = $2 WHERE id = $1",
            &[&instance_id, &(i32::from(ADVERTISED_PORT) + 1)],
        )
        .await;
    let moved_enroller = pg
        .execute(
            "UPDATE zeroship.worker_instances SET enroller_id = $2 WHERE id = $1",
            &[&instance_id, &other_enroller_id],
        )
        .await;

    // Clean up BEFORE asserting: a failing assertion panics past anything after
    // it, which is how a probe row survives exactly the runs that matter.
    forget(pg, &instance_id).await;
    forget_enroller(pg, &enroller_id).await;
    forget_enroller(pg, &other_enroller_id).await;

    assert!(
        progressed.is_ok(),
        "status must still progress, or the trigger refuses everything and the \
         refusal arms below prove nothing: {progressed:?}"
    );
    for (what, outcome) in [
        ("ring_key", &rotated_ring),
        ("public_key", &swapped_key),
        ("advertise_host", &moved_host),
        ("advertise_port", &moved_port),
        ("enroller_id", &moved_enroller),
    ] {
        assert!(
            outcome.is_err(),
            "{what} is documented as frozen at enrolment but the UPDATE succeeded"
        );
    }

    drop(fixture);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// Option 1A success criteria 4 and 5
// ---------------------------------------------------------------------------

/// Success criterion 5: a lost-reply retry with the SAME public key returns
/// the SAME instance id, and the SAME key under a DIFFERENT enroller conflicts.
#[ntex::test]
async fn idempotent_retry_returns_the_same_instance_and_a_foreign_enroller_conflicts() {
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let other_enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    let key = instance_key(0x99);

    let first = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&key, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(first.status(), StatusCode::CREATED);
    let first_id = body_json(first).await["instance_id"]
        .as_str()
        .expect("instance_id")
        .to_string();

    // THE CONTROL: retrying with the SAME key under the SAME enroller is the
    // lost-reply case, and it must return the SAME id rather than minting a
    // second row.
    let retry = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&key, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(retry.status(), StatusCode::CREATED);
    let retry_id = body_json(retry).await["instance_id"]
        .as_str()
        .expect("instance_id")
        .to_string();
    assert_eq!(
        retry_id, first_id,
        "a retry with the same public key must return the existing instance id"
    );
    assert_eq!(
        count_instances(&fixture.state.control_pg).await
            - {
                // isolate this test's own rows from any sibling running
                // concurrently against the same database: count only rows
                // this enroller could have produced.
                let other: i64 = fixture
                    .state
                    .control_pg
                    .query_one(
                        "SELECT count(*) FROM zeroship.worker_instances WHERE id <> $1",
                        &[&first_id],
                    )
                    .await
                    .expect("count sibling rows")
                    .get(0);
                other
            },
        1,
        "a retried enrolment must not mint a second row"
    );

    // THE PAIRED FALSIFIER: the SAME key presented under a DIFFERENT enroller
    // is a conflict, not a silent reassignment.
    let foreign = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &other_enroller_id,
        request(&key, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(foreign.status(), StatusCode::CONFLICT);
    assert_eq!(
        body_json(foreign).await["reason"],
        "public_key_conflict"
    );

    // The original row is untouched by the conflicting attempt.
    assert_eq!(
        instance_status(&fixture.state.control_pg, &first_id).await.as_deref(),
        Some("active")
    );
    let owning_enroller: String = fixture
        .state
        .control_pg
        .query_one(
            "SELECT enroller_id FROM zeroship.worker_instances WHERE id = $1",
            &[&first_id],
        )
        .await
        .expect("row readable")
        .get(0);
    assert_eq!(owning_enroller, enroller_id);

    forget(&fixture.state.control_pg, &first_id).await;
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    forget_enroller(&fixture.state.control_pg, &other_enroller_id).await;
    drop(fixture);
    common::drain_pg().await;
}

/// A revoked enroller's own status refuses a brand-new enrolment attempt, with
/// no race involved - the simple half of success criterion 3 ("refuses E's new
/// enrollments"). [`revoking_an_enroller_while_an_enrolment_holds_its_lock_leaves_no_active_instance`]
/// below is the half that needs a race.
#[ntex::test]
async fn a_revoked_enroller_refuses_a_fresh_enrolment() {
    let fixture = build_fixture(declared_envelope()).await;
    let enroller_id = seed_enroller(&fixture.state.control_pg, "active").await;
    revoke_enroller(&fixture.state.control_pg, &enroller_id).await;
    let before = count_instances(&fixture.state.control_pg).await;

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &enroller_id,
        request(&instance_key(0xa0), ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(response).await["reason"], "enroller_inactive");
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

    // THE CONTROL: an enroller that was never touched stays able to enrol.
    let other_enroller = seed_enroller(&fixture.state.control_pg, "active").await;
    let admitted = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        &other_enroller,
        request(&instance_key(0xa1), ADVERTISED_PORT),
    )
    .await;
    assert_eq!(admitted.status(), StatusCode::CREATED);
    let instance_id = body_json(admitted).await["instance_id"]
        .as_str()
        .expect("instance_id")
        .to_string();

    forget(&fixture.state.control_pg, &instance_id).await;
    forget_enroller(&fixture.state.control_pg, &enroller_id).await;
    forget_enroller(&fixture.state.control_pg, &other_enroller).await;
    drop(fixture);
    common::drain_pg().await;
}

/// The blocking chain rooted at `blocker_pid`, in the pattern
/// `crates/zeroship-control/tests/organizations/concurrency.rs::wait_for_departures`
/// uses: follow `pg_blocking_pids` recursively, because PostgreSQL may queue a
/// waiter behind another waiter rather than directly behind the row's holder.
async fn count_blocked_on(observer: &compio_postgres::Client, blocker_pid: i32) -> i64 {
    observer
        .query_one(
            "WITH RECURSIVE blocked(pid) AS (
                 SELECT $1::integer
                 UNION
                 SELECT a.pid FROM pg_stat_activity a
                 JOIN blocked b ON b.pid = ANY(pg_blocking_pids(a.pid))
                 WHERE a.datname = current_database()
             ) SELECT count(*)::bigint FROM blocked WHERE pid <> $1",
            &[&blocker_pid],
        )
        .await
        .expect("count blocked backends")
        .get(0)
}

async fn wait_until_blocked_on(observer: &compio_postgres::Client, blocker_pid: i32) -> bool {
    compio::time::timeout(Duration::from_secs(15), async {
        loop {
            if count_blocked_on(observer, blocker_pid).await >= 1 {
                return;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// Success criterion 4: an enrolment transaction holds the enroller row lock
/// while a concurrent revocation waits, OBSERVED rather than assumed from a
/// sleep, and after both commit no active instance of the revoked enroller
/// exists.
///
/// The hold is built the same way
/// `organizations/concurrency.rs::concurrent_owner_departures_preserve_the_last_owner`
/// builds one: a raw connection opens an explicit transaction and takes
/// `SELECT ... FOR UPDATE` on the enroller row BEFORE either real operation
/// starts, so both `enrol_instance` (which locks the same row inside
/// `zeroship.enrol_worker_instance`) and `revoke_worker_enroller` (whose own
/// first UPDATE locks it too) queue behind ONE known backend. Once BOTH are
/// observed waiting, the blocker releases and PostgreSQL's row-lock queue
/// decides which of the two real operations goes first - the invariant this
/// arm checks holds under EITHER order, which is exactly the property option
/// 1A claims.
#[ntex::test]
async fn revoking_an_enroller_while_an_enrolment_holds_its_lock_leaves_no_active_instance() {
    let outcome = run_the_enrol_revoke_race().await;
    assert!(
        outcome.both_observed_blocked,
        "both the enrolment and the revocation must be seen waiting on the \
         fixture's row lock, or this arm is exercising a sleep instead of a lock"
    );
    assert!(
        !outcome.active_instance_of_revoked_enroller_survived,
        "no ordering of enrolment and revocation may leave an active instance \
         of a revoked enroller"
    );
}

struct RaceOutcome {
    both_observed_blocked: bool,
    active_instance_of_revoked_enroller_survived: bool,
}

/// Drive the race in [`revoking_an_enroller_while_an_enrolment_holds_its_lock_leaves_no_active_instance`]
/// against whatever `zeroship.enrol_worker_instance` / `revoke_worker_enroller`
/// definitions are CURRENTLY LIVE in the target database. Factored out so the
/// mutation control below can run the identical scenario against a
/// deliberately broken definition and require the OPPOSITE verdict.
async fn run_the_enrol_revoke_race() -> RaceOutcome {
    let db_url = common::require_control_db();
    let (pg_client, pg_conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("control-pg connect");
    let pg_driver = compio::runtime::spawn(async move { pg_conn.run().await });
    let enroller_id = seed_enroller(&pg_client, "active").await;

    let (mut blocker, blocker_conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("blocker connect");
    let blocker_driver = compio::runtime::spawn(async move { blocker_conn.run().await });
    let blocker_tx = blocker.transaction().await.expect("begin blocker tx");
    let blocker_pid: i32 = blocker_tx
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .expect("blocker pid")
        .get(0);
    blocker_tx
        .query_one(
            "SELECT id FROM zeroship.worker_enrollers WHERE id = $1 FOR UPDATE",
            &[&enroller_id],
        )
        .await
        .expect("blocker holds the enroller row lock");

    // A DEDICATED connection for the enrolment call, distinct from `pg_client`
    // (the observer below). PostgreSQL processes one connection's statements
    // strictly in the order they arrive at the backend: if the enrolment call
    // shares a connection with the observer polling `pg_blocking_pids`, the
    // observer's OWN next poll queues behind the enrolment's still-blocked
    // statement on that same connection and can never see it - the exact
    // self-deadlock this split exists to avoid. Measured: sharing one
    // connection between the two, `wait_until_blocked_on` timed out at its
    // full 15 seconds on every run, `queued` was always false, and the
    // enrolment only proceeded once ITS OWN internal timeout gave up and
    // let the join move on to releasing the blocker.
    let (enroller_session, enroller_conn) =
        compio_postgres::connect(&db_url, compio_postgres::NoTls)
            .await
            .expect("enroller session connect");
    let enroller_driver = compio::runtime::spawn(async move { enroller_conn.run().await });

    let mut public_key = [0_u8; PUBLIC_KEY_LENGTH];
    rand::rngs::OsRng.fill_bytes(&mut public_key);
    let host: std::net::IpAddr = "10.7.3.9".parse().expect("host");

    // Pre-extracted references, moved whole into each `async move` arm below.
    // Moving a `&T` is unambiguous to the borrow checker in a way that letting
    // three sibling futures each infer their own partial capture of the
    // enclosing locals is not, and this scenario has three arms doing exactly
    // that over the same handful of variables.
    let pg_client_ref = &pg_client;
    let enroller_session_ref = &enroller_session;
    let db_url_ref = &db_url;
    let enroller_id_ref = &enroller_id;
    let public_key_ref = &public_key;
    let host_ref = &host;

    let (queued, (_enrol_ok, instance_id), ()) = compio::time::timeout(
        Duration::from_secs(30),
        Box::pin(async move {
            futures::join!(
                async move {
                    let queued = wait_until_blocked_on(pg_client_ref, blocker_pid).await;
                    // Two waiters: the enrolment's guarded lock UPDATE and
                    // revocation's own first UPDATE, both against the same row.
                    let both = queued && count_blocked_on(pg_client_ref, blocker_pid).await >= 2;
                    blocker_tx.commit().await.expect("release the blocker lock");
                    both
                },
                async move {
                    let mut instance_id = zeroship_core::typed_id::new_worker_instance_id();
                    let ring_key = zeroship_control::worker_enrolment::mint_ring_key();
                    let row = enroller_session_ref
                        .query_one(
                            "SELECT zeroship.enrol_worker_instance($1, $2, $3, $4, $5, $6)",
                            &[
                                enroller_id_ref,
                                &instance_id,
                                &ring_key.as_slice(),
                                &public_key_ref.as_slice(),
                                host_ref,
                                &i32::from(ADVERTISED_PORT),
                            ],
                        )
                        .await;
                    if let Ok(row) = &row {
                        instance_id = row.get(0);
                    }
                    (row.is_ok(), instance_id)
                },
                async move {
                    let (revoker, revoker_conn) =
                        compio_postgres::connect(db_url_ref, compio_postgres::NoTls)
                            .await
                            .expect("revoker connect");
                    let revoker_driver =
                        compio::runtime::spawn(async move { revoker_conn.run().await });
                    revoker
                        .execute(
                            "SELECT zeroship.revoke_worker_enroller($1)",
                            &[enroller_id_ref],
                        )
                        .await
                        .expect("revoke_worker_enroller runs");
                    drop(revoker);
                    let _ = compio::time::timeout(Duration::from_secs(10), revoker_driver).await;
                }
            )
        }),
    )
    .await
    .expect("the race must resolve after the fixture releases its lock");

    let survived: i64 = pg_client
        .query_one(
            "SELECT count(*) FROM zeroship.worker_instances \
             WHERE enroller_id = $1 AND status = 'active'",
            &[&enroller_id],
        )
        .await
        .expect("count active instances of the enroller")
        .get(0);

    // Best-effort cleanup; the assertions in the caller do not depend on it.
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.worker_enrollers WHERE id = $1",
            &[&enroller_id],
        )
        .await;

    drop(blocker);
    drop(pg_client);
    drop(enroller_session);
    let _ = compio::time::timeout(Duration::from_secs(10), blocker_driver).await;
    let _ = compio::time::timeout(Duration::from_secs(10), pg_driver).await;
    let _ = compio::time::timeout(Duration::from_secs(10), enroller_driver).await;

    RaceOutcome {
        both_observed_blocked: queued,
        active_instance_of_revoked_enroller_survived: survived > 0,
    }
}
