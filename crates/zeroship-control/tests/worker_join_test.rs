//! What may join a worker instance, from where, and what row it leaves.
//!
//! The pure half of the derivation - the envelope grammar, the loopback fence,
//! the port bound, the IPv4-mapped canonicalisation - is exercised in
//! `crates/zeroship-control/src/worker_join.rs`'s own test module, which needs
//! no database and runs under a bare `cargo test -p zeroship-control`. The
//! token grammar itself - signature, audience, expiry, lifetime, claims - is
//! exercised in `zeroship_core::worker_join`. What lives HERE is everything
//! that cannot be stated without a real `PostgreSQL` and a real ntex transport:
//!
//!   * that an admitted join writes the derived address, the presented key,
//!     the admitting signer and token, and the token's zone into
//!     `zeroship.worker_instances`, and mints its own id and ring key;
//!   * that two joins carrying BYTE-IDENTICAL caller input still land two
//!     different ring keys, which is the end-to-end form of "the registrant
//!     contributes nothing to it";
//!   * that a refused join writes nothing at all;
//!   * that the HANDLER really reads `req.peer_addr()`;
//!   * that a token's USES are consumed exactly, that a retry costs none, and
//!     that an exhausted token is refused;
//!   * that PROOF OF POSSESSION is what binds the presented key;
//!   * that the instance LEASE expires, renews monotonically, and is terminal
//!     once lapsed;
//!   * the signer's row lock, and the race between an in-flight join and a
//!     concurrent rotation.
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
use std::time::{Duration, SystemTime};

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
use zeroship_control::worker_join::{
    join, EnrolmentEnvelope, WorkerJoinRequest, RING_KEY_BYTES,
};
use zeroship_control::{
    internal, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::service_assertion::{
    thumbprint_key_id, ServiceAssertionVerifier, ServiceIssuer, ServiceSigningKey,
};
use zeroship_core::service_peers::{
    service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME, GATEWAY_SERVICE_NAME,
    WORKER_SERVICE_NAME,
};
use zeroship_core::worker_join::{
    join_proof_message, mint_join_token, mint_join_token_at, JoinTokenGrant,
    DEFAULT_EXECUTION_ZONE,
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
/// `db/migrations-ts/20260914000450_execution_zones_default_zone.ts`.
const DEFAULT_ZONE_ID: &str = "ezn_default000000000000000000";

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-worker-join-{label}-{}",
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

fn control_audience() -> ServiceIssuer {
    service_issuer(CONTROL_SERVICE_NAME).expect("control issuer")
}

/// A TRUSTED SIGNER as Control records it, with the private half this suite
/// mints tokens with.
///
/// The row is inserted directly - the import that would write it is measured in
/// `join_signer_import_test` - so an arm here can choose a signer's zones and
/// its status without going through a file.
struct Signer {
    id: String,
    key: ServiceSigningKey,
}

impl Signer {
    /// Mint a token for `zone` with `uses` uses, valid for a comfortable span.
    fn token(&self, zone: &str, uses: u32) -> String {
        self.grant(&JoinTokenGrant {
            zone: zone.to_owned(),
            lifetime: Duration::from_secs(300),
            uses,
            confirm: None,
        })
    }

    fn grant(&self, grant: &JoinTokenGrant) -> String {
        mint_join_token(&self.id, &self.key, &control_audience(), grant)
            .expect("the grant mints")
    }
}

/// Insert one signer, permitted for `zones`, at `status`.
async fn seed_signer(pg: &compio_postgres::Client, zones: &[&str], status: &str) -> Signer {
    let id = zeroship_core::typed_id::new_join_signer_id();
    let key = ServiceSigningKey::generate();
    pg.execute(
        "INSERT INTO zeroship.worker_join_signers (id, public_key, status) VALUES ($1, $2, $3)",
        &[&id, &key.verifying_key_bytes().as_slice(), &status],
    )
    .await
    .expect("insert signer row");
    for zone in zones {
        pg.execute(
            "INSERT INTO zeroship.worker_join_signer_zones (signer_id, execution_zone_id) \
             SELECT $1, id FROM zeroship.execution_zones WHERE name = $2",
            &[&id, zone],
        )
        .await
        .expect("permit the signer for the zone");
    }
    Signer { id, key }
}

/// An ACTIVE signer permitted for the deployment's one zone. What almost every
/// arm needs.
async fn seed_default_signer(pg: &compio_postgres::Client) -> Signer {
    seed_signer(pg, &[DEFAULT_EXECUTION_ZONE], "active").await
}

async fn rotate_signer(pg: &compio_postgres::Client, signer_id: &str) {
    pg.execute(
        "SELECT zeroship.rotate_worker_join_signer($1)",
        &[&signer_id],
    )
    .await
    .expect("rotate_worker_join_signer runs");
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

/// A worker's boot-drawn keypair, as this suite needs it: reusable across
/// several presentations, so the lost-reply retry and the foreign-signer
/// conflict can present the SAME key twice.
///
/// `InstanceSigningKey` deliberately cannot be cloned or re-read, which is
/// exactly right for the worker and exactly wrong for a test that must present
/// one key twice, so this holds the raw key instead. What it produces is the
/// same wire shape: `crates/zeroship-worker/src/join.rs` builds the identical
/// body from `join_proof_message`.
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

    fn public(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.key.verifying_key().to_bytes()
    }

    fn thumbprint(&self) -> String {
        thumbprint_key_id(&self.public())
    }

    /// The request body a worker holding this key would post for `token`.
    fn request(&self, token: &str, port: u16) -> WorkerJoinRequest {
        use ed25519_dalek::Signer as _;
        let message = join_proof_message(token, &self.public(), port);
        WorkerJoinRequest {
            port,
            public_key: URL_SAFE_NO_PAD.encode(self.public()),
            proof: URL_SAFE_NO_PAD.encode(self.key.sign(&message).to_bytes()),
        }
    }

    /// The same body as JSON, for the arms that go over a real socket.
    fn body(&self, token: &str, port: u16) -> serde_json::Value {
        let request = self.request(token, port);
        json!({
            "port": request.port,
            "public_key": request.public_key,
            "proof": request.proof,
        })
    }
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
        self.worker
            .mint_for(&control_audience())
            .map(|a| format!("Bearer {a}"))
            .expect("mint")
    }

    fn gateway_header(&self) -> String {
        self.gateway
            .mint_for(&control_audience())
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

/// The single-host declaration, which names loopback so a real socket can be
/// admitted.
fn loopback_envelope() -> EnrolmentEnvelope {
    EnrolmentEnvelope::parse("127.0.0.0/8", ENROLMENT_PORTS, false)
        .expect("the declaration parses")
}

fn peer(text: &str) -> SocketAddr {
    text.parse().expect("peer socket parses")
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

/// The instance id an admitted join returned.
async fn admitted_id(response: web::HttpResponse) -> String {
    assert_eq!(response.status(), StatusCode::CREATED);
    body_json(response).await["instance_id"]
        .as_str()
        .expect("instance_id")
        .to_string()
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

/// How many instances name `signer_id`.
///
/// PER SIGNER, NEVER TABLE-WIDE. `live_db.rs` shares one database and sibling
/// modules join and forget rows throughout, so a table-wide count taken before
/// a refusal and again after it measures their traffic as well as this test's -
/// and the arm then fails, or passes, on somebody else's cleanup. Every signer
/// here is minted per test, so counting under one is counting exactly what this
/// arm could have written.
async fn instances_of(pg: &compio_postgres::Client, signer_id: &str) -> i64 {
    pg.query_one(
        "SELECT count(*) FROM zeroship.worker_instances WHERE join_signer_id = $1",
        &[&signer_id],
    )
    .await
    .expect("count this signer's worker instances")
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

async fn instance_expiry(
    pg: &compio_postgres::Client,
    instance_id: &str,
) -> Option<std::time::SystemTime> {
    pg.query(
        "SELECT expires_at FROM zeroship.worker_instances WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("read instance expiry")
    .first()
    .map(|row| row.get(0))
}

/// THE CONTROL. Without an arm that succeeds, every refusal below passes
/// against an endpoint that refuses everything.
#[ntex::test]
async fn an_admitted_join_records_the_address_the_signer_and_the_token() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let joiner = Joiner::random();

    let response = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &token,
        joiner.request(&token, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = body_json(response).await;
    let instance_id = body["instance_id"].as_str().expect("instance_id").to_string();
    assert!(instance_id.starts_with("wkr_"), "got {instance_id}");
    // The lease the control plane granted is reported, so an operator reading a
    // boot log sees how long the identity lives without reading the source.
    assert!(
        body["lease_seconds"].as_u64().unwrap_or(0) > 0,
        "a join must report the lease it granted: {body}"
    );

    let row = pg
        .query_one(
            "SELECT advertise_host, advertise_port, public_key, ring_key, status, \
                    join_signer_id, join_token_id, execution_zone_id, expires_at > now() \
               FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await
        .expect("the joined row is readable");

    // CLEAN UP BEFORE ASSERTING, not after. A failing assertion panics past
    // whatever follows it, so a `forget` at the end of the body leaves the probe
    // row behind on exactly the runs that matter - which is how a mutation run
    // of this suite left two rows in the registry for a later reader to find.
    forget(pg, &instance_id).await;
    forget_signer(pg, &signer.id).await;

    // THE HOST IS THE OBSERVED PEER'S, THE PORT IS THE CALLER'S. That split is
    // the whole design: reading the port off the socket would advertise the
    // worker's ephemeral source port, and reading the host off the body would
    // let a token holder point dispatch at an address it chose.
    let host: std::net::IpAddr = row.get("advertise_host");
    assert_eq!(host, "10.7.3.9".parse::<std::net::IpAddr>().expect("host"));
    assert_eq!(row.get::<_, i32>("advertise_port"), i32::from(ADVERTISED_PORT));

    let stored_key: Vec<u8> = row.get("public_key");
    assert_eq!(stored_key, joiner.public().to_vec());

    let ring_key: Vec<u8> = row.get("ring_key");
    assert_eq!(ring_key.len(), RING_KEY_BYTES);
    assert_ne!(ring_key, vec![0_u8; RING_KEY_BYTES]);
    // The registrant sent the public key and nothing else that could seed a
    // ring position; the two must not be the same bytes.
    assert_ne!(ring_key, joiner.public().to_vec());

    assert_eq!(row.get::<_, &str>("status"), "active");
    // WHO VOUCHED FOR THIS WORKER, as a stored fact rather than an inference.
    assert_eq!(row.get::<_, &str>("join_signer_id"), signer.id);
    assert!(!row.get::<_, &str>("join_token_id").is_empty());
    // THE ZONE CAME FROM THE TOKEN. The request carries no zone field and there
    // is nowhere to add one.
    assert_eq!(row.get::<_, &str>("execution_zone_id"), DEFAULT_ZONE_ID);
    // THE LEASE. An identity admitted without one would never stop working on
    // its own, which is the whole reason the column exists.
    assert!(row.get::<_, bool>(8), "an admitted instance must be live");

    drop(fixture);
    common::drain_pg().await;
}

/// The end-to-end form of "the registrant contributes nothing to the ring key".
#[ntex::test]
async fn two_joins_with_identical_input_land_different_ring_keys() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 2);

    // The same claimed port, from the same peer, under the same token.
    // Anything derived from what the caller sent would come out the same twice.
    // Distinct KEYS across the loop, because joining is idempotent on the
    // public key and a repeated key would exercise the retry path this test
    // does not intend to.
    let mut ids = Vec::new();
    let mut ring_keys = Vec::new();
    for joiner in [Joiner::random(), Joiner::random()] {
        let response = join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            joiner.request(&token, ADVERTISED_PORT),
        )
        .await;
        let id = admitted_id(response).await;
        let ring_key: Vec<u8> = pg
            .query_one(
                "SELECT ring_key FROM zeroship.worker_instances WHERE id = $1",
                &[&id],
            )
            .await
            .expect("joined row")
            .get("ring_key");
        ids.push(id);
        ring_keys.push(ring_key);
    }

    // Both rows go before either assertion fires; see the note in the arm above.
    for id in &ids {
        forget(pg, id).await;
    }
    forget_signer(pg, &signer.id).await;

    assert_ne!(
        ring_keys[0], ring_keys[1],
        "the ring key must not be derivable from the request"
    );
    assert_ne!(ids[0], ids[1]);

    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn a_refused_join_writes_no_row() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 8);
    let before = instances_of(pg, &signer.id).await;

    // Two refusals, each one variable away from the control above: the peer and
    // the claimed port.
    let joiner = Joiner::random();
    let outside = join(
        &fixture.state,
        Some(peer(OUT_OF_ENVELOPE_PEER)),
        &token,
        joiner.request(&token, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(outside.status(), StatusCode::FORBIDDEN);
    assert_eq!(body_json(outside).await["reason"], "peer_outside_envelope");

    let bad_port = Joiner::random();
    let refused_port = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &token,
        bad_port.request(&token, 9999),
    )
    .await;
    assert_eq!(refused_port.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        body_json(refused_port).await["reason"],
        "port_outside_envelope"
    );

    assert_eq!(
        instances_of(pg, &signer.id).await,
        before,
        "a refused join must leave the registry untouched"
    );
    forget_signer(pg, &signer.id).await;
    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn an_undeclared_envelope_refuses_every_join() {
    // ABSENCE REFUSES. This is the deployment-state arm, so it answers 503 and
    // points an operator at the configuration rather than at a credential.
    let fixture = build_fixture(EnrolmentEnvelope::closed()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let before = instances_of(pg, &signer.id).await;

    let joiner = Joiner::random();
    let response = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &token,
        joiner.request(&token, ADVERTISED_PORT),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body_json(response).await["reason"], "envelope_unset");
    assert_eq!(instances_of(pg, &signer.id).await, before);

    forget_signer(pg, &signer.id).await;
    drop(fixture);
    common::drain_pg().await;
}

/// EVERY TOKEN REFUSAL, each one variable away from a control that is admitted.
///
/// The token grammar itself is bound in `zeroship_core::worker_join`; what this
/// arm adds is that Control APPLIES it against its own recorded registry - an
/// unknown signer resolves to no key, a rotated one resolves to nothing at all,
/// and a zone outside the signer's recorded set is refused even though the
/// token verifies perfectly.
#[ntex::test]
async fn a_token_control_cannot_trust_is_refused_and_writes_no_row() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;

    // A second declared zone, so a token can name a zone this signer may not
    // mint for. Zones are declared by migrations, never by Control.
    let other_zone = format!("probe-zone-{}", Uuid::new_v4().simple());
    let other_zone_id = zeroship_core::typed_id::generate("ezn");
    pg.execute(
        "INSERT INTO zeroship.execution_zones (id, name, status) VALUES ($1, $2, 'active')",
        &[&other_zone_id, &other_zone],
    )
    .await
    .expect("declare a second zone");

    let unrecorded = Signer {
        id: zeroship_core::typed_id::new_join_signer_id(),
        key: ServiceSigningKey::generate(),
    };
    let impostor = Signer {
        id: signer.id.clone(),
        key: ServiceSigningKey::generate(),
    };
    let rotated = seed_default_signer(pg).await;
    rotate_signer(pg, &rotated.id).await;

    let elsewhere = service_issuer(GATEWAY_SERVICE_NAME).expect("gateway issuer");
    let wrong_audience = mint_join_token(
        &signer.id,
        &signer.key,
        &elsewhere,
        &JoinTokenGrant {
            zone: DEFAULT_EXECUTION_ZONE.to_owned(),
            lifetime: Duration::from_secs(300),
            uses: 1,
            confirm: None,
        },
    )
    .expect("a token for another audience still mints");
    let long_expired = mint_join_token_at(
        &signer.id,
        &signer.key,
        &control_audience(),
        &JoinTokenGrant {
            zone: DEFAULT_EXECUTION_ZONE.to_owned(),
            lifetime: Duration::from_secs(60),
            uses: 1,
            confirm: None,
        },
        SystemTime::now() - Duration::from_secs(60 * 60),
    )
    .expect("a token minted an hour ago still mints");

    let cases: Vec<(&str, String, &str)> = vec![
        (
            "a signer Control has never recorded",
            unrecorded.token(DEFAULT_EXECUTION_ZONE, 1),
            "signer_unknown",
        ),
        (
            "a recorded id signed by another key",
            impostor.token(DEFAULT_EXECUTION_ZONE, 1),
            "token_signature",
        ),
        (
            "a rotated signer",
            rotated.token(DEFAULT_EXECUTION_ZONE, 1),
            "signer_unknown",
        ),
        ("another control plane's audience", wrong_audience, "token_audience"),
        ("a token whose expiry has passed", long_expired, "token_expired"),
        (
            "a zone this signer may not mint for",
            signer.token(&other_zone, 1),
            "zone_not_permitted",
        ),
        ("not a token at all", "not.a.token".to_owned(), "token_signer_malformed"),
        (
            "a service assertion presented as a join token",
            fixture
                .worker_header()
                .strip_prefix("Bearer ")
                .expect("the header carries an assertion")
                .to_owned(),
            "token_signer_malformed",
        ),
    ];

    let mut verdicts = Vec::new();
    for (label, token, _) in &cases {
        let joiner = Joiner::random();
        let response = join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            token,
            joiner.request(token, ADVERTISED_PORT),
        )
        .await;
        let status = response.status();
        verdicts.push((*label, status, body_json(response).await));
    }

    // THE CONTROL, one variable apart: the same signer, the same peer, the same
    // port, with a token it may actually mint. Without it every refusal above
    // would pass against an endpoint that refuses everyone.
    let joiner = Joiner::random();
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let admitted = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &token,
        joiner.request(&token, ADVERTISED_PORT),
    )
    .await;
    let admitted_status = admitted.status();
    let admitted_body = body_json(admitted).await;
    let instance_id = admitted_body["instance_id"].as_str().unwrap_or_default().to_owned();

    let written = instances_of(pg, &signer.id).await + instances_of(pg, &rotated.id).await;
    if !instance_id.is_empty() {
        forget(pg, &instance_id).await;
    }
    forget_signer(pg, &signer.id).await;
    forget_signer(pg, &rotated.id).await;
    pg.execute(
        "DELETE FROM zeroship.execution_zones WHERE id = $1",
        &[&other_zone_id],
    )
    .await
    .expect("remove the probe zone");

    for ((label, status, body), (_, _, reason)) in verdicts.into_iter().zip(&cases) {
        assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {body}");
        assert_eq!(body["reason"], *reason, "{label}");
    }
    assert_eq!(admitted_status, StatusCode::CREATED, "{admitted_body}");
    assert_eq!(written, 1, "only the control may have written a row");

    drop(fixture);
    common::drain_pg().await;
}

/// PROOF OF POSSESSION, which is what bounds a captured token.
///
/// Presenting a token without the private half of the key being registered
/// proves nothing, so a captor can admit workers it controls and nothing else.
/// Each refusal breaks exactly one of the three things the proof binds - the
/// token, the key, the port - and the paired `cnf` arms say a token minted for
/// a KNOWN key admits that key and no other.
#[ntex::test]
async fn a_join_not_signed_by_the_presented_key_is_refused() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 16);

    let holder = Joiner::random();
    let captor = Joiner::random();

    // The captor presents the HOLDER's public key with a proof made by its own
    // key. Without the proof check this would register a key the captor does
    // not hold and make Control attribute a worker to somebody else's material.
    let mut stolen = captor.request(&token, ADVERTISED_PORT);
    stolen.public_key = URL_SAFE_NO_PAD.encode(holder.public());

    // A proof made over a DIFFERENT token: a proof captured from one join must
    // not be replayable against another token the same key was offered.
    let other_token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let mut replayed = holder.request(&other_token, ADVERTISED_PORT);
    replayed.port = ADVERTISED_PORT;

    // A proof made over a DIFFERENT port: the one value the registrant
    // contributes to its own address is covered rather than left free for an
    // on-path party to change.
    let mut moved_port = holder.request(&token, ADVERTISED_PORT);
    moved_port.port = ADVERTISED_PORT + 1;

    let cases = [
        ("somebody else's public key", stolen),
        ("a proof over another token", replayed),
        ("a proof over another port", moved_port),
    ];
    let mut verdicts = Vec::new();
    for (label, request) in cases {
        let response = join(&fixture.state, Some(peer(IN_ENVELOPE_PEER)), &token, request).await;
        let status = response.status();
        verdicts.push((label, status, body_json(response).await));
    }

    // A token minted for a KNOWN key admits that key and no other.
    let confirmed_token = signer.grant(&JoinTokenGrant {
        zone: DEFAULT_EXECUTION_ZONE.to_owned(),
        lifetime: Duration::from_secs(300),
        uses: 4,
        confirm: Some(holder.public()),
    });
    let wrong_key = Joiner::random();
    let mismatched = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &confirmed_token,
        wrong_key.request(&confirmed_token, ADVERTISED_PORT),
    )
    .await;
    let mismatched_status = mismatched.status();
    let mismatched_body = body_json(mismatched).await;

    // THE PAIRED CONTROL for `cnf`: the confirmed key itself is admitted, so
    // the refusal above is the thumbprint comparison rather than a token nobody
    // can use.
    let confirmed = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &confirmed_token,
        holder.request(&confirmed_token, ADVERTISED_PORT),
    )
    .await;
    let confirmed_status = confirmed.status();
    let confirmed_body = body_json(confirmed).await;
    let instance_id = confirmed_body["instance_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    let written = instances_of(pg, &signer.id).await;
    if !instance_id.is_empty() {
        forget(pg, &instance_id).await;
    }
    forget_signer(pg, &signer.id).await;

    for (label, status, body) in verdicts {
        assert_eq!(status, StatusCode::FORBIDDEN, "{label}: {body}");
        assert_eq!(body["reason"], "proof_invalid", "{label}");
    }
    assert_eq!(mismatched_status, StatusCode::FORBIDDEN, "{mismatched_body}");
    assert_eq!(mismatched_body["reason"], "confirmation_mismatch");
    assert!(
        confirmed_body["instance_id"].as_str().is_some(),
        "{confirmed_body}"
    );
    assert_eq!(confirmed_status, StatusCode::CREATED);
    assert_eq!(
        holder.thumbprint(),
        thumbprint_key_id(&holder.public()),
        "the confirmed thumbprint is the joining key's own"
    );
    assert_eq!(written, 1, "only the confirmed key wrote a row");

    drop(fixture);
    common::drain_pg().await;
}

