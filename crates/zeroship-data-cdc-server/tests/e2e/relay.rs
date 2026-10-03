//! Separate-process relay test. PostgreSQL is mandatory.

use zeroship_data_testkit::data::platform as platform_fixture;
use zeroship_testkit::postgres::server as postgres_fixture;

/// The plain identity a creator binding carries, for the shared ladder.
fn plain_binding(
    binding: &zeroship_data_orm::binding::DbBinding,
) -> zeroship_data_testkit::data::HarnessBinding {
    let edge = binding
        .edge()
        .expect("a creator binding addresses a database");
    zeroship_data_testkit::data::HarnessBinding::new(
        binding.app_id(),
        binding.deploy_token(),
        edge.database().clone(),
        edge.binding().clone(),
        edge.database_capability(),
    )
}

/// The shared role ladder, adapted to this binary's ORM binding type.
mod roles {
    pub(super) use zeroship_data_testkit::data::roles::BindingLadderOutcome;

    pub(super) async fn ensure_binding_ladder(
        pool: &compio_postgres::Pool,
        binding: &zeroship_data_orm::binding::DbBinding,
    ) -> Result<BindingLadderOutcome, compio_postgres::Error> {
        zeroship_data_testkit::data::roles::ensure_binding_ladder(
            pool,
            &super::plain_binding(binding),
        )
        .await
    }
}

