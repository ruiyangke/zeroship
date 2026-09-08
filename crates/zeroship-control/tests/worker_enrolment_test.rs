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
//!   * that the HANDLER really reads `req.peer_addr()`.
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
use zeroship_core::service_assertion::ServiceAssertionVerifier;
use zeroship_core::service_peers::{
    service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME,
    WORKER_SERVICE_NAME,
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

/// THE CONTROL. Without an arm that succeeds, every refusal below passes
/// against an endpoint that refuses everything.
#[ntex::test]
async fn an_admitted_enrolment_records_the_derived_address_and_a_minted_ring_key() {
    let fixture = build_fixture(declared_envelope()).await;
    let key = instance_key(0x11);

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
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
            "SELECT advertise_host, advertise_port, public_key, ring_key, status \
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

    drop(fixture);
    common::drain_pg().await;
}

/// The end-to-end form of "the registrant contributes nothing to the ring key".
#[ntex::test]
async fn two_enrolments_with_identical_input_land_different_ring_keys() {
    let fixture = build_fixture(declared_envelope()).await;
    // BYTE-IDENTICAL caller input: the same instance key, the same claimed
    // port, from the same peer. Anything derived from what the caller sent
    // would come out the same twice.
    let key = instance_key(0x22);

    let mut ids = Vec::new();
    let mut ring_keys = Vec::new();
    for _ in 0..2 {
        let response = enrol(
            &fixture.state,
            peer(IN_ENVELOPE_PEER),
            request(&key, ADVERTISED_PORT),
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

    assert_ne!(ring_keys[0], ring_keys[1], "the ring key must not be derivable from the request");
    // And the SECOND row exists at all, which is this endpoint stating plainly
    // that it is not idempotent: a repeat is a second instance, not a lookup.
    // Nothing marks the first one `gone`; there is no reaper.
    assert_ne!(ids[0], ids[1]);

    for id in &ids {
        forget(&fixture.state.control_pg, id).await;
    }
    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn a_refused_enrolment_writes_no_row() {
    let fixture = build_fixture(declared_envelope()).await;
    let before = count_instances(&fixture.state.control_pg).await;

    // Three refusals, each one variable away from the control above: the peer,
    // the claimed port, and the declaration itself.
    let outside = enrol(
        &fixture.state,
        peer(OUT_OF_ENVELOPE_PEER),
        request(&instance_key(0x33), ADVERTISED_PORT),
    )
    .await;
    assert_eq!(outside.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(outside).await["reason"], "peer_outside_envelope");

    let bad_port = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
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
    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn an_undeclared_envelope_refuses_every_enrolment() {
    // ABSENCE REFUSES. This is the deployment-state arm, so it answers 503 and
    // points an operator at the configuration rather than at a credential.
    let fixture = build_fixture(EnrolmentEnvelope::closed()).await;
    let before = count_instances(&fixture.state.control_pg).await;

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        request(&instance_key(0x44), ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(response).await["reason"], "envelope_unset");
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

    drop(fixture);
    common::drain_pg().await;
}

macro_rules! enrol_app {
    ($state:expr) => {
        test::init_service(web::App::new().state($state).service(
            web::resource("/internal/workers/enrol")
                .route(web::post().to(internal::enrol_worker_instance)),
        ))
        .await
    };
}

#[ntex::test]
async fn only_the_worker_may_enrol_an_instance() {
    let fixture = build_fixture(declared_envelope()).await;
    let app = enrol_app!(Arc::clone(&fixture.state));
    let body = json!({"port": ADVERTISED_PORT, "public_key": URL_SAFE_NO_PAD.encode(instance_key(0x55))});

    // A response borrows the app state, so each arm keeps only its status:
    // a live `WebResponse` at the end of the body would still be holding this
    // fixture's Postgres client when `drain_pg` measures.
    let status = |header: String| {
        let app = &app;
        let body = &body;
        async move {
            test::call_service(
                app,
                test::TestRequest::post()
                    .uri("/internal/workers/enrol")
                    .header("authorization", header)
                    .set_json(body)
                    .to_request(),
            )
            .await
            .status()
        }
    };

    // The shared control key proves membership of a group, not an identity.
    assert_eq!(
        status(format!("Bearer {CONTROL_KEY}")).await,
        StatusCode::UNAUTHORIZED
    );

    // A gateway assertion VERIFIES under the same bundle and the same audience
    // and is still refused: `svc/gateway` holds no grant on this endpoint.
    assert_eq!(
        status(fixture.gateway_header()).await,
        StatusCode::UNAUTHORIZED
    );

    // THE CONTROL, one variable apart. The worker's assertion reaches the
    // handler, which refuses on the ADDRESS - 403 with a reason - rather than
    // on the credential. `TestRequest::peer_addr` is dropped by `to_request`,
    // so what an in-process request can reach is exactly this arm.
    assert_eq!(status(fixture.worker_header()).await, StatusCode::FORBIDDEN);

    drop(status);
    drop(app);
    drop(fixture);
    common::drain_pg().await;
}

/// The handler's read of the transport, bound over a REAL socket.
///
/// A loopback client gets `peer_is_loopback`, where the in-process arm above
/// gets a 403 for having no peer at all. A handler that ignored `req.peer_addr()`
/// could not tell those apart, so this pair is what makes the derivation's
/// input the connection rather than a default.
#[ntex::test]
async fn a_real_loopback_connection_is_refused_as_loopback() {
    let fixture = build_fixture(declared_envelope()).await;
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

    let mut response = server
        .post("/internal/workers/enrol")
        .header("authorization", fixture.worker_header())
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
        body["reason"], "peer_is_loopback",
        "a real connection must be judged on its observed peer, not on a default"
    );
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

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
    let before = count_instances(&fixture.state.control_pg).await;

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
        WorkerEnrolmentRequest {
            port: ADVERTISED_PORT,
            public_key: URL_SAFE_NO_PAD.encode([9_u8; PUBLIC_KEY_LENGTH - 1]),
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(count_instances(&fixture.state.control_pg).await, before);

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
/// This is the arm that binds it.
#[ntex::test]
async fn identity_and_address_are_frozen_after_enrolment() {
    let fixture = build_fixture(declared_envelope()).await;
    let key = instance_key(0x22);

    let response = enrol(
        &fixture.state,
        peer(IN_ENVELOPE_PEER),
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

    // Clean up BEFORE asserting: a failing assertion panics past anything after
    // it, which is how a probe row survives exactly the runs that matter.
    forget(pg, &instance_id).await;

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
    ] {
        assert!(
            outcome.is_err(),
            "{what} is documented as frozen at enrolment but the UPDATE succeeded"
        );
    }

    drop(fixture);
    common::drain_pg().await;
}