/// A TOKEN'S USES ARE EXACT, AND A RETRY COSTS NONE.
///
/// A token minted for two uses admits two workers and refuses the third, and a
/// worker that retries after a lost reply gets its own instance id back without
/// spending a second use - otherwise a single-use token could not survive one
/// dropped response.
#[ntex::test]
async fn a_token_admits_exactly_its_uses_and_a_retry_costs_none() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 2);

    let first = Joiner::random();
    let second = Joiner::random();
    let third = Joiner::random();

    let first_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            first.request(&token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;
    // THE LOST-REPLY RETRY, taken BEFORE the second worker: the same key under
    // the same token must return the same row and must not spend the use the
    // second worker is about to need.
    let retry_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            first.request(&token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;
    let second_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            second.request(&token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;
    let exhausted = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &token,
        third.request(&token, ADVERTISED_PORT),
    )
    .await;
    let exhausted_status = exhausted.status();
    let exhausted_body = body_json(exhausted).await;

    // A FRESH token from the SAME signer still admits, so the refusal above is
    // the token's budget rather than the signer being spent.
    let fresh = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let fourth = Joiner::random();
    let admitted = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &fresh,
        fourth.request(&fresh, ADVERTISED_PORT),
    )
    .await;
    let admitted_status = admitted.status();
    let fourth_id = body_json(admitted).await["instance_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();

    for id in [&first_id, &second_id, &fourth_id] {
        if !id.is_empty() {
            forget(pg, id).await;
        }
    }
    forget_signer(pg, &signer.id).await;

    assert_eq!(
        retry_id, first_id,
        "a retry with the same key must return the existing instance id"
    );
    assert_ne!(second_id, first_id);
    assert_eq!(exhausted_status, StatusCode::FORBIDDEN, "{exhausted_body}");
    assert_eq!(exhausted_body["reason"], "token_exhausted");
    assert_eq!(admitted_status, StatusCode::CREATED);

    drop(fixture);
    common::drain_pg().await;
}

/// The SAME key presented under a DIFFERENT signer is a conflict, refused
/// rather than silently reassigned.
#[ntex::test]
async fn one_key_joined_under_another_signer_conflicts() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let owner = seed_default_signer(pg).await;
    let other = seed_default_signer(pg).await;
    let owner_token = owner.token(DEFAULT_EXECUTION_ZONE, 4);
    let other_token = other.token(DEFAULT_EXECUTION_ZONE, 4);
    let joiner = Joiner::random();

    let instance_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &owner_token,
            joiner.request(&owner_token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;
    let foreign = join(
        &fixture.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &other_token,
        joiner.request(&other_token, ADVERTISED_PORT),
    )
    .await;
    let foreign_status = foreign.status();
    let foreign_body = body_json(foreign).await;

    let owning_signer: String = pg
        .query_one(
            "SELECT join_signer_id FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await
        .expect("row readable")
        .get(0);
    let status = instance_status(pg, &instance_id).await;

    forget(pg, &instance_id).await;
    forget_signer(pg, &owner.id).await;
    forget_signer(pg, &other.id).await;

    assert_eq!(foreign_status, StatusCode::CONFLICT, "{foreign_body}");
    assert_eq!(foreign_body["reason"], "public_key_conflict");
    assert_eq!(
        owning_signer, owner.id,
        "a conflicting attempt must not reassign the row"
    );
    assert_eq!(status.as_deref(), Some("active"));

    drop(fixture);
    common::drain_pg().await;
}

/// THE LEASE. An instance identity expires, the worker renews it, and renewal
/// is terminal once the identity has lapsed or been retired.
///
/// Renewal takes the LATER of the recorded expiry and the new one, so a slow
/// replica's in-flight renewal cannot shorten a window a newer one already
/// extended. That is measured by pushing the expiry far into the future behind
/// the renewal's back and requiring it to stay there.
#[ntex::test]
async fn an_instance_renews_its_own_lease_monotonically_until_it_lapses() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 4);
    let joiner = Joiner::random();

    let instance_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            joiner.request(&token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;

    // THE CONTROL: a live instance renews.
    let renewed = zeroship_control::worker_join::renew(&fixture.state, &instance_id).await;
    let renewed_status = renewed.status();

    // MONOTONIC. Push the expiry well past anything a renewal would compute,
    // then renew again: the column must not move backwards.
    pg.execute(
        "UPDATE zeroship.worker_instances SET expires_at = now() + interval '30 days' \
          WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("a newer replica extended the window");
    let far = instance_expiry(pg, &instance_id).await;
    let late = zeroship_control::worker_join::renew(&fixture.state, &instance_id).await;
    let late_status = late.status();
    let after_late = instance_expiry(pg, &instance_id).await;

    // LAPSED IS TERMINAL. An identity whose lease ran out cannot be revived:
    // the worker rejoins, which needs a token.
    pg.execute(
        "UPDATE zeroship.worker_instances SET expires_at = now() - interval '1 second' \
          WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("let the identity lapse");
    let lapsed = zeroship_control::worker_join::renew(&fixture.state, &instance_id).await;
    let lapsed_status = lapsed.status();
    let lapsed_body = body_json(lapsed).await;

    // A RETIRED instance cannot renew either, and an instance that never
    // existed is refused rather than created.
    let unknown = zeroship_control::worker_join::renew(
        &fixture.state,
        &zeroship_core::typed_id::new_worker_instance_id(),
    )
    .await;
    let unknown_status = unknown.status();

    forget(pg, &instance_id).await;
    forget_signer(pg, &signer.id).await;

    assert_eq!(renewed_status, StatusCode::OK);
    assert_eq!(late_status, StatusCode::OK);
    assert_eq!(
        after_late, far,
        "renewal must take the later of the two expiries, never move one back"
    );
    assert_eq!(lapsed_status, StatusCode::FORBIDDEN, "{lapsed_body}");
    assert_eq!(lapsed_body["reason"], "instance_not_live");
    assert_eq!(unknown_status, StatusCode::FORBIDDEN);

    drop(fixture);
    common::drain_pg().await;
}

/// A LAPSED INSTANCE STOPS AUTHENTICATING, with nobody acting.
///
/// This is what replaced "an instance row is live until an operator says
/// otherwise": the key resolution Control does before verifying any instance
/// assertion filters on the lease as well as on the status, so a crashed or
/// abandoned worker's credential dies on its own.
#[ntex::test]
async fn a_lapsed_instance_key_no_longer_resolves() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let joiner = Joiner::random();

    let instance_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            joiner.request(&token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;

    // THE CONTROL: while the lease is live the key resolves, and it is the key
    // the worker registered.
    let live = zeroship_control::worker_join::active_instance_public_key(pg, &instance_id).await;

    pg.execute(
        "UPDATE zeroship.worker_instances SET expires_at = now() - interval '1 second' \
          WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("let the identity lapse");
    let lapsed = zeroship_control::worker_join::active_instance_public_key(pg, &instance_id).await;

    // The other half of the same read: a RETIRED instance stops resolving too,
    // which is the fact `status` carries and the lease does not.
    pg.execute(
        "UPDATE zeroship.worker_instances \
            SET expires_at = now() + interval '1 hour', status = 'gone' WHERE id = $1",
        &[&instance_id],
    )
    .await
    .expect("retire the instance");
    let retired = zeroship_control::worker_join::active_instance_public_key(pg, &instance_id).await;

    forget(pg, &instance_id).await;
    forget_signer(pg, &signer.id).await;

    assert_eq!(live.expect("the registry is readable"), Some(joiner.public()));
    assert_eq!(
        lapsed.expect("the registry is readable"),
        None,
        "a lapsed lease must stop resolving"
    );
    assert_eq!(
        retired.expect("the registry is readable"),
        None,
        "a retired instance must stop resolving"
    );

    drop(fixture);
    common::drain_pg().await;
}

/// The join endpoint is NOT behind the service-assertion allowlist, and the
/// worker's other two routes ARE.
///
/// A joining process has no service identity yet, so the join route reads a
/// join token and nothing else. Renewal and retirement are the opposite: they
/// take no selector at all, so the instance they act on IS the issuer that
/// verified, and a caller with no instance identity reaches neither.
///
/// Driven over a REAL socket (`test::server`, not `test::call_service`), because
/// the successful join needs `req.peer_addr()` to be a real observed loopback
/// address - `test::call_service`'s request has none, so it can only ever reach
/// `peer_address_unobservable`.
#[ntex::test]
async fn joining_needs_a_token_and_renewing_needs_an_instance_identity() {
    let fixture = build_fixture(loopback_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 4);

    let state = Arc::clone(&fixture.state);
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new()
                .state(state)
                .service(
                    web::resource("/internal/workers/join")
                        .route(web::post().to(internal::join_worker_instance)),
                )
                .service(
                    web::resource("/internal/workers/renew")
                        .route(web::post().to(internal::renew_worker_instance)),
                )
        }
    })
    .await;

    // NO BEARER AT ALL: 401, naming the missing token rather than an address.
    let anonymous = server
        .post("/internal/workers/join")
        .send_json(&Joiner::random().body(&token, ADVERTISED_PORT))
        .await
        .expect("the join response arrives");
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);

    // THE SHARED CONTROL KEY proves membership of a group, not a grant to join.
    let shared = server
        .post("/internal/workers/join")
        .header("authorization", format!("Bearer {CONTROL_KEY}"))
        .send_json(&Joiner::random().body(&token, ADVERTISED_PORT))
        .await
        .expect("the join response arrives");
    assert_eq!(shared.status(), StatusCode::FORBIDDEN);

    // A GATEWAY ASSERTION verifies under the same bundle for the same audience
    // and is still not a join token: its `typ` is a service assertion's, so
    // Control's join verifier refuses it before any grant is consulted.
    let assertion = fixture.gateway_header();
    let as_assertion = server
        .post("/internal/workers/join")
        .header("authorization", assertion)
        .send_json(&Joiner::random().body(&token, ADVERTISED_PORT))
        .await
        .expect("the join response arrives");
    assert_eq!(as_assertion.status(), StatusCode::FORBIDDEN);

    // THE CONTROL: a real worker joins, over a real socket, end to end. The
    // stored host is the loopback address the transport OBSERVED, and the
    // request body carries no host field at all.
    let joiner = Joiner::random();
    let admitted = server
        .post("/internal/workers/join")
        .header("authorization", format!("Bearer {token}"))
        .send_json(&joiner.body(&token, ADVERTISED_PORT))
        .await
        .expect("the join response arrives");
    assert_eq!(admitted.status(), StatusCode::CREATED);
    let instance_id = {
        let bytes = admitted.body().await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        body["instance_id"].as_str().expect("instance_id").to_string()
    };
    let host: std::net::IpAddr = pg
        .query_one(
            "SELECT advertise_host FROM zeroship.worker_instances WHERE id = $1",
            &[&instance_id],
        )
        .await
        .expect("the joined row is readable")
        .get(0);

    // RENEWAL is the opposite shape: the join token that just worked is not a
    // service assertion and reaches nothing here.
    let renew_with_token = server
        .post("/internal/workers/renew")
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("the renewal response arrives");
    assert_eq!(renew_with_token.status(), StatusCode::UNAUTHORIZED);
    // Nor does a bare `svc/worker` role assertion, although this fixture's peer
    // document publishes that key: Control refuses the worker role at role
    // arity before any grant is consulted.
    let renew_as_role = server
        .post("/internal/workers/renew")
        .header("authorization", fixture.worker_header())
        .send()
        .await
        .expect("the renewal response arrives");
    assert_eq!(renew_as_role.status(), StatusCode::UNAUTHORIZED);

    forget(pg, &instance_id).await;
    forget_signer(pg, &signer.id).await;

    assert_eq!(host, "127.0.0.1".parse::<std::net::IpAddr>().expect("host"));

    drop(server);
    drop(fixture);
    common::drain_pg().await;
}