use compio_postgres::Pool;
use compio_tls::TlsConnector;
use compio_ws::{tungstenite::Message, WebSocketStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zeroship_core::service_assertion::{ServiceAssertionMinter, ServiceIssuer, ServiceSigningKey};
use zeroship_core::service_peers::service_issuer;
use zeroship_data_cdc_wire::{Event, Operation, Subscribe, PATH};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the relay binary under `url`'s login, listening on `port` and
/// logging into `log`.
fn start_relay(
    url: &str,
    port: u16,
    cert: &Path,
    key: &Path,
    log: &Path,
    extra: &[&str],
) -> Process {
    let log = std::fs::File::create(log).unwrap();
    Process(
        Command::new(env!("CARGO_BIN_EXE_zeroship-data-cdc-server"))
            .args(["--no-config", "--listen", &format!("127.0.0.1:{port}")])
            .args(extra)
            .arg("--tls-cert-file")
            .arg(cert)
            .arg("--tls-key-file")
            .arg(key)
            .env("ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL", url)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    )
}

/// Wait until the relay accepts a connection on `port`, failing if it exits.
async fn until_listening(process: &mut Process, port: u16, log: &Path) {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "relay exited: {}",
            std::fs::read_to_string(log).unwrap()
        );
        if compio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        assert!(Instant::now() < until, "relay listener timed out");
        compio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn receive<S: compio::io::AsyncRead + compio::io::AsyncWrite>(
    socket: &mut WebSocketStream<S>,
) -> Result<Event, String> {
    loop {
        let message = compio::time::timeout(Duration::from_secs(15), socket.read())
            .await
            .map_err(|_| "timed out")?
            .map_err(|e| e.to_string())?;
        let Message::Binary(bytes) = message else {
            return Err("nonbinary message".into());
        };
        let event = Event::decode(&bytes).map_err(|_| "invalid event")?;
        if event != Event::Heartbeat {
            return Ok(event);
        }
    }
}

async fn native_event(
    subscription: &zeroship_data_orm::cdc::Subscription,
) -> zeroship_data_orm::cdc::SubscriptionMessage {
    compio::time::timeout(
        Duration::from_secs(15),
        futures::future::poll_fn(|cx| {
            subscription.register_waker(cx.waker().clone());
            subscription
                .pop()
                .map_or(std::task::Poll::Pending, std::task::Poll::Ready)
        }),
    )
    .await
    .expect("ORM event timed out")
}

fn assertion(instance: &str, key: &ServiceSigningKey) -> String {
    let issuer =
        ServiceIssuer::parse(&format!("spiffe://zeroship.ai/svc/worker/{instance}")).unwrap();
    let minter = ServiceAssertionMinter::new(issuer, key.key_id(), key).unwrap();
    format!(
        "Bearer {}",
        minter.mint(&service_issuer("svc/cdc").unwrap()).unwrap()
    )
}

#[compio::test]
async fn relay_process_authenticates_workers_and_streams_commits_without_worker_replication() {
    // The platform schema the corpus builds, and the two logins it creates:
    // the relay runs as `zeroship_cdc` and the worker as `zeroship_worker`,
    // each with exactly the reach `db/migrations-ts` grants it.
    let postgres = postgres_fixture::Postgres::start();
    let platform = platform_fixture::Platform::apply(postgres.url());
    let db = Pool::connect(&platform.admin_url(), 2)
        .await
        .expect("required PostgreSQL");
    let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
    let publication = zeroship_core::replication_names::DATASTORE_PUBLICATION;
    // The app's ONE database, declared in Control's rows. The relay reads the
    // ids back out of those rows to learn which schema this subscriber is
    // entitled to, so nothing here derives a schema from the app id.
    let database_id = zeroship_core::DatabaseId::mint();
    let edge = platform_fixture::declare_binding(&db, &app, &database_id, "active").await;
    let binding = zeroship_data_orm::binding::DbBinding::to_database(
        app.as_str(),
        zeroship_data_orm::binding::COLD_START_DEPLOY_TOKEN,
        database_id,
        edge,
        zeroship_core::database_role::DatabaseCapability::ReadWrite,
    )
    .unwrap();
    let schema = binding.schema().as_str().to_owned();
    let database = binding.database().unwrap().as_str().to_owned();
    let binding_role = binding.session_role().unwrap().to_owned();
    // The capture this subscription starts, named for the pair it captures.
    let slot = zeroship_core::replication_names::relay_slot_name(&app, &database).unwrap();
    // The runtime's reach comes from the binding ladder, never from a grant to
    // the login. A direct grant to the worker login is the thing the whole
    // fence exists to prevent, and it would survive every revoke.
    roles::ensure_binding_ladder(&db, &binding).await.unwrap();
    db.batch_execute(&format!("CREATE TABLE \"{schema}\".orders (id int PRIMARY KEY, secret text); GRANT SELECT, INSERT, UPDATE, DELETE ON \"{schema}\".orders TO \"{}\"; GRANT \"{binding_role}\" TO \"{}\" WITH INHERIT FALSE; CREATE PUBLICATION \"{publication}\" FOR TABLE \"{schema}\".orders",
        zeroship_core::database_derivation::capability_role_name(
            binding.database().unwrap(),
            zeroship_core::database_role::DatabaseCapability::ReadWrite,
        )
        .unwrap(),
        platform_fixture::WORKER_LOGIN,
    )).await.unwrap();
    // Two enrolled worker instances, each admitted under a join signer of its
    // own through the function Control's join handler admits with. The relay
    // reads only `worker_instances`, so a SIGNER-level purge reaches it through
    // the instance status the purge sets, never through a join of its own.
    let first_id = zeroship_core::typed_id::generate("wkr");
    let second_id = zeroship_core::typed_id::generate("wkr");
    let first_key = ServiceSigningKey::generate();
    let second_key = ServiceSigningKey::generate();
    // `first_id` joins under E, `second_id` under F - the later arm purges E
    // alone and requires only `first_id` to be affected.
    let signer_e =
        platform_fixture::enroll_worker(&db, &first_id, &first_key.verifying_key_bytes()).await;
    let signer_f =
        platform_fixture::enroll_worker(&db, &second_id, &second_key.verifying_key_bytes()).await;
    assert_ne!(signer_e, signer_f, "each instance has a signer of its own");
    let temp = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = temp.path().join("cert.pem");
    let key_path = temp.path().join("key.pem");
    std::fs::write(&cert_path, certificate.cert.pem()).unwrap();
    std::fs::write(&key_path, certificate.signing_key.serialize_pem()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    ));
    let port = free_port();
    let log = temp.path().join("relay.log");
    let mut process = start_relay(
        &platform.relay_url(),
        port,
        &cert_path,
        &key_path,
        &log,
        &["--transaction-changes", "2"],
    );
    until_listening(&mut process, port, &log).await;
    let endpoint = format!("wss://localhost:{port}{PATH}");
    let connect = || {
        compio_ws::connect_async_tls_with_config(endpoint.as_str(), None, Some(connector.clone()))
    };
    let (mut first, _) = connect().await.unwrap();
    let token = assertion(&first_id, &first_key);
    first
        .send(Message::Binary(
            Subscribe {
                app_id: app.clone(),
                database_id: database.clone(),
                authorization: token.clone(),
            }
            .encode()
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut first).await.unwrap(), Event::Ready);
    let (mut replay, _) = connect().await.unwrap();
    replay
        .send(Message::Binary(
            Subscribe {
                app_id: app.clone(),
                database_id: database.clone(),
                authorization: token,
            }
            .encode()
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert!(
        receive(&mut replay).await.is_err(),
        "replayed assertion accepted"
    );
    let (mut forged, _) = connect().await.unwrap();
    forged
        .send(Message::Binary(
            Subscribe {
                app_id: app.clone(),
                database_id: database.clone(),
                authorization: assertion(&first_id, &second_key),
            }
            .encode()
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert!(
        receive(&mut forged).await.is_err(),
        "unregistered key accepted"
    );
    let (mut second, _) = connect().await.unwrap();
    second
        .send(Message::Binary(
            Subscribe {
                app_id: app.clone(),
                database_id: database.clone(),
                authorization: assertion(&second_id, &second_key),
            }
            .encode()
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut second).await.unwrap(), Event::Ready);
    let identity =
        ServiceIssuer::parse(&format!("spiffe://zeroship.ai/svc/worker/{second_id}")).unwrap();
    let keyring = zeroship_core::service_peers::ServiceKeyring::from_parts(
        identity,
        second_key,
        zeroship_core::service_assertion::ServiceTrustBundle::new(),
    )
    .unwrap();
    let verifier = zeroship_core::service_assertion::ServiceAssertionVerifier::new(
        zeroship_core::service_assertion::ServiceTrustBundle::new(),
        Arc::new(zeroship_core::service_assertion::InMemoryReplayStore::new()),
    );
    let auth = Arc::new(zeroship_core::service_peers::ServiceAuth::new(
        keyring,
        Arc::new(verifier),
    ));
    let client = zeroship_data_orm::cdc::relay::RelayConfig::new(endpoint.clone(), auth)
        .unwrap()
        .with_tls_connector(connector.clone());
    let route = binding.route();
    let native = zeroship_data_orm::cdc::broker::subscribe(&route, "orders");
    let native_handle = client.spawn(&route).await.unwrap();
    let slots: i64 = db
        .query(
            "SELECT count(*) FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(slots, 1, "workers must share capture");
    let worker = Pool::connect(&platform.worker_url(), 1).await.unwrap();
    // The login carries nothing of its own: the grant is `WITH INHERIT FALSE`,
    // so it reaches the table only by assuming the binding role, exactly as the
    // data path does with `SET LOCAL ROLE`.
    worker
        .batch_execute(&format!("SET ROLE \"{binding_role}\""))
        .await
        .unwrap();
    assert!(worker
        .query(
            "SELECT * FROM pg_create_logical_replication_slot('forbidden_worker', 'pgoutput')",
            &[]
        )
        .await
        .is_err());
    worker.batch_execute(&format!("BEGIN; INSERT INTO \"{schema}\".orders VALUES (1, 'private'); ROLLBACK; INSERT INTO \"{schema}\".orders VALUES (2, 'private')")).await.unwrap();
    let expected = Event::Change {
        collection: "orders".into(),
        operation: Operation::Insert,
    };
    assert_eq!(receive(&mut first).await.unwrap(), expected);
    assert_eq!(receive(&mut second).await.unwrap(), expected);
    let zeroship_data_orm::cdc::SubscriptionMessage::Change(change) = native_event(&native).await
    else {
        panic!("ORM change expected");
    };
    assert_eq!(change.collection, "orders");
    assert!(change.pk.is_none() && change.new_tuple.is_empty() && change.old_tuple.is_none());

    worker
        .batch_execute(&format!(
            "INSERT INTO \"{schema}\".orders VALUES (3, 'private'), (4, 'private'), (5, 'private')"
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut first).await.unwrap(), Event::Resync);
    assert_eq!(receive(&mut second).await.unwrap(), Event::Resync);
    assert!(matches!(
        native_event(&native).await,
        zeroship_data_orm::cdc::SubscriptionMessage::Resync
    ));

    // Purge signer E through the operator's own function
    // (db/migrations-ts/20260914000500_worker_join_bindings.ts), which marks the
    // signer revoked and every active instance it admitted gone in one
    // transaction. Nothing here writes `first_id` by name - the cascade is
    // scoped entirely through `join_signer_id`, so a SIGNER-level operation,
    // not a per-instance one, is what reaches this reader.
    db.execute("SELECT zeroship.purge_worker_join_signer($1)", &[&signer_e])
        .await
        .unwrap();
    assert!(
        receive(&mut first).await.is_err(),
        "revoked worker kept its stream"
    );
    worker
        .batch_execute(&format!(
            "INSERT INTO \"{schema}\".orders VALUES (6, 'private')"
        ))
        .await
        .unwrap();
    // THE PAIRED CONTROL: `second_id` joined under F, which the purge above
    // never touched, is unaffected by E's revocation.
    assert_eq!(receive(&mut second).await.unwrap(), expected);
    native_handle.shutdown().await.unwrap();
    native.close();
    drop(first);
    drop(second);
    drop(forged);
    drop(replay);
    let cleanup_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let slots = db
            .query(
                "SELECT 1 FROM pg_replication_slots WHERE slot_name = $1",
                &[&slot],
            )
            .await
            .unwrap();
        if slots.is_empty() {
            break;
        }
        assert!(
            Instant::now() < cleanup_deadline,
            "relay did not release its final subscription slot"
        );
        compio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(process);
    worker.close().await;
    db.close().await;
}

/// **A relay whose login cannot run the binding lookup refuses to boot.**
///
/// The relay proves at startup that its login can read both of Control's
/// projections admission reads. Without the live-binding grant it must exit
/// before it ever listens and name the table the server refused, rather than
/// listen and refuse every subscriber - which is what a relay newer than the
/// applied platform migrations did.
///
/// The control differs in the one variable: the same binary, login and
/// database, with the grant the migrations made restored, listens.
#[compio::test]
async fn relay_refuses_to_boot_without_the_live_binding_grant() {
    use platform_fixture::{grant_relay, relay_columns, revoke_relay};
    let postgres = postgres_fixture::Postgres::start();
    let platform = platform_fixture::Platform::apply(postgres.url());
    let db = Pool::connect(&platform.admin_url(), 2)
        .await
        .expect("required PostgreSQL");
    let mut granted = Vec::new();
    for table in ["database_bindings", "databases"] {
        let columns = relay_columns(&db, table).await;
        assert!(
            !columns.is_empty(),
            "the relay's login must hold a grant on zeroship.{table} to lose one"
        );
        revoke_relay(&db, table).await;
        granted.push((table, columns));
    }
    let temp = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = temp.path().join("cert.pem");
    let key = temp.path().join("key.pem");
    std::fs::write(&cert, certificate.cert.pem()).unwrap();
    std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();

    let port = free_port();
    let log = temp.path().join("refused.log");
    let mut refused = start_relay(&platform.relay_url(), port, &cert, &key, &log, &[]);
    let until = Instant::now() + Duration::from_mins(1);
    let status = loop {
        if let Some(status) = refused.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            compio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err(),
            "a relay that cannot run the binding lookup must not listen"
        );
        assert!(
            Instant::now() < until,
            "the relay neither exited nor listened"
        );
        compio::time::sleep(Duration::from_millis(20)).await;
    };
    let output = std::fs::read_to_string(&log).unwrap();
    assert!(
        !status.success(),
        "a refused boot exits with failure: {status}\n{output}"
    );
    assert!(
        output.contains("permission denied for table database_bindings"),
        "the refused boot names the table the server refused:\n{output}"
    );

    // THE CONTROL: the grant restored, the same boot listens.
    for (table, columns) in &granted {
        grant_relay(&db, table, columns).await;
    }
    let port = free_port();
    let log = temp.path().join("admitted.log");
    let mut admitted = start_relay(&platform.relay_url(), port, &cert, &key, &log, &[]);
    until_listening(&mut admitted, port, &log).await;
    drop(admitted);
    drop(refused);
    db.close().await;
}

/// **A malformed database URL is refused without echoing its password.**
///
/// The relay's database URL carries its login's password, and the boot error
/// the relay logs is that error's whole cause chain. A driver that quoted the
/// offending token in a parse error would write the password into the relay's
/// log. Each marker is a password the driver refuses to parse, one for a
/// malformed escape and one for an escaped NUL; the refusal must say what
/// failed - the control, `invalid connection string` - and never the value.
///
/// No server is reached: the URL is refused before any connection is made.
#[compio::test]
async fn a_malformed_database_url_is_refused_without_echoing_its_password() {
    let temp = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = temp.path().join("cert.pem");
    let key = temp.path().join("key.pem");
    std::fs::write(&cert, certificate.cert.pem()).unwrap();
    std::fs::write(&key, certificate.signing_key.serialize_pem()).unwrap();
    let secret = "SECRETMARKER";
    let markers = [format!("hunter2%zz{secret}"), format!("hunter2%00{secret}")];
    for marker in &markers {
        let url = format!("postgres://zeroship_cdc:{marker}@127.0.0.1:1/zeroship");
        let port = free_port();
        let log = temp.path().join("refused.log");
        let mut relay = start_relay(&url, port, &cert, &key, &log, &[]);
        let until = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = relay.0.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < until,
                "the relay neither exited nor listened"
            );
            compio::time::sleep(Duration::from_millis(20)).await;
        };
        let output = std::fs::read_to_string(&log).unwrap();
        assert!(
            !status.success(),
            "a malformed database URL fails the boot: {status}\n{output}"
        );
        assert!(
            output.contains("invalid connection string"),
            "the refusal names what failed:\n{output}"
        );
        assert!(
            !output.contains(secret) && !output.contains("hunter2"),
            "the relay's log must not carry the password from `{marker}`:\n{output}"
        );
    }
}
