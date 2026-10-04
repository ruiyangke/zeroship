//! Real workflow services and deploy artifact, owned by each acceptance test.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::support::workflow_postgres::{self, Database};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use serde_json::{json, Value};
use testcontainers::{core::WaitFor, runners::SyncRunner, Container, GenericImage, ImageExt};
use uuid::Uuid;
use zeroship_core::service_assertion::ServiceSigningKey;
use zeroship_core::service_peers::service_issuer;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::{AppId, BindingId, DatabaseId, UserId};

pub const CONTROL_KEY: &str = "test-control-key";
const MASTER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn key(service: &str) -> ed25519_dalek::SigningKey {
    let seed = match service {
        "control" => 31,
        "gateway" => 32,
        "join-signer" => 34,
        // The workflow manager is an ordinary peer: it holds a role key and
        // verifies joined worker instances from the platform registry.
        "workflow" => 35,
        _ => panic!("unknown fixture service"),
    };
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn signing_key(service: &str) -> ServiceSigningKey {
    ServiceSigningKey::from_pkcs8_der(key(service).to_pkcs8_der().unwrap().as_bytes()).unwrap()
}

/// The credential a gateway mints for a worker dispatch, from the same key the
/// fleet's gateway process holds. A test that has to reach the worker without
/// the gateway's route table mints the identical assertion rather than a
/// weaker stand-in.
pub fn worker_dispatch_authorization() -> String {
    use zeroship_core::service_assertion::ServiceTrustBundle;
    use zeroship_core::service_peers::{service_issuer, ServiceKeyring};
    let gateway = ServiceKeyring::from_parts(
        service_issuer("svc/gateway").unwrap(),
        signing_key("gateway"),
        ServiceTrustBundle::new(),
    )
    .unwrap();
    let worker = service_issuer(zeroship_core::service_peers::WORKER_SERVICE_NAME).unwrap();
    format!("Bearer {}", gateway.mint_for(&worker).unwrap())
}

fn binaries() -> &'static BTreeMap<String, PathBuf> {
    static BINARIES: OnceLock<BTreeMap<String, PathBuf>> = OnceLock::new();
    BINARIES.get_or_init(zeroship_testkit::prebuilt::resolve_all)
}

/// Run one fixture statement on the fleet's platform database.
async fn execute(url: &str, sql: &str) {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .expect("fleet database connection");
    let driver = compio::runtime::spawn(connection.run());
    client.batch_execute(sql).await.expect("fleet fixture DDL");
    drop(client);
    driver.await.expect("fleet database task").expect("driver");
}

/// Port reservations held from [`port`] until [`release`], keyed by port.
static RESERVED: std::sync::Mutex<Vec<(u16, TcpListener)>> =
    std::sync::Mutex::new(Vec::new());

/// A port reserved for one fleet child, held until that child is spawned.
///
/// A port picked and released immediately can be handed to a sibling process
/// between the pick and the child's bind, which is how a fleet's child dies
/// with `Address already in use` before its readiness poll. The reservation
/// holds the listener until [`release`] drops it: two picks for one fleet are
/// distinct, and the window a sibling can take a port is the spawn itself.
pub fn port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve a fleet port");
    let port = listener.local_addr().expect("reserved port").port();
    RESERVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((port, listener));
    port
}

/// Drop the reservation for `port` so the child about to bind it can.
fn release(port: u16) {
    let mut reserved = RESERVED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(index) = reserved.iter().position(|(held, _)| *held == port) {
        reserved.swap_remove(index);
    }
}

/// What this fleet runs beyond the always-present services.
#[derive(Clone, Copy, Debug, Default)]
pub struct FleetOptions {
    /// Run a workflow manager, point Control's lifecycle publisher at it, and
    /// give the worker a workflow host registered with it.
    pub workflow_manager: bool,
    /// Reserve a second worker that is NOT started at launch, and point the
    /// gateway's route list only at it. A test starts it later, so a run can be
    /// started through the first worker's dispatch frame and finished by a
    /// worker the gateway routes to but that never ran the app.
    pub deferred_second_worker: bool,
}