/// The handler's read of the transport, bound over a REAL socket.
///
/// A loopback client gets `peer_outside_envelope` against a declaration that
/// does not name loopback, where the in-process arms above can only reach
/// `peer_address_unobservable`. Its paired control is the admitted loopback
/// join in the arm above, which differs from this one in the declared networks
/// and nothing else - together they say the rule consults the declaration
/// rather than carrying a verdict of its own, which a suite of refusals alone
/// cannot show.
#[ntex::test]
async fn a_real_loopback_connection_is_refused_as_outside_the_envelope() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);

    let state = Arc::clone(&fixture.state);
    let server = test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource("/internal/workers/join")
                    .route(web::post().to(internal::join_worker_instance)),
            )
        }
    })
    .await;

    let response = server
        .post("/internal/workers/join")
        .header("authorization", format!("Bearer {token}"))
        .send_json(&Joiner::random().body(&token, ADVERTISED_PORT))
        .await
        .expect("the join response arrives");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body: serde_json::Value =
        serde_json::from_slice(&response.body().await.expect("body")).expect("json");
    assert_eq!(
        body["reason"], "peer_outside_envelope",
        "a real connection must be judged on its observed peer, not on a default"
    );
    assert_eq!(instances_of(pg, &signer.id).await, 0);

    forget_signer(pg, &signer.id).await;
    drop(server);
    drop(fixture);
    common::drain_pg().await;
}

