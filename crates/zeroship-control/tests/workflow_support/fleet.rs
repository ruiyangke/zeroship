//! Real workflow services and deploy artifact, owned by each acceptance test.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::workflow_postgres::{self, Database};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use serde_json::{json, Value};
use testcontainers::{core::WaitFor, runners::SyncRunner, Container, GenericImage, ImageExt};
use uuid::Uuid;
use zeroship_core::service_assertion::{
    ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
};
use zeroship_core::service_peers::{service_issuer, ServiceAuth, ServiceKeyring};
use zeroship_core::AppId;
use zeroship_core::UserId;

pub const CONTROL_KEY: &str = "test-control-key";
const MASTER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn key(service: &str) -> ed25519_dalek::SigningKey {
    let seed = match service {
        "control" => 31,
        "gateway" => 32,
        "worker" => 33,
        _ => panic!("unknown fixture service"),
    };
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

fn signing_key(service: &str) -> ServiceSigningKey {
    ServiceSigningKey::from_pkcs8_der(key(service).to_pkcs8_der().unwrap().as_bytes()).unwrap()
}

fn trust_bundle() -> ServiceTrustBundle {
    let mut peers = ServiceTrustBundle::new();
    for name in ["control", "gateway", "worker"] {
        let key = signing_key(name);
        peers
            .trust_signing_key(
                &service_issuer(&format!("svc/{name}")).unwrap(),
                key.key_id(),
                &key,
            )
            .unwrap();
    }
    peers
}

pub fn service_auth(service: &str) -> Arc<ServiceAuth> {
    Arc::new(ServiceAuth::new(
        ServiceKeyring::from_parts(
            service_issuer(&format!("svc/{service}")).unwrap(),
            signing_key(service),
            trust_bundle(),
        )
        .unwrap(),
        Arc::new(TransportAssertionVerifier::new(trust_bundle())),
    ))
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

pub fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
    pub app_id: AppId,
    pub deploy_id: String,
    pub blob_root: PathBuf,
}

impl Fleet {
    pub fn start() -> Self {
        Self::with_advance(true)
    }

    pub fn with_advance(advance: bool) -> Self {
        std::thread::spawn(move || {
            compio::runtime::Runtime::new()
                .expect("fleet runtime")
                .block_on(Self::launch(advance))
        })
        .join()
        .unwrap_or_else(|error| std::panic::resume_unwind(error))
    }

    async fn launch(advance: bool) -> Self {
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
            app_id: AppId::mint(),
            deploy_id: String::new(),
        };
        fs::create_dir_all(&fleet.blob_root).unwrap();
        let mut peer_keys = vec![];
        for name in ["control", "gateway", "worker"] {
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
        fleet.spawn(
            "control",
            &binaries["zeroship-control"],
            &[
                "--no-config".into(),
                "--port".into(),
                control_port,
                "--blob-store".into(),
                blobs.clone(),
                "--gateway-url".into(),
                fleet.gateway_url.clone(),
                "--worker-urls".into(),
                fleet.worker_url.clone(),
                "--disable-workflow-engine".into(),
            ],
            &[
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
            ],
        );
        fleet.ready(fleet.control_url.clone()).await;
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
                "--max-step-blob-bytes".into(),
                "2097152".into(),
            ]
            .into_iter()
            .chain(advance.then(|| "--workflow-advance-unsigned".to_string()))
            .collect::<Vec<_>>(),
            &[
                (
                    "ZEROSHIP_WORKER_DATABASE_URL",
                    fleet.role_url("zeroship_worker"),
                ),
                (
                    "ZEROSHIP_WORKER_CDC_RELAY_URL",
                    format!("wss://localhost:{relay_port}/internal/v1/cdc/subscribe"),
                ),
                (
                    "ZEROSHIP_WORKER_CDC_RELAY_CA_FILE",
                    fleet.path("relay-cert.pem"),
                ),
            ],
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
        zeroship_migrate_server::provisioning::provision_database(&pg, fleet.app_id.as_str())
            .await
            .unwrap();
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
        zeroship_migrate_server::apply::apply_ir_documents(
            &fleet.database.url(),
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
        pg.batch_execute("UPDATE zeroship.plans SET workflows_allowed = true; INSERT INTO zeroship.workflow_rollout_config (id, dispatch_paused, ingress_disabled, source_validity_ms, updated_by) VALUES ('global', false, false, 30000, 'workflow-fixture') ON CONFLICT (id) DO UPDATE SET dispatch_paused = false, ingress_disabled = false").await.unwrap();
        fleet.deploy_id = pg.query_one("SELECT id FROM zeroship.app_deploys WHERE app_id = $1 ORDER BY activated_at DESC, created_at DESC, id DESC LIMIT 1", &[&fleet.app_id.as_str()]).await.unwrap().get(0);
        drop(pg);
        driver.await.unwrap().unwrap();
        fleet
    }

    fn path(&self, name: &str) -> String {
        self.work.path().join(name).to_str().unwrap().into()
    }

    fn secret(&self, name: &str, contents: &[u8]) {
        let path = self.work.path().join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn role_url(&self, role: &str) -> String {
        let mut url = url::Url::parse(&self.database.url()).unwrap();
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
            cmd.env(
                format!("ZEROSHIP_{}_SERVICE_KEY_FILE", name.to_uppercase()),
                self.path(&format!("{name}.pem")),
            )
            .env(
                format!("ZEROSHIP_{}_SERVICE_PEERS_FILE", name.to_uppercase()),
                self.path("peers.json"),
            );
        }
        self.children
            .push((name.into(), cmd.spawn().expect("start workflow service")));
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
