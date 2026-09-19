//! Real relay process and least-privilege worker identity for V8 integration.

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
    relay_role: String,
    worker_role: String,
    instance: String,
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
    /// Start the relay and mint the two logins the exercise connects as.
    ///
    /// `binding` is the edge the caller provisioned the cluster for. The worker
    /// login is admitted to that binding's role and to nothing else, so a
    /// session that narrows with `SET LOCAL ROLE` reaches exactly the schema
    /// the ladder granted and the connection itself carries none of it.
    pub async fn start(admin: &Pool, admin_url: &str, binding: &DbBinding) -> Self {
        let suffix = zeroship_core::typed_id::generate("tst");
        let relay_role = format!("relay_{suffix}");
        let worker_role = format!("worker_{suffix}");
        let instance = zeroship_core::typed_id::generate("wkr");
        let key = ServiceSigningKey::generate();
        let binding_role = binding
            .session_role()
            .expect("the worker login assumes a binding that names a role");
        // `WITH INHERIT FALSE` alone, spelled the way
        // `zeroship_migrate_server::datastore::cluster::grant_binding` spells
        // the worker edge. A fixture that added `SET TRUE` would provision an
        // option the reconciler never emits.
        admin.batch_execute(&format!(
            "CREATE ROLE \"{relay_role}\" LOGIN REPLICATION NOSUPERUSER NOCREATEROLE NOCREATEDB NOINHERIT NOBYPASSRLS PASSWORD 'fixture';
             CREATE ROLE \"{worker_role}\" LOGIN NOREPLICATION NOSUPERUSER NOCREATEROLE NOCREATEDB NOINHERIT NOBYPASSRLS PASSWORD 'fixture';
             GRANT \"{binding_role}\" TO \"{worker_role}\" WITH INHERIT FALSE;
             CREATE SCHEMA IF NOT EXISTS zeroship;
             CREATE TABLE IF NOT EXISTS zeroship.worker_instances (
               id text PRIMARY KEY, ring_key bytea NOT NULL, public_key bytea NOT NULL,
               advertise_host inet NOT NULL, advertise_port int NOT NULL,
               registered_at timestamptz NOT NULL DEFAULT now(), status text NOT NULL
             );
             GRANT USAGE ON SCHEMA zeroship TO \"{relay_role}\";
             GRANT SELECT (id, status, public_key) ON zeroship.worker_instances TO \"{relay_role}\";"
        )).await.unwrap();
        admin.execute(
            "INSERT INTO zeroship.worker_instances (id, ring_key, public_key, advertise_host, advertise_port, status) VALUES ($1, $2, $2, '127.0.0.1'::inet, 8080, 'active')",
            &[&instance, &key.verifying_key_bytes().to_vec()],
        ).await.unwrap();
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
        let mut relay_url = url::Url::parse(admin_url).unwrap();
        relay_url.set_username(&relay_role).unwrap();
        relay_url.set_password(Some("fixture")).unwrap();
        let mut worker_url = relay_url.clone();
        worker_url.set_username(&worker_role).unwrap();
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
            .env("ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL", relay_url.as_str())
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
            relay_role,
            worker_role,
            instance,
            worker_url: worker_url.into(),
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

    pub async fn cleanup(mut self, admin: &Pool) {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
        admin
            .execute(
                "DELETE FROM zeroship.worker_instances WHERE id = $1",
                &[&self.instance],
            )
            .await
            .unwrap();
        admin
            .batch_execute(&format!(
                "DROP OWNED BY \"{}\", \"{}\"; DROP ROLE \"{}\", \"{}\"",
                self.relay_role, self.worker_role, self.relay_role, self.worker_role,
            ))
            .await
            .unwrap();
    }
}