#[ntex::test]
async fn a_malformed_body_is_refused_before_any_derivation() {
    // Refused with 400 rather than 403: the body is malformed, which is a
    // different fact from the address being inadmissible, and an operator
    // reading a worker's logs should not go looking at network policy for it.
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);

    let joiner = Joiner::random();
    let short_key = WorkerJoinRequest {
        port: ADVERTISED_PORT,
        public_key: URL_SAFE_NO_PAD.encode([9_u8; PUBLIC_KEY_LENGTH - 1]),
        proof: joiner.request(&token, ADVERTISED_PORT).proof,
    };
    let short_proof = WorkerJoinRequest {
        port: ADVERTISED_PORT,
        public_key: URL_SAFE_NO_PAD.encode(joiner.public()),
        proof: URL_SAFE_NO_PAD.encode([9_u8; 63]),
    };

    for (label, request) in [("a short key", short_key), ("a short proof", short_proof)] {
        let response = join(&fixture.state, Some(peer(IN_ENVELOPE_PEER)), &token, request).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{label}");
    }
    assert_eq!(instances_of(pg, &signer.id).await, 0);

    forget_signer(pg, &signer.id).await;
    drop(fixture);
    common::drain_pg().await;
}

/// The frozen-column trigger, which is the ONLY thing making the registry's
/// "immutable" and "insert-once" true rather than prose.
///
/// Control must hold UPDATE so `status` can progress and `expires_at` can be
/// renewed, and a table grant cannot be column-selective, so without the
/// trigger any control-side path could rotate a worker's ring position, swap
/// its public key, or move it into another zone. The admitting signer, the
/// admitting token and the zone joined the frozen set with the join contract:
/// an instance's provenance is fixed at join exactly like its key and address.
#[ntex::test]
async fn identity_zone_and_provenance_are_frozen_after_joining() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let other_signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
    let joiner = Joiner::random();

    let instance_id = admitted_id(
        join(
            &fixture.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            joiner.request(&token, ADVERTISED_PORT),
        )
        .await,
    )
    .await;

    let other_zone_id = zeroship_core::typed_id::generate("ezn");
    pg.execute(
        "INSERT INTO zeroship.execution_zones (id, name, status) VALUES ($1, $2, 'active')",
        &[
            &other_zone_id,
            &format!("probe-zone-{}", Uuid::new_v4().simple()),
        ],
    )
    .await
    .expect("declare a second zone");

    // THE ONE-VARIABLE CONTROLS, TAKEN FIRST. `status` and `expires_at` are the
    // columns that MAY move, so a trigger that refused every UPDATE would
    // satisfy all the refusal arms below while silently breaking the lifecycle
    // those columns exist for.
    let progressed = pg
        .execute(
            "UPDATE zeroship.worker_instances SET status = 'draining' WHERE id = $1",
            &[&instance_id],
        )
        .await;
    let extended = pg
        .execute(
            "UPDATE zeroship.worker_instances SET expires_at = now() + interval '1 hour' \
              WHERE id = $1",
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
    let moved_signer = pg
        .execute(
            "UPDATE zeroship.worker_instances SET join_signer_id = $2 WHERE id = $1",
            &[&instance_id, &other_signer.id],
        )
        .await;
    let moved_token = pg
        .execute(
            "UPDATE zeroship.worker_instances SET join_token_id = 'forged' WHERE id = $1",
            &[&instance_id],
        )
        .await;
    let moved_zone = pg
        .execute(
            "UPDATE zeroship.worker_instances SET execution_zone_id = $2 WHERE id = $1",
            &[&instance_id, &other_zone_id],
        )
        .await;

    // Clean up BEFORE asserting: a failing assertion panics past anything after
    // it, which is how a probe row survives exactly the runs that matter.
    forget(pg, &instance_id).await;
    forget_signer(pg, &signer.id).await;
    forget_signer(pg, &other_signer.id).await;
    pg.execute(
        "DELETE FROM zeroship.execution_zones WHERE id = $1",
        &[&other_zone_id],
    )
    .await
    .expect("remove the probe zone");

    assert!(
        progressed.is_ok(),
        "status must still progress, or the trigger refuses everything and the \
         refusal arms below prove nothing: {progressed:?}"
    );
    assert!(
        extended.is_ok(),
        "the lease must still be renewable, or renewal cannot work at all: {extended:?}"
    );
    for (what, outcome) in [
        ("ring_key", &rotated_ring),
        ("public_key", &swapped_key),
        ("advertise_host", &moved_host),
        ("advertise_port", &moved_port),
        ("join_signer_id", &moved_signer),
        ("join_token_id", &moved_token),
        ("execution_zone_id", &moved_zone),
    ] {
        assert!(
            outcome.is_err(),
            "{what} is documented as frozen at join but the UPDATE succeeded"
        );
    }

    drop(fixture);
    common::drain_pg().await;
}

