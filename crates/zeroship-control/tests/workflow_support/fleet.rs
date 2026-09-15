//! Real workflow services and deploy artifact, owned by each acceptance test.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::workflow_postgres::{self, Database};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use serde_json::{json, Value};
use testcontainers::{core::WaitFor, runners::SyncRunner, Container, GenericImage, ImageExt};
use uuid::Uuid;
use zeroship_core::service_assertion::ServiceSigningKey;
use zeroship_core::service_peers::service_issuer;
use zeroship_core::AppId;
use zeroship_core::UserId;

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

fn binaries() -> &'static BTreeMap<String, PathBuf> {
    static BINARIES: OnceLock<BTreeMap<String, PathBuf>> = OnceLock::new();
    BINARIES.get_or_init(|| {
        let output = Command::new("cargo")
            .args([
                "build",
                "--locked",
                "--message-format=json",
                "--bins",
                "-p",
                "zeroship-control",
                "-p",
                "zeroship-worker",
                "-p",
                "zeroship-gateway",
                "-p",
                "zeroship-data-cdc-server",
                "-p",
                "zeroship-workflow-server",
            ])
            .current_dir(workflow_postgres::root())
            .output()
            .expect("build workflow services");
        if !output.status.success() {
            let logs = workflow_postgres::root().join("target/workflow-tests");
            fs::create_dir_all(&logs).unwrap();
            let path = logs.join(format!("build-{}.log", Uuid::new_v4()));
            let mut contents = output.stderr.clone();
            contents.extend_from_slice(&output.stdout);
            fs::write(&path, contents).unwrap();
            panic!("workflow service build failed; see {}", path.display());
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let value: Value = serde_json::from_str(line).ok()?;
                Some((
                    value["target"]["name"].as_str()?.to_owned(),
                    PathBuf::from(value["executable"].as_str()?),
                ))
            })
            .collect()
    })
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