#[derive(Debug)]
pub struct Fleet {
    work: tempfile::TempDir,
    _issuer: Container<GenericImage>,
    issuer_url: String,
    children: Vec<(String, Child)>,
    pub logs: PathBuf,
    pub database: Database,
    pub control_url: String,
    pub gateway_url: String,
    pub worker_url: String,
    /// The port and URL of the deferred second worker, present only when
    /// [`FleetOptions::deferred_second_worker`] asked for one. Started by
    /// [`Fleet::start_second_worker`], not at launch.
    second_worker_port: Option<u16>,
    pub second_worker_url: Option<String>,
    /// The CDC relay port and blob root every worker spawn reuses.
    relay_port: u16,
    blobs: String,
    /// Present only when [`FleetOptions::workflow_manager`] asked for one.
    pub manager_url: Option<String>,
    pub app_id: AppId,
    /// The creator database this fleet's app is bound to, on the creator zone.
    pub creator_database: DatabaseId,
    /// The app's read-write binding to [`Self::creator_database`]: the role
    /// the worker narrows to before it reaches that database's tables.
    pub binding: BindingId,
    pub deploy_id: String,
    pub blob_root: PathBuf,
}

impl Fleet {
    /// A fleet with no workflow manager: nothing places an app, so no host
    /// serves the workflow namespace.
    pub fn start() -> Self {
        Self::with(FleetOptions::default())
    }

    /// A fleet whose worker runs a workflow host against a real manager.
    pub fn with_workflow_manager() -> Self {
        Self::with(FleetOptions {
            workflow_manager: true,
            deferred_second_worker: false,
        })
    }

    /// A workflow fleet with a second worker reserved but not started. The
    /// gateway's route list names only the second worker.
    pub fn with_deferred_second_worker() -> Self {
        Self::with(FleetOptions {
            workflow_manager: true,
            deferred_second_worker: true,
        })
    }

    pub fn with(options: FleetOptions) -> Self {
        std::thread::spawn(move || {
            compio::runtime::Runtime::new()
                .expect("fleet runtime")
                .block_on(Self::launch(options))
        })
        .join()
        .unwrap_or_else(|error| std::panic::resume_unwind(error))
    }