/// PURGE retires what ROTATE leaves running, and the difference is the whole
/// reason there are two verbs.
///
/// Rotating a signer is the hygiene path: no further token can be minted under
/// it and the fleet it admitted keeps serving. Purging is the incident path,
/// for a key believed to have leaked, where an operator must not be retiring
/// instances one at a time while an attacker's workers keep serving.
#[ntex::test]
async fn rotate_leaves_the_fleet_running_and_purge_retires_it() {
    let fixture = build_fixture(declared_envelope()).await;
    let pg = &fixture.state.control_pg;
    let rotated = seed_default_signer(pg).await;
    let purged = seed_default_signer(pg).await;
    let bystander = seed_default_signer(pg).await;

    let mut instances = Vec::new();
    for signer in [&rotated, &purged, &bystander] {
        let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);
        let joiner = Joiner::random();
        instances.push(
            admitted_id(
                join(
                    &fixture.state,
                    Some(peer(IN_ENVELOPE_PEER)),
                    &token,
                    joiner.request(&token, ADVERTISED_PORT),
                )
                .await,
            )
            .await,
        );
    }

    rotate_signer(pg, &rotated.id).await;
    pg.execute(
        "SELECT zeroship.purge_worker_join_signer($1)",
        &[&purged.id],
    )
    .await
    .expect("purge_worker_join_signer runs");

    let statuses = [
        instance_status(pg, &instances[0]).await,
        instance_status(pg, &instances[1]).await,
        instance_status(pg, &instances[2]).await,
    ];

    for id in &instances {
        forget(pg, id).await;
    }
    for signer in [&rotated, &purged, &bystander] {
        forget_signer(pg, &signer.id).await;
    }

    assert_eq!(
        statuses[0].as_deref(),
        Some("active"),
        "rotating a key must not retire the fleet it admitted"
    );
    assert_eq!(
        statuses[1].as_deref(),
        Some("gone"),
        "purging a leaked key must retire everything it admitted"
    );
    assert_eq!(
        statuses[2].as_deref(),
        Some("active"),
        "neither verb may reach another signer's instances"
    );

    drop(fixture);
    common::drain_pg().await;
}

