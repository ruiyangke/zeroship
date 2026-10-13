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
        let (process, port) = spawn_listening(&binary, &cert, &private, &relay_url, &log_path).await;
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
        Self {
            process,
            _files: files,
            worker_url: platform.worker_url(),
            config,
        }
    }
}

/// How a spawned relay's readiness wait ended.
enum Started {
    Ready,
    /// The relay exited because another process already owns the reserved
    /// port, so the caller reserves a different one.
    AddressInUse,
}

/// Start the relay on a port this process reserves, and return it once THIS
/// relay is listening there.
///
/// The reserved port is released before the relay binds it, so another case
/// can take it first. A connection that merely succeeds on the port proves
/// nothing then: it may reach that other process while this relay has exited
/// on the collision, and the first subscription later fails as "CDC relay
/// connection failed". Readiness is accepted only from the relay's own pid,
/// and a collision reserves a fresh port.
async fn spawn_listening(
    binary: &std::path::Path,
    cert: &std::path::Path,
    private: &std::path::Path,
    relay_url: &str,
    log_path: &std::path::Path,
) -> (Child, u16) {
    for _ in 0..16 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let log = std::fs::File::create(log_path).unwrap();
        let mut process = Command::new(binary)
            .args([
                "--no-config",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--tls-cert-file",
            ])
            .arg(cert)
            .arg("--tls-key-file")
            .arg(private)
            .env("ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL", relay_url)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("start required relay");
        match wait_listening(&mut process, port, log_path).await {
            Started::Ready => return (process, port),
            Started::AddressInUse => {
                let _ = process.kill();
                let _ = process.wait();
            }
        }
    }
    panic!("could not reserve a listening port for the relay after repeated collisions");
}

/// Wait until `process` itself listens on `port`, or report why it stopped.
///
/// # Panics
/// When the relay exits for any reason other than an address collision, or
/// does not listen within the wait.
async fn wait_listening(process: &mut Child, port: u16, log_path: &std::path::Path) -> Started {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if process.try_wait().unwrap().is_some() {
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            if log.contains("Address already in use") {
                return Started::AddressInUse;
            }
            panic!("relay exited: {log}");
        }
        if zeroship_testkit::listen::child_listens(process.id(), port) {
            // The relay that owns the socket can still have exited since,
            // freeing the port for another case.
            if process.try_wait().unwrap().is_none() {
                return Started::Ready;
            }
            continue;
        }
        assert!(Instant::now() < deadline, "relay listener timed out");
        compio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A port another process holds is not this relay listening: a connection to
/// it succeeds, but the readiness wait keeps waiting for its own child.
#[compio::test]
async fn readiness_is_not_taken_from_a_port_another_process_holds() {
    let foreign = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = foreign.local_addr().unwrap().port();
    let mut child = Command::new("sleep").arg("30").spawn().expect("spawn a child that never listens");
    let log = tempfile::NamedTempFile::new().unwrap();
    assert!(
        compio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok(),
        "the foreign listener accepts, so a bare connect would call the relay ready"
    );
    let waited = compio::time::timeout(
        Duration::from_millis(300),
        wait_listening(&mut child, port, log.path()),
    )
    .await;
    let _ = child.kill();
    let _ = child.wait();
    assert!(
        waited.is_err(),
        "readiness was accepted from a port this child does not hold"
    );
}