    async fn launch(options: FleetOptions) -> Self {
        let binaries = binaries();
        let database = Database::new();
        let work = tempfile::tempdir().expect("workflow fleet directory");
        let logs = workflow_postgres::root()
            .join("target/workflow-tests")
            .join(Uuid::new_v4().to_string());
        fs::create_dir_all(&logs).unwrap();
        eprintln!("workflow fleet logs: {}", logs.display());
        let jwks = json!({"keys":[{"kty":"OKP", "crv":"Ed25519", "alg":"EdDSA", "use":"sig", "kid":"workflow-issuer", "x":signing_key("control").public_jwk_x()}]});
        let issuer = GenericImage::new("nginx", "1")
            .with_wait_for(WaitFor::message_on_stderr("start worker processes"))
            .with_exposed_port(testcontainers::core::IntoContainerPort::tcp(80))
            .with_copy_to(
                "/usr/share/nginx/html/.well-known/jwks.json",
                serde_json::to_vec(&jwks).unwrap(),
            )
            .start()
            .expect("workflow issuer container");
        let issuer_url = format!(
            "http://{}:{}",
            issuer.get_host().unwrap(),
            issuer.get_host_port_ipv4(80).unwrap()
        );
        let worker_port = port();
        let second_worker_port = options.deferred_second_worker.then(port);
        let mut fleet = Self {
            _issuer: issuer,
            issuer_url,
            blob_root: work.path().join("blobs"),
            work,
            children: vec![],
            logs,
            database,
            control_url: format!("http://127.0.0.1:{}", port()),
            gateway_url: format!("http://127.0.0.1:{}", port()),
            worker_url: format!("http://127.0.0.1:{worker_port}"),
            second_worker_port,
            second_worker_url: second_worker_port
                .map(|second| format!("http://127.0.0.1:{second}")),
            relay_port: 0,
            blobs: String::new(),
            manager_url: options
                .workflow_manager
                .then(|| format!("http://127.0.0.1:{}", port())),
            app_id: AppId::mint(),
            creator_database: DatabaseId::mint(),
            binding: BindingId::mint(),
            deploy_id: String::new(),
        };
        fs::create_dir_all(&fleet.blob_root).unwrap();
        let mut peer_keys = vec![];
        // The worker is not a peer with a key of its own: it joins with a
        // TOKEN and mints under an instance key it draws in memory at boot, so
        // the document publishes no `svc/worker` key.
        for name in ["control", "gateway", "workflow"] {
            fleet.secret(
                &format!("{name}.pem"),
                key(name)
                    .to_pkcs8_pem(Default::default())
                    .unwrap()
                    .as_bytes(),
            );
            peer_keys.push(json!({"kty":"OKP", "crv":"Ed25519", "x":signing_key(name).public_jwk_x(), "iss":service_issuer(&format!("svc/{name}")).unwrap().as_str()}));
        }
        fleet.secret(
            "peers.json",
            &serde_json::to_vec(&json!({"keys":peer_keys})).unwrap(),
        );
        // The fleet's trusted signer, and Control's import file naming its
        // public half. Control is also the MINTER here, exactly as a
        // single-host deployment configures it: it rotates a token into a file
        // the worker reads at boot, so this fixture exercises the real minting
        // path rather than a token written by the test.
        let signer_id = zeroship_core::typed_id::new_join_signer_id();
        fleet.secret(
            "join-signer.json",
            &serde_json::to_vec(&json!({
                "signer_id": signer_id,
                "private_key": key("join-signer").to_pkcs8_pem(Default::default()).unwrap().as_str(),
            }))
            .unwrap(),
        );
        fleet.secret(
            "join-signers.json",
            &serde_json::to_vec(&json!({"signers": [{
                "id": signer_id,
                "zones": ["default"],
                "public_key": signing_key("join-signer").public_jwk_x(),
            }]}))
            .unwrap(),
        );
        fleet.secret("broker", b"workflow-fixture-broker-secret-for-gateway");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        fleet.secret("relay-cert.pem", cert.cert.pem().as_bytes());
        fleet.secret("relay-key.pem", cert.signing_key.serialize_pem().as_bytes());
        let relay_port = port();
        fleet.relay_port = relay_port;
        let relay_addr = format!("127.0.0.1:{relay_port}");
        release(relay_port);
        fleet.spawn(
            "relay",
            &binaries["zeroship-data-cdc-server"],
            &[
                "--no-config".into(),
                "--listen".into(),
                relay_addr.clone(),
                "--tls-cert-file".into(),
                fleet.path("relay-cert.pem"),
                "--tls-key-file".into(),
                fleet.path("relay-key.pem"),
            ],
            &[(
                "ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL",
                fleet.role_url("zeroship_cdc"),
            )],
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            fleet.assert_alive();
            if compio::net::TcpStream::connect(&relay_addr).await.is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "CDC relay readiness; see {}",
                fleet.logs.display()
            );
            compio::time::sleep(Duration::from_millis(100)).await;
        }
        let control_port = url::Url::parse(&fleet.control_url)
            .unwrap()
            .port()
            .unwrap()
            .to_string();
        // The enrolment envelope bounds the ports a worker may advertise. With
        // a deferred second worker both listening ports must be inside it.
        let enrolment_ports = match fleet.second_worker_port {
            Some(second) => format!("{}-{}", worker_port.min(second), worker_port.max(second)),
            None => worker_port.to_string(),
        };
        let worker_port = worker_port.to_string();
        let gateway_port = url::Url::parse(&fleet.gateway_url)
            .unwrap()
            .port()
            .unwrap()
            .to_string();
        let blobs = fleet.blob_root.to_str().unwrap().to_owned();
        fleet.blobs = blobs.clone();
        // The manager owns no creator database: it reads the platform metadata
        // under its own login and verifies enrolled instances from the same
        // registry Control writes at enrolment.
        if let Some(manager_url) = fleet.manager_url.clone() {
            // The platform migration creates this login role without a
            // password, so a fixture that authenticates by password has to
            // supply one. Every other service role here carries a development
            // password from the migration itself.
            execute(
                &fleet.database.url(),
                "ALTER ROLE zeroship_workflow WITH PASSWORD 'zeroship_workflow'",
            )
            .await;
            let listen = format!(
                "127.0.0.1:{}",
                url::Url::parse(&manager_url).unwrap().port().unwrap()
            );
            let payloads = fleet.payload_store();
            release(url::Url::parse(&manager_url).unwrap().port().unwrap());
            fleet.spawn(
                "workflow",
                &binaries["zeroship-workflow-server"],
                &["--no-config".into(), "--listen".into(), listen],
                &[
                    (
                        "ZEROSHIP_WORKFLOW_DATABASE_URL",
                        fleet.role_url("zeroship_workflow"),
                    ),
                    ("ZEROSHIP_WORKFLOW_CONTROL_URL", fleet.control_url.clone()),
                    (
                        "ZEROSHIP_WORKFLOW_STORAGE_URL",
                        payloads.to_str().unwrap().to_owned(),
                    ),
                ],
            );
        }
        let mut control_env = vec![
            ("ZEROSHIP_CONTROL_DATABASE_URL", fleet.database.url()),
            ("ZEROSHIP_CONTROL_MASTER_KEY", MASTER_KEY.into()),
            ("ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING", "true".into()),
            (
                "ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS",
                "127.0.0.1/32".into(),
            ),
            ("ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS", enrolment_ports),
            (
                "ZEROSHIP_CONTROL_JOIN_SIGNERS_FILE",
                fleet.path("join-signers.json"),
            ),
            (
                "ZEROSHIP_CONTROL_JOIN_TOKEN_SIGNER_FILE",
                fleet.path("join-signer.json"),
            ),
            ("ZEROSHIP_CONTROL_JOIN_TOKEN_FILE", fleet.path("join-token")),
            ("ZEROSHIP_CONTROL_JOIN_TOKEN_ZONE", "default".into()),
        ];
        if let Some(manager_url) = fleet.manager_url.clone() {
            control_env.push((
                "ZEROSHIP_CONTROL_WORKFLOW_COORDINATOR_URL",
                manager_url,
            ));
        }
        release(control_port.parse().expect("control port"));
        fleet.spawn(
            "control",
            &binaries["zeroship-control"],
            &[
                "--no-config".into(),
                "--port".into(),
                control_port,
                "--blob-store".into(),
                blobs.clone(),
                "--worker-urls".into(),
                fleet.worker_url.clone(),
            ],
            &control_env,
        );
        fleet.ready(fleet.control_url.clone()).await;
        if let Some(manager_url) = fleet.manager_url.clone() {
            fleet.ready(manager_url).await;
        }
        fleet.spawn_worker("worker", &worker_port);
        fleet.ready(fleet.worker_url.clone()).await;
        release(gateway_port.parse().expect("gateway port"));
        fleet.spawn(
            "gateway",
            &binaries["zeroship-gate"],
            &[
                "--no-config".into(),
                "--port".into(),
                gateway_port,
                "--control-url".into(),
                fleet.control_url.clone(),
                // With a deferred second worker the gateway routes only to it,
                // so reaching the first worker means its own dispatch frame.
                "--worker-urls".into(),
                fleet
                    .second_worker_url
                    .clone()
                    .unwrap_or_else(|| fleet.worker_url.clone()),
                "--blob-store".into(),
                blobs.clone(),
                "--poll-interval".into(),
                "1".into(),
                "--broker-secret-file".into(),
                fleet.path("broker"),
            ],
            &[
                ("ZEROSHIP_GATEWAY_DATABASE_URL", fleet.database.url()),
                (
                    "ZEROSHIP_GATEWAY_SIGNING_KEY_FILE",
                    fleet.path("gateway.pem"),
                ),
                ("ZEROSHIP_GATEWAY_STASH_SIGNING_KEY", MASTER_KEY.into()),
            ],
        );
        fleet.ready(fleet.gateway_url.clone()).await;
        let bundle = fleet.work.path().join("workflow.zship");
        let schema_work = fleet.work.path().join("schema");
        let output = Command::new("node")
            .arg(
                workflow_postgres::root()
                    .join("crates/zeroship-control/tests/support/fixtures/build.mjs"),
            )
            .arg(workflow_postgres::root())
            .arg(&schema_work)
            .output()
            .expect("generate workflow effect schema");
        assert!(
            output.status.success(),
            "workflow schema generation: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        pack(&bundle);
        let owner = UserId::mint();
        let output = Command::new(&binaries["dev-provision"])
            .args([
                "--db",
                &fleet.database.url(),
                "--blob-store",
                &blobs,
                "--name",
                "workflow-fixture",
                "--defer-deploy",
                "--zship",
            ])
            .arg(&bundle)
            .arg("--owner")
            .arg(owner.as_str())
            .current_dir(fleet.work.path())
            .output()
            .expect("provision workflow app");
        assert!(
            output.status.success(),
            "workflow provisioning failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let app_id = stdout
            .lines()
            .find_map(|line| line.strip_prefix("app_id="))
            .expect("provision app id");
        fleet.app_id = AppId::parse(app_id).expect("canonical provisioned app id");
        let (pg, connection) =
            compio_postgres::connect(&fleet.database.url(), compio_postgres::NoTls)
                .await
                .unwrap();
        let driver = compio::runtime::spawn(connection.run());
        // THE CREATOR ZONE. Every schema statement below runs on the creator
        // database, which has no platform schema; `pg` above stays on the
        // platform one and is used only for platform catalog rows.
        let (mut creator_pg, creator_connection) =
            compio_postgres::connect(&fleet.database.creator_url(), compio_postgres::NoTls)
                .await
                .unwrap();
        let creator_driver = compio::runtime::spawn(creator_connection.run());
        // THE CREATOR DATABASE. Declared on the platform side exactly as
        // Control declares one, converged and granted on the creator side
        // exactly as the cluster reconciler converges and grants one, and bound
        // to this app - the apply below writes into ITS schema, and the worker
        // reaches it through ITS binding role.
        declare_creator_database(&pg, &fleet.app_id, &fleet.creator_database, &fleet.binding)
            .await;
        zeroship_migrate_server::datastore::cluster::apply_bootstrap_corpus(&creator_pg)
            .await
            .expect("bootstrap the creator cluster");
        zeroship_migrate_server::datastore::cluster::converge_database(
            &mut creator_pg,
            &fleet.creator_database,
        )
        .await
        .expect("converge the creator database");
        zeroship_migrate_server::datastore::cluster::grant_binding(
            &creator_pg,
            &fleet.binding,
            &fleet.creator_database,
            DatabaseCapability::ReadWrite,
        )
        .await
        .expect("grant the fleet app's binding");
        let request = serde_json::from_slice(
            &fs::read(schema_work.join("generated/apply-request.json")).unwrap(),
        )
        .unwrap();
        let policy = zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined(
            MASTER_KEY.as_bytes().to_vec(),
            1,
        )
        .unwrap();
        zeroship_migrate_server::apply::apply_ir_documents(
            &fleet.database.creator_url(),
            &schema_work,
            &fleet.creator_database,
            &request,
            &policy,
            &owner,
        )
        .await
        .expect("apply workflow effect schema through the migration service");
        let output = Command::new(&binaries["dev-provision"])
            .args([
                "--db",
                &fleet.database.url(),
                "--blob-store",
                &blobs,
                "--name",
                "workflow-fixture",
                "--zship",
            ])
            .arg(&bundle)
            .arg("--owner")
            .arg(owner.as_str())
            .current_dir(fleet.work.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "activate workflow deployment: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        pg.execute(
            "UPDATE zeroship.apps SET workflows_enabled = true WHERE id = $1",
            &[&fleet.app_id.as_str()],
        )
        .await
        .unwrap();
        // A plan carries the workflow policy the manager grants an app's host
        // under. Control's startup seeding leaves the column null, and a null
        // one refuses every policy lease, so the fixture publishes the catalog
        // default the way an operator does.
        pg.execute(
            "UPDATE zeroship.plans SET workflows_allowed = true, workflow_policy_json = $1",
            &[&serde_json::to_value(zeroship_core::workflow_policy::AppPolicy::default()).unwrap()],
        )
        .await
        .unwrap();
        pg.batch_execute("INSERT INTO workflow_manager.workflow_rollout_config (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by) VALUES ('global', false, false, 30000, 'workflow-fixture') ON CONFLICT (id) DO UPDATE SET dispatch_paused = false, ingress_disabled = false").await.unwrap();
        fleet.deploy_id = pg.query_one("SELECT id FROM zeroship.app_deploys WHERE app_id = $1 ORDER BY activated_at DESC, created_at DESC, id DESC LIMIT 1", &[&fleet.app_id.as_str()]).await.unwrap().get(0);
        drop(pg);
        drop(creator_pg);
        creator_driver.await.unwrap().unwrap();
        driver.await.unwrap().unwrap();
        fleet
    }

    fn path(&self, name: &str) -> String {
        self.work.path().join(name).to_str().unwrap().into()
    }

    /// The app name every fleet provisions, and so the ingress subdomain.
    pub const APP_NAME: &'static str = "workflow-fixture";

    fn secret(&self, name: &str, contents: &[u8]) {
        let path = self.work.path().join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// The payload object store BOTH the workflow service and the worker name.
    ///
    /// `workflow.storage_url` MUST name the same store as `worker.storage_url`
    /// (`crates/zeroship-workflow-server/src/config.rs`): the service stages a
    /// run input that an executing run reads back, so two directories fail the
    /// READ rather than the write, far from the cause. One source here is what
    /// keeps them from drifting.
    pub fn payload_store(&self) -> PathBuf {
        let payloads = self.work.path().join("objects");
        fs::create_dir_all(&payloads).unwrap();
        payloads
    }

    /// Spawn a worker process listening on `worker_port`.
    ///
    /// The same process shape serves the first and the deferred second worker:
    /// both read the SAME join-token file Control rotates, whose capped uses
    /// cover both, and both poll Control for the app's version and env. The
    /// port's reservation is held until this spawn, so the deferred second
    /// worker's port stays out of a sibling's reach until it starts.
    fn spawn_worker(&mut self, name: &str, worker_port: &str) {
        let mut worker_env = vec![
            (
                "ZEROSHIP_WORKER_DATABASE_URL",
                self.creator_role_url("zeroship_worker"),
            ),
            (
                "ZEROSHIP_WORKER_CDC_RELAY_URL",
                format!(
                    "wss://localhost:{}/internal/v1/cdc/subscribe",
                    self.relay_port
                ),
            ),
            (
                "ZEROSHIP_WORKER_CDC_RELAY_CA_FILE",
                self.path("relay-cert.pem"),
            ),
            ("ZEROSHIP_WORKER_JOIN_TOKEN_FILE", self.path("join-token")),
        ];
        if let Some(manager_url) = self.manager_url.clone() {
            // A workflow host stages payloads in the object store the workflow
            // service also names, and runs creator code against the app
            // database, so the worker needs both.
            let payloads = self.payload_store();
            worker_env.push(("ZEROSHIP_WORKER_WORKFLOW_MANAGER_URL", manager_url));
            worker_env.push(("ZEROSHIP_WORKER_WORKFLOW_PREPARED_APPS", "8".into()));
            worker_env.push(("ZEROSHIP_WORKER_WORKFLOW_SLOTS", "2".into()));
            worker_env.push((
                "ZEROSHIP_WORKER_STORAGE_URL",
                payloads.to_str().unwrap().to_owned(),
            ));
        }
        release(worker_port.parse().expect("worker port"));
        self.spawn_as(
            name,
            "worker",
            &binaries()["zeroship-worker"],
            &[
                "--port".into(),
                worker_port.to_owned(),
                "--threads".into(),
                "1".into(),
                "--control-url".into(),
                self.control_url.clone(),
                "--blob-store".into(),
                self.blobs.clone(),
                "--poll-interval".into(),
                "1".into(),
            ],
            &worker_env,
        );
    }

    /// Start the reserved second worker and wait for it to be ready.
    pub async fn start_second_worker(&mut self) {
        let port = self
            .second_worker_port
            .expect("the fleet reserved no deferred second worker")
            .to_string();
        self.spawn_worker("worker-2", &port);
        let url = self
            .second_worker_url
            .clone()
            .expect("the fleet reserved no deferred second worker");
        self.ready(url).await;
    }

    pub fn role_url(&self, role: &str) -> String {
        let mut url = url::Url::parse(&self.database.url()).unwrap();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        url.into()
    }

    /// The same login against the CREATOR zone. Only the worker takes one.
    pub fn creator_role_url(&self, role: &str) -> String {
        let mut url = url::Url::parse(&self.database.creator_url()).unwrap();
        url.set_username(role).unwrap();
        url.set_password(Some(role)).unwrap();
        url.into()
    }

    fn spawn(&mut self, name: &str, binary: &Path, args: &[String], env: &[(&str, String)]) {
        self.spawn_as(name, name, binary, args, env);
    }

    /// As [`Self::spawn`], naming the service whose environment variables this
    /// process reads when the process name is not the service name.
    ///
    /// A second worker is a distinct process (`worker-2`) serving the SAME
    /// `zeroship-worker` service, so it reads `ZEROSHIP_WORKER_*`. Deriving the
    /// prefix from the process name would ask it for `ZEROSHIP_WORKER-2_*`,
    /// which it never reads.
    fn spawn_as(
        &mut self,
        name: &str,
        service: &str,
        binary: &Path,
        args: &[String],
        env: &[(&str, String)],
    ) {
        let log = File::create(self.logs.join(format!("{name}.log"))).unwrap();
        let mut cmd = Command::new(binary);
        cmd.args(args)
            .current_dir(self.work.path())
            .env_clear()
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        for (key, value) in [
            (
                "PATH",
                zeroship_core::declared_env_os!(
                    external,
                    "PATH",
                    zeroship_core::config::TestHarness
                ),
            ),
            (
                "LD_LIBRARY_PATH",
                zeroship_core::declared_env_os!(
                    external,
                    "LD_LIBRARY_PATH",
                    zeroship_core::config::TestHarness
                ),
            ),
        ] {
            if let Some(value) = value {
                cmd.env(key, value);
            }
        }
        cmd.envs(env.iter().map(|(key, value)| (*key, value)))
            .env("ZEROSHIP_CONTROL_KEY", CONTROL_KEY)
            .env("ZEROSHIP_PAIRWISE_SALT", MASTER_KEY)
            .env("ZEROSHIP_ORIGIN_SCHEME", "http")
            .env("ZEROSHIP_AUTH_PLATFORM_ISSUER", &self.issuer_url)
            .env("ZEROSHIP_OBSERVABILITY_LOG_FORMAT", "json");
        if service != "relay" {
            // The worker holds no key of its own: it reads a join token, set
            // by its spawn, and draws its instance key in memory. Every other
            // service holds a role key of its own.
            if service != "worker" {
                cmd.env(
                    format!("ZEROSHIP_{}_SERVICE_KEY_FILE", service.to_uppercase()),
                    self.path(&format!("{service}.pem")),
                );
            }
            cmd.env(
                format!("ZEROSHIP_{}_SERVICE_PEERS_FILE", service.to_uppercase()),
                self.path("peers.json"),
            );
        }
        self.children
            .push((name.into(), cmd.spawn().expect("start workflow service")));
    }

    /// Stop one service the way an orchestrator does - SIGTERM, then wait for
    /// it to exit on its own - and return how it exited.
    ///
    /// `Drop` uses SIGKILL, which is the crash path; this is the graceful one,
    /// and a service that has not exited within the deadline is a failure
    /// rather than something to escalate past.
    pub fn terminate(&mut self, name: &str) -> std::process::ExitStatus {
        let index = self
            .children
            .iter()
            .position(|(child_name, _)| child_name == name)
            .unwrap_or_else(|| panic!("no fleet service named {name}"));
        let child = &mut self.children[index].1;
        let signalled = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("run kill -TERM");
        assert!(signalled.success(), "kill -TERM {name} failed: {signalled}");
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll the terminating service") {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "{name} did not exit after SIGTERM; see {}",
                self.logs.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        // A terminated service has left the fleet: `assert_alive`
        // and `Drop` must not read its clean exit as a crash.
        self.children.remove(index);
        status
    }

    pub fn assert_alive(&mut self) {
        for (name, child) in &mut self.children {
            assert!(
                child.try_wait().unwrap().is_none(),
                "workflow service {name} exited; see {}",
                self.logs.display()
            );
        }
    }

    async fn ready(&mut self, url: String) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            self.assert_alive();
            let ready = async {
                let response = cyper::Client::new()
                    .get(format!("{url}/readyz"))
                    .unwrap()
                    .send()
                    .await?;
                let ok = response.status().is_success();
                response.bytes().await?;
                Ok::<_, cyper::Error>(ok)
            };
            if matches!(
                compio::time::timeout(Duration::from_secs(2), ready).await,
                Ok(Ok(true))
            ) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{url} readiness failed; see {}",
                self.logs.display()
            );
            compio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        for (_, child) in self.children.iter_mut().rev() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn pack(path: &Path) {
    let source = include_bytes!("fixtures/keystone.js");
    let hash = zeroship_bundle::sha256_hex(source);
    let mut manifest: Value = serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    manifest["worker"]["modules"]["index.js"] = hash.clone().into();
    manifest["metadata"]["built_at"] = chrono::Utc::now().to_rfc3339().into();
    // No `runtime_descriptor`. Its entries are CREATOR databases: `executable.rs`
    // turns each into an `env.databases` member, and control's publication
    // command collects their ids into the set Deploy verifies a live binding
    // for. This app declares no database, so the field stays absent - the arm
    // `absent_descriptor_ingests_to_an_empty_set` covers. The workflow journal
    // schema reaches the service from `zeroship-workflow-schema`, not a bundle.
    let encoder = zstd::Encoder::new(File::create(path).unwrap(), 0).unwrap();
    let mut archive = tar::Builder::new(encoder);
    for (name, body) in [
        (
            "manifest.json".into(),
            serde_json::to_vec(&manifest).unwrap(),
        ),
        (format!("blobs/{hash}"), source.to_vec()),
    ] {
        let mut header = tar::Header::new_ustar();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive
            .append_data(&mut header, name, body.as_slice())
            .unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap();
}

/// Declare one creator database for this app and bind it, on the CONTROL
/// connection.
///
/// The rows are written already live because what this fleet exercises is the
/// worker and the workflow host, not convergence: Control declares a database
/// `provisioning` and a binding `pending`, and a per-cluster reconciler is what
/// moves either to `active`. The zone and the datastore come first because
/// `zeroship.databases` carries composite keys onto both.
async fn declare_creator_database(
    pg: &compio_postgres::Client,
    app: &AppId,
    database: &DatabaseId,
    binding: &BindingId,
) {
    let project: String = pg
        .query_one(
            "SELECT project_id FROM zeroship.apps WHERE id = $1::text",
            &[&app.as_str()],
        )
        .await
        .expect("the provisioned app row")
        .get("project_id");
    let zone: String = pg
        .query_one(
            "SELECT execution_zone_id FROM zeroship.projects WHERE id = $1::text",
            &[&project],
        )
        .await
        .expect("the app's project row")
        .get("execution_zone_id");
    let datastore = zeroship_core::typed_id::generate("dst");
    pg.execute(
        "INSERT INTO zeroship.datastores (id, system_identifier, execution_zone_id, status) \
         VALUES ($1, 5566778899001122, $2, 'active') \
         ON CONFLICT (system_identifier) DO NOTHING",
        &[&datastore, &zone],
    )
    .await
    .expect("register the fleet's creator cluster");
    let datastore: String = pg
        .query_one(
            "SELECT id FROM zeroship.datastores WHERE system_identifier = 5566778899001122",
            &[],
        )
        .await
        .expect("the registered datastore row")
        .get("id");
    pg.execute(
        "INSERT INTO zeroship.databases \
             (id, project_id, execution_zone_id, datastore_id, name, status) \
         VALUES ($1, $2, $3, $4, $5, 'active')",
        &[
            &database.as_str(),
            &project,
            &zone,
            &datastore,
            &format!("fleet-{}", database.as_str()),
        ],
    )
    .await
    .expect("declare the fleet's creator database");
    pg.execute(
        "INSERT INTO zeroship.database_bindings \
             (id, app_id, database_id, project_id, capability, status, observed_generation) \
         VALUES ($1, $2, $3, $4, $5, 'active', 1)",
        &[
            &binding.as_str(),
            &app.as_str(),
            &database.as_str(),
            &project,
            &DatabaseCapability::ReadWrite.as_wire(),
        ],
    )
    .await
    .expect("bind the fleet app to its creator database");
}