/// The blocking chain rooted at `blocker_pid`, in the pattern
/// `crates/zeroship-control/tests/organizations/concurrency.rs::wait_for_departures`
/// uses: follow `pg_blocking_pids` recursively, because `PostgreSQL` may queue a
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

/// Wait until at least `waiters` backends queue behind `blocker_pid`.
///
/// The count is the whole point: returning at the FIRST waiter and then
/// sampling for the second races the second waiter's own connection set-up,
/// and a loaded machine loses that race while the lock order is fine.
async fn wait_until_blocked_on(
    observer: &compio_postgres::Client,
    blocker_pid: i32,
    waiters: i64,
) -> bool {
    compio::time::timeout(Duration::from_secs(15), async {
        loop {
            if count_blocked_on(observer, blocker_pid).await >= waiters {
                return;
            }
            compio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// A join transaction holds the signer row lock while a concurrent PURGE
/// waits, OBSERVED rather than assumed from a sleep, and after both commit no
/// active instance of the purged signer exists.
///
/// PURGE IS THE VERB THIS INVARIANT BELONGS TO, and the distinction is the
/// whole point of having two. Rotate deliberately leaves the fleet running, so
/// "no active instance survives" is false of it by design and
/// [`rotate_leaves_the_fleet_running_and_purge_retires_it`] says so. Purge is
/// the incident path, and an ordering that let one instance slip through it
/// would leave an attacker's worker serving under a key the operator believes
/// they have destroyed.
///
/// The hold is built the same way
/// `organizations/concurrency.rs::concurrent_owner_departures_preserve_the_last_owner`
/// builds one: a raw connection opens an explicit transaction and takes
/// `SELECT ... FOR UPDATE` on the signer row BEFORE either real operation
/// starts, so both `zeroship.join_worker_instance` (which locks the same row)
/// and `zeroship.purge_worker_join_signer` (whose own first UPDATE locks it
/// too) queue behind ONE known backend. Once BOTH are observed waiting, the
/// blocker releases and `PostgreSQL`'s row-lock queue decides which of the two
/// real operations goes first. Both orders are admissible and the two of them
/// are exhaustive: either the join committed first and the purge's second
/// statement retires what it finds, or the purge committed first and the join's
/// guarded lock finds no active signer and refuses.
#[ntex::test]
async fn purging_a_signer_while_a_join_holds_its_lock_leaves_no_active_instance() {
    let outcome = run_the_join_purge_race().await;
    assert!(
        outcome.both_observed_blocked,
        "both the join and the purge must be seen waiting on the fixture's \
         row lock, or this arm is exercising a sleep instead of a lock"
    );
    assert!(
        !outcome.active_instance_of_purged_signer_survived,
        "no ordering of join and purge may leave an active instance of a \
         purged signer"
    );
    assert!(
        outcome.verdict_matches_the_registry,
        "a join that reported success must have left a row and one that \
         refused must have left none; anything else is a torn outcome"
    );
}

struct RaceOutcome {
    both_observed_blocked: bool,
    active_instance_of_purged_signer_survived: bool,
    /// Whether the join's own verdict and the row it left AGREE: a reported
    /// success with no row, or a refusal with one, is a torn outcome.
    verdict_matches_the_registry: bool,
}

/// Drive the race in [`purging_a_signer_while_a_join_holds_its_lock_leaves_no_active_instance`]
/// against whatever `zeroship.join_worker_instance` /
/// `zeroship.purge_worker_join_signer` definitions are CURRENTLY LIVE in the
/// target database.
async fn run_the_join_purge_race() -> RaceOutcome {
    let db_url = common::require_control_db();
    let (pg_client, pg_conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("control-pg connect");
    let pg_driver = compio::runtime::spawn(async move { pg_conn.run().await });
    let signer = seed_default_signer(&pg_client).await;

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
            "SELECT id FROM zeroship.worker_join_signers WHERE id = $1 FOR UPDATE",
            &[&signer.id],
        )
        .await
        .expect("blocker holds the signer row lock");

    // A DEDICATED connection for the join call, distinct from `pg_client` (the
    // observer below). PostgreSQL processes one connection's statements strictly
    // in the order they arrive at the backend: if the join call shares a
    // connection with the observer polling `pg_blocking_pids`, the observer's
    // OWN next poll queues behind the join's still-blocked statement on that
    // same connection and can never see it - the exact self-deadlock this split
    // exists to avoid.
    let (join_session, join_conn) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("join session connect");
    let join_driver = compio::runtime::spawn(async move { join_conn.run().await });

    let mut public_key = [0_u8; PUBLIC_KEY_LENGTH];
    rand::rngs::OsRng.fill_bytes(&mut public_key);
    let host: std::net::IpAddr = "10.7.3.9".parse().expect("host");
    let token_id = Uuid::new_v4().simple().to_string();

    // Pre-extracted references, moved whole into each `async move` arm below.
    // Moving a `&T` is unambiguous to the borrow checker in a way that letting
    // three sibling futures each infer their own partial capture of the
    // enclosing locals is not, and this scenario has three arms doing exactly
    // that over the same handful of variables.
    let pg_client_ref = &pg_client;
    let join_session_ref = &join_session;
    let db_url_ref = &db_url;
    let signer_id_ref = &signer.id;
    let token_id_ref = &token_id;
    let public_key_ref = &public_key;
    let host_ref = &host;

    let (queued, (join_ok, instance_id), ()) = compio::time::timeout(
        Duration::from_secs(30),
        Box::pin(async move {
            futures::join!(
                async move {
                    // Two waiters: the join's guarded lock UPDATE and the
                    // purge's own first UPDATE, both against the same row.
                    let both = wait_until_blocked_on(pg_client_ref, blocker_pid, 2).await;
                    blocker_tx.commit().await.expect("release the blocker lock");
                    both
                },
                async move {
                    let mut instance_id = zeroship_core::typed_id::new_worker_instance_id();
                    let ring_key = zeroship_control::worker_join::mint_ring_key();
                    let row = join_session_ref
                        .query_one(
                            "SELECT zeroship.join_worker_instance($1, $2, $3, $4, $5, $6, $7, $8, $9)",
                            &[
                                signer_id_ref,
                                token_id_ref,
                                &DEFAULT_ZONE_ID,
                                &instance_id,
                                &ring_key.as_slice(),
                                &public_key_ref.as_slice(),
                                host_ref,
                                &i32::from(ADVERTISED_PORT),
                                &600_i32,
                            ],
                        )
                        .await;
                    if let Ok(row) = &row {
                        instance_id = row.get(0);
                    }
                    (row.is_ok(), instance_id)
                },
                async move {
                    let (purger, purger_conn) =
                        compio_postgres::connect(db_url_ref, compio_postgres::NoTls)
                            .await
                            .expect("purger connect");
                    let purger_driver =
                        compio::runtime::spawn(async move { purger_conn.run().await });
                    purger
                        .execute(
                            "SELECT zeroship.purge_worker_join_signer($1)",
                            &[signer_id_ref],
                        )
                        .await
                        .expect("purge_worker_join_signer runs");
                    drop(purger);
                    let _ = compio::time::timeout(Duration::from_secs(10), purger_driver).await;
                }
            )
        }),
    )
    .await
    .expect("the race must resolve after the fixture releases its lock");

    let survived: i64 = pg_client
        .query_one(
            "SELECT count(*) FROM zeroship.worker_instances \
             WHERE join_signer_id = $1 AND status = 'active'",
            &[&signer.id],
        )
        .await
        .expect("count active instances of the signer")
        .get(0);
    let rows: i64 = pg_client
        .query_one(
            "SELECT count(*) FROM zeroship.worker_instances WHERE join_signer_id = $1",
            &[&signer.id],
        )
        .await
        .expect("count every instance of the signer")
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
            "DELETE FROM zeroship.worker_join_signer_zones WHERE signer_id = $1",
            &[&signer.id],
        )
        .await;
    let _ = pg_client
        .execute(
            "DELETE FROM zeroship.worker_join_signers WHERE id = $1",
            &[&signer.id],
        )
        .await;

    drop(blocker);
    drop(pg_client);
    drop(join_session);
    let _ = compio::time::timeout(Duration::from_secs(10), blocker_driver).await;
    let _ = compio::time::timeout(Duration::from_secs(10), pg_driver).await;
    let _ = compio::time::timeout(Duration::from_secs(10), join_driver).await;

    RaceOutcome {
        both_observed_blocked: queued,
        active_instance_of_purged_signer_survived: survived > 0,
        verdict_matches_the_registry: join_ok == (rows > 0),
    }
}

// ---------------------------------------------------------------------------
// The distributed rules, measured across TWO Control replicas
// ---------------------------------------------------------------------------
//
// Every arm above drives one `AppState` and therefore one PostgreSQL
// connection, which serializes its own statements: a race staged on it is a
// race the CLIENT resolved, not the database. Two fixtures are two connections
// and two replicas, which is the shape a real deployment has and the only shape
// in which "guarded write, never a read followed by a write" is falsifiable.

/// A TOKEN'S USES ARE EXACT HOWEVER MANY REPLICAS SEE IT AT ONCE.
///
/// Consuming a use is a guarded write rather than a read followed by a write,
/// so one use admits exactly one worker even when two replicas present two
/// different keys against it simultaneously. A read-then-write would let both
/// read "unused" and both admit.
#[ntex::test]
async fn two_replicas_racing_one_use_admit_exactly_one_worker() {
    let left = build_fixture(declared_envelope()).await;
    let right = build_fixture(declared_envelope()).await;
    let signer = seed_default_signer(&left.state.control_pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 1);

    let first = Joiner::random();
    let second = Joiner::random();
    let (a, b) = futures::join!(
        join(
            &left.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            first.request(&token, ADVERTISED_PORT),
        ),
        join(
            &right.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            second.request(&token, ADVERTISED_PORT),
        )
    );
    let (a_status, b_status) = (a.status(), b.status());
    let (a_body, b_body) = (body_json(a).await, body_json(b).await);

    let mut admitted = Vec::new();
    for body in [&a_body, &b_body] {
        if let Some(id) = body["instance_id"].as_str() {
            admitted.push(id.to_owned());
        }
    }
    for id in &admitted {
        forget(&left.state.control_pg, id).await;
    }
    forget_signer(&left.state.control_pg, &signer.id).await;

    assert_eq!(
        admitted.len(),
        1,
        "one use must admit exactly one worker: {a_status} {a_body} / {b_status} {b_body}"
    );
    let refused = if a_status == StatusCode::CREATED { &b_body } else { &a_body };
    assert_eq!(refused["reason"], "token_exhausted", "{refused}");

    drop(left);
    drop(right);
    common::drain_pg().await;
}

/// A LOST-REPLY RETRY THAT LANDS ON A DIFFERENT REPLICA CONVERGES ON ONE ROW.
///
/// The instance public key is UNIQUE, so two replicas admitting the same key at
/// once mint one identity rather than two. Without that constraint one process
/// would hold two instance rows, each independently retirable and neither
/// retiring the other - so revoking the worker would stop half of it.
///
/// The retry also costs no second use, which is what makes a single-use token
/// survive a dropped response.
#[ntex::test]
async fn two_replicas_racing_one_key_converge_on_one_instance() {
    let left = build_fixture(declared_envelope()).await;
    let right = build_fixture(declared_envelope()).await;
    let pg = &left.state.control_pg;
    let signer = seed_default_signer(pg).await;
    let token = signer.token(DEFAULT_EXECUTION_ZONE, 2);
    let joiner = Joiner::random();

    let (a, b) = futures::join!(
        join(
            &left.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            joiner.request(&token, ADVERTISED_PORT),
        ),
        join(
            &right.state,
            Some(peer(IN_ENVELOPE_PEER)),
            &token,
            joiner.request(&token, ADVERTISED_PORT),
        )
    );
    let (a_status, b_status) = (a.status(), b.status());
    let (a_body, b_body) = (body_json(a).await, body_json(b).await);

    let rows: i64 = pg
        .query_one(
            "SELECT count(*) FROM zeroship.worker_instances WHERE public_key = $1",
            &[&joiner.public().as_slice()],
        )
        .await
        .expect("count rows for the joining key")
        .get(0);
    // A SECOND worker on the remaining use, so "the retry cost no use" is
    // measured rather than assumed.
    let sibling = Joiner::random();
    let second = join(
        &left.state,
        Some(peer(IN_ENVELOPE_PEER)),
        &token,
        sibling.request(&token, ADVERTISED_PORT),
    )
    .await;
    let second_status = second.status();
    let second_body = body_json(second).await;

    for body in [&a_body, &b_body, &second_body] {
        if let Some(id) = body["instance_id"].as_str() {
            forget(pg, id).await;
        }
    }
    forget_signer(pg, &signer.id).await;

    assert_eq!(a_status, StatusCode::CREATED, "{a_body}");
    assert_eq!(b_status, StatusCode::CREATED, "{b_body}");
    assert_eq!(
        a_body["instance_id"], b_body["instance_id"],
        "two replicas admitting one key must converge on one instance id"
    );
    assert_eq!(rows, 1, "one process must hold exactly one instance row");
    assert_eq!(
        second_status,
        StatusCode::CREATED,
        "the retry must not have spent the second use: {second_body}"
    );

    drop(left);
    drop(right);
    common::drain_pg().await;
}

/// THE MINTER IS A LEASED ROLE, AND ONLY THE HOLDER WRITES.
///
/// Deployments run several Control replicas against one database, and two of
/// them rotating the same volume would write over each other. Without the lease
/// both replicas would answer "I am the minter" and both would write.
///
/// The lease ends with the SESSION, so a holder that dies drops it: that half
/// is measured by closing the holder's connection and requiring the next asker
/// to succeed, which is what makes a dead minter recoverable without anyone
/// acting.
#[compio::test]
async fn only_one_control_replica_holds_the_join_token_minter_lease() {
    use zeroship_control::join_minter::claim_minter_lease;

    let url = common::require_control_db();
    let open = || async {
        let (client, connection) = compio_postgres::connect(&url, compio_postgres::NoTls)
            .await
            .expect("replica connect");
        let driver = compio::runtime::spawn(async move { connection.run().await });
        (client, driver)
    };

    let (holder, holder_driver) = open().await;
    let (standby, standby_driver) = open().await;

    let held = claim_minter_lease(&holder).await;
    let refused = claim_minter_lease(&standby).await;
    // The holder re-asking gets the lease again: the lock is re-entrant within
    // a session, which is how a replica notices on every tick that it is still
    // the minter without any bookkeeping of its own.
    let held_again = claim_minter_lease(&holder).await;

    // The holder dies. Its session ends, the lock goes with it, and the next
    // tick elects the standby.
    drop(holder);
    let _ = compio::time::timeout(Duration::from_secs(10), holder_driver).await;
    let succeeded = compio::time::timeout(Duration::from_secs(15), async {
        loop {
            match claim_minter_lease(&standby).await {
                Ok(true) => return true,
                Ok(false) => compio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);

    let _ = standby
        .execute("SELECT pg_advisory_unlock_all()", &[])
        .await;
    drop(standby);
    let _ = compio::time::timeout(Duration::from_secs(10), standby_driver).await;

    assert_eq!(held, Ok(true), "the first asker becomes the minter");
    assert_eq!(
        refused,
        Ok(false),
        "a second replica must stand by rather than write the same file"
    );
    assert_eq!(held_again, Ok(true), "the holder re-asking is still the minter");
    assert!(
        succeeded,
        "a minter that dies must drop its lease with its session"
    );

    common::drain_pg().await;
}
