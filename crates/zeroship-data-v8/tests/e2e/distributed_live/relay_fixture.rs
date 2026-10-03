//! Real relay process and least-privilege worker identity for V8 integration.
//!
//! Both processes run under the logins the platform corpus creates: the relay
//! as `zeroship_cdc` and the worker as `zeroship_worker`, each with exactly the
//! reach `db/migrations-ts` grants it.

use super::platform::{self, Platform};
use compio_postgres::Pool;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroship_core::service_assertion::{ServiceIssuer, ServiceSigningKey, ServiceTrustBundle};
use zeroship_core::service_peers::{ServiceAuth, ServiceKeyring};
use zeroship_data_orm::binding::DbBinding;
use zeroship_data_orm::cdc::relay::RelayConfig;

pub struct RelayFixture {
    process: Child,
    _files: tempfile::TempDir,
    pub worker_url: String,
    pub config: RelayConfig,
}

impl Drop for RelayFixture {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

impl RelayFixture {
    /// Start the relay against the platform schema `platform` holds.
    ///
    /// `binding` is the edge the caller provisioned the cluster for. It is
    /// declared in Control's rows under its own ids, which is what the relay
    /// resolves a subscriber's schema from, and the worker login is admitted
    /// to that binding's role and to nothing else, so a session that narrows
    /// with `SET LOCAL ROLE` reaches exactly the schema the ladder granted and
    /// the connection itself carries none of it.
    pub async fn start(admin: &Pool, platform: &Platform, binding: &DbBinding) -> Self {
        let instance = zeroship_core::typed_id::generate("wkr");
        let key = ServiceSigningKey::generate();
        let binding_role = binding
            .session_role()
            .expect("the worker login assumes a binding that names a role");
        // `WITH INHERIT FALSE` alone, spelled the way
        // `zeroship_migrate_server::datastore::cluster::grant_binding` spells
        // the worker edge. A fixture that added `SET TRUE` would provision an
        // option the reconciler never emits.
        admin
            .batch_execute(&format!(
                "GRANT \"{binding_role}\" TO \"{}\" WITH INHERIT FALSE",
                platform::WORKER_LOGIN
            ))
            .await
            .unwrap();
        platform::enroll_worker(admin, &instance, &key.verifying_key_bytes()).await;
        let edge = binding
            .edge()
            .expect("the fixture binding addresses a database");
        platform::declare_edge(
            admin,
            binding.app_id(),
            edge.database(),
            edge.binding(),
            "active",
        )
        .await;
        let files = tempfile::tempdir().unwrap();
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = files.path().join("cert.pem");
        let private = files.path().join("key.pem");
        std::fs::write(&cert, certificate.cert.pem()).unwrap();
        std::fs::write(&private, certificate.signing_key.serialize_pem()).unwrap();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let relay_url = platform.relay_url();
        let binary = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("zeroship-data-cdc-server");
        assert!(binary.is_file(), "build the required relay with cargo build -p zeroship-data-cdc-server before running V8 integration tests");
        let log_path = files.path().join("relay.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let process = Command::new(binary)
            .args([
                "--no-config",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--tls-cert-file",
            ])
            .arg(&cert)
            .arg("--tls-key-file")
            .arg(&private)
            .env("ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL", &relay_url)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("start required relay");
        let keyring = ServiceKeyring::from_parts(
            ServiceIssuer::parse(&format!("spiffe://zeroship.ai/svc/worker/{instance}")).unwrap(),
            key,
            ServiceTrustBundle::new(),
        )
        .unwrap();
        let verifier = zeroship_core::service_assertion::ServiceAssertionVerifier::new(
            ServiceTrustBundle::new(),
            Arc::new(zeroship_core::service_assertion::InMemoryReplayStore::new()),
        );
        let auth = ServiceAuth::new(keyring, Arc::new(verifier));
        let config = RelayConfig::new(
            format!("wss://localhost:{port}/internal/v1/cdc/subscribe"),
            Arc::new(auth),
        )
        .unwrap()
        .with_ca_file(&cert)
        .unwrap();
        let mut fixture = Self {
            process,
            _files: files,
            worker_url: platform.worker_url(),
            config,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                fixture.process.try_wait().unwrap().is_none(),
                "relay exited: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            if compio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < deadline, "relay listener timed out");
            compio::time::sleep(Duration::from_millis(20)).await;
        }
        fixture
    }
}