pub fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What this fleet runs beyond the always-present services.
#[derive(Clone, Copy, Debug, Default)]
pub struct FleetOptions {
    /// Run a workflow manager, point Control's lifecycle publisher at it, and
    /// give the worker a workflow host registered with it.
    pub workflow_manager: bool,
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
    /// Present only when [`FleetOptions::workflow_manager`] asked for one.
    pub manager_url: Option<String>,
    pub app_id: AppId,
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
            worker_url: format!("http://127.0.0.1:{}", port()),
            manager_url: options
                .workflow_manager
                .then(|| format!("http://127.0.0.1:{}", port())),
            app_id: AppId::mint(),
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
        let relay_addr = format!("127.0.0.1:{relay_port}");
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
        let worker_port = url::Url::parse(&fleet.worker_url)
            .unwrap()
            .port()
            .unwrap()
            .to_string();
        let gateway_port = url::Url::parse(&fleet.gateway_url)
            .unwrap()
            .port()
            .unwrap()
            .to_string();
        let blobs = fleet.blob_root.to_str().unwrap().to_owned();
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
            (
                "ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS",
                worker_port.clone(),
            ),
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
        let mut worker_env = vec![
            (
                "ZEROSHIP_WORKER_DATABASE_URL",
                fleet.creator_role_url("zeroship_worker"),
            ),
            (
                "ZEROSHIP_WORKER_CDC_RELAY_URL",
                format!("wss://localhost:{relay_port}/internal/v1/cdc/subscribe"),
            ),
            (
                "ZEROSHIP_WORKER_CDC_RELAY_CA_FILE",
                fleet.path("relay-cert.pem"),
            ),
            // Control mints this before it binds, and `fleet.ready` above
            // waited for that bind, so the file is there by the time the
            // worker reads it.
            ("ZEROSHIP_WORKER_JOIN_TOKEN_FILE", fleet.path("join-token")),
        ];
        if let Some(manager_url) = fleet.manager_url.clone() {
            // A workflow host stages payloads in the app object store and keeps
            // creator journals in the app database, so the worker needs both.
            let payloads = fleet.work.path().join("objects");
            fs::create_dir_all(&payloads).unwrap();
            worker_env.push(("ZEROSHIP_WORKER_WORKFLOW_MANAGER_URL", manager_url));
            worker_env.push(("ZEROSHIP_WORKER_WORKFLOW_CAPACITY", "8".into()));
            worker_env.push(("ZEROSHIP_WORKER_WORKFLOW_SLOTS", "2".into()));
            worker_env.push((
                "ZEROSHIP_WORKER_STORAGE_URL",
                payloads.to_str().unwrap().to_owned(),
            ));
        }
        fleet.spawn(
            "worker",
            &binaries["zeroship-worker"],
            &[
                "--port".into(),
                worker_port,
                "--threads".into(),
                "1".into(),
                "--control-url".into(),
                fleet.control_url.clone(),
                "--blob-store".into(),
                blobs.clone(),
                "--poll-interval".into(),
                "1".into(),
            ],
            &worker_env,
        );
        fleet.ready(fleet.worker_url.clone()).await;
        fleet.spawn(
            "gateway",
            &binaries["zeroship-gate"],
            &[
                "--no-config".into(),
                "--port".into(),
                gateway_port,
                "--control-url".into(),
                fleet.control_url.clone(),
                "--worker-urls".into(),
                fleet.worker_url.clone(),
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
                    .join("crates/zeroship-control/tests/workflow_support/fixtures/build.mjs"),
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
        let descriptor = fs::read(schema_work.join("generated/schema.runtime.json")).unwrap();
        pack(&bundle, &descriptor);
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
        // database, which has no platform schema;  above stays on the
        // platform one and is used only for platform catalog rows.
        let (creator_pg, creator_connection) =
            compio_postgres::connect(&fleet.database.creator_url(), compio_postgres::NoTls)
                .await
                .unwrap();
        let creator_driver = compio::runtime::spawn(creator_connection.run());
        zeroship_migrate_server::provisioning::provision_database(&creator_pg, fleet.app_id.as_str())
            .await
            .unwrap();
        if fleet.manager_url.is_some() {
            // The creator journal is migration-path DDL: the worker's host
            // never creates it. Installing it before the apply below leaves it
            // inside the apply's schema-wide runtime-role grant, so the worker
            // reaches it exactly as it reaches the creator's own tables.
            let schema = zeroship_workflow::service::store::SchemaName::new(fleet.app_id.as_str())
                .expect("the app's schema name");
            creator_pg.batch_execute(&zeroship_workflow::service::schema::postgres_sql(&schema))
                .await
                .expect("install the creator workflow journal");
        }
        let request = serde_json::from_slice(
            &fs::read(schema_work.join("generated/migrations.ir.json")).unwrap(),
        )
        .unwrap();
        let policy = zeroship_migrate_server::policy::ManagedPolicyConfig::default_confined(
            MASTER_KEY.as_bytes().to_vec(),
            1,
        )
        .unwrap();
        let ledger = zeroship_migrate_server::schema_apply_store::SchemaApplyStore::new(
            fleet.database.url(),
        );
        // The DDL lands in the creator database; the apply LEDGER stays a
        // platform table, which is the same split the migration service runs
        // under in production.
        zeroship_migrate_server::apply::apply_ir_documents(
            &fleet.database.creator_url(),
            &schema_work,
            &fleet.app_id,
            &request,
            &policy,
            &ledger,
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
        pg.batch_execute("INSERT INTO zeroship.workflow_rollout_config (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by) VALUES ('global', false, false, 30000, 'workflow-fixture') ON CONFLICT (id) DO UPDATE SET dispatch_paused = false, ingress_disabled = false").await.unwrap();
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
        if name != "relay" {
            // The worker holds no key of its own: it reads a join token, set
            // by its spawn, and draws its instance key in memory. Every other
            // service holds a role key of its own.
            if name != "worker" {
                cmd.env(
                    format!("ZEROSHIP_{}_SERVICE_KEY_FILE", name.to_uppercase()),
                    self.path(&format!("{name}.pem")),
                );
            }
            cmd.env(
                format!("ZEROSHIP_{}_SERVICE_PEERS_FILE", name.to_uppercase()),
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
        let (_, child) = self
            .children
            .iter_mut()
            .find(|(child_name, _)| child_name == name)
            .unwrap_or_else(|| panic!("no fleet service named {name}"));
        let signalled = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .expect("run kill -TERM");
        assert!(signalled.success(), "kill -TERM {name} failed: {signalled}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().expect("poll the terminating service") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{name} did not exit after SIGTERM; see {}",
                self.logs.display()
            );
            std::thread::sleep(Duration::from_millis(100));
        }
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

fn pack(path: &Path, descriptor: &[u8]) {
    let source = include_bytes!("fixtures/keystone.js");
    let hash = zeroship_bundle::sha256_hex(source);
    let mut manifest: Value = serde_json::from_str(include_str!("fixtures/manifest.json")).unwrap();
    manifest["worker"]["modules"]["index.js"] = hash.clone().into();
    manifest["metadata"]["built_at"] = chrono::Utc::now().to_rfc3339().into();
    let descriptor_hash = zeroship_bundle::sha256_hex(descriptor);
    manifest["runtime_descriptor"] = json!({"hash": descriptor_hash});
    let encoder = zstd::Encoder::new(File::create(path).unwrap(), 0).unwrap();
    let mut archive = tar::Builder::new(encoder);
    for (name, body) in [
        (
            "manifest.json".into(),
            serde_json::to_vec(&manifest).unwrap(),
        ),
        (format!("blobs/{hash}"), source.to_vec()),
        (format!("blobs/{descriptor_hash}"), descriptor.to_vec()),
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
