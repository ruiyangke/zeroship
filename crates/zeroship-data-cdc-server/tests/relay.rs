//! Separate-process relay test. PostgreSQL is mandatory.

use compio_postgres::Pool;
use compio_tls::TlsConnector;
use compio_ws::{tungstenite::Message, WebSocketStream};
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
    let admin_url = url::Url::parse(&zeroship_core::config::test_database_url()).unwrap();
    let admin = Pool::connect(admin_url.as_str(), 2)
        .await
        .expect("required PostgreSQL");
    let suffix = zeroship_core::typed_id::generate("tst");
    let relay_role = format!("relay_{suffix}");
    let worker_role = format!("worker_{suffix}");
    admin.batch_execute(&format!("CREATE ROLE \"{relay_role}\" LOGIN REPLICATION NOSUPERUSER NOBYPASSRLS PASSWORD 'fixture'; CREATE ROLE \"{worker_role}\" LOGIN NOREPLICATION NOSUPERUSER NOBYPASSRLS PASSWORD 'fixture'")).await.unwrap();
    let db = Pool::connect(admin_url.as_str(), 2).await.unwrap();
    let app = zeroship_core::typed_id::generate(zeroship_core::typed_id::APP_PREFIX);
    let publication = zeroship_core::replication_names::publication_name(&app).unwrap();
    let slot = publication.replacen("__zs_pub_", "__zs_relay_", 1);
    db.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS zeroship; CREATE TABLE IF NOT EXISTS zeroship.worker_instances (id text PRIMARY KEY, ring_key bytea NOT NULL, public_key bytea NOT NULL, advertise_host inet NOT NULL, advertise_port int NOT NULL, registered_at timestamptz NOT NULL DEFAULT now(), status text NOT NULL CHECK (status IN ('active', 'draining', 'gone'))); GRANT USAGE ON SCHEMA zeroship TO \"{relay_role}\"; GRANT SELECT (id, status, public_key) ON zeroship.worker_instances TO \"{relay_role}\"; CREATE SCHEMA \"{app}\"; CREATE TABLE \"{app}\".orders (id int PRIMARY KEY, secret text); GRANT USAGE ON SCHEMA \"{app}\" TO \"{worker_role}\"; GRANT SELECT, INSERT, UPDATE, DELETE ON \"{app}\".orders TO \"{worker_role}\"; CREATE PUBLICATION \"{publication}\" FOR TABLE \"{app}\".orders")).await.unwrap();
    let first_id = zeroship_core::typed_id::generate("wkr");
    let second_id = zeroship_core::typed_id::generate("wkr");
    let first_key = ServiceSigningKey::generate();
    let second_key = ServiceSigningKey::generate();
    for (id, key) in [(&first_id, &first_key), (&second_id, &second_key)] {
        let public = key.verifying_key_bytes().to_vec();
        db.execute(
            "INSERT INTO zeroship.worker_instances (id, ring_key, public_key, advertise_host, advertise_port, status) VALUES ($1, $2, $2, '127.0.0.1'::inet, 8080, 'active')",
            &[id, &public],
        )
        .await
        .unwrap();
    }
    let temp = tempfile::tempdir().unwrap();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = temp.path().join("cert.pem");
    let key_path = temp.path().join("key.pem");
    std::fs::write(&cert_path, certificate.cert.pem()).unwrap();
    std::fs::write(&key_path, certificate.signing_key.serialize_pem()).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.cert.der().clone()).unwrap();
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut relay_url = admin_url.clone();
    relay_url.set_username(&relay_role).unwrap();
    relay_url.set_password(Some("fixture")).unwrap();
    let log = std::fs::File::create(temp.path().join("relay.log")).unwrap();
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_zeroship-data-cdc-server"))
            .args([
                "--no-config",
                "--listen",
                &format!("127.0.0.1:{port}"),
                "--transaction-changes",
                "2",
                "--tls-cert-file",
            ])
            .arg(&cert_path)
            .arg("--tls-key-file")
            .arg(&key_path)
            .env("ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL", relay_url.as_str())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "relay exited: {}",
            std::fs::read_to_string(temp.path().join("relay.log")).unwrap()
        );
        if compio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        assert!(Instant::now() < until, "relay listener timed out");
        compio::time::sleep(Duration::from_millis(20)).await;
    }
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
    let native = zeroship_data_orm::cdc::broker::subscribe(&app, "orders");
    let native_handle = client.spawn(&app).await.unwrap();
    let slots: i64 = db
        .query(
            "SELECT count(*) FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(slots, 1, "workers must share capture");
    let mut worker_url = admin_url.clone();
    worker_url.set_username(&worker_role).unwrap();
    worker_url.set_password(Some("fixture")).unwrap();
    let worker = Pool::connect(worker_url.as_str(), 1).await.unwrap();
    assert!(worker
        .query(
            "SELECT * FROM pg_create_logical_replication_slot('forbidden_worker', 'pgoutput')",
            &[]
        )
        .await
        .is_err());
    worker.batch_execute(&format!("BEGIN; INSERT INTO \"{app}\".orders VALUES (1, 'private'); ROLLBACK; INSERT INTO \"{app}\".orders VALUES (2, 'private')")).await.unwrap();
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
            "INSERT INTO \"{app}\".orders VALUES (3, 'private'), (4, 'private'), (5, 'private')"
        ))
        .await
        .unwrap();
    assert_eq!(receive(&mut first).await.unwrap(), Event::Resync);
    assert_eq!(receive(&mut second).await.unwrap(), Event::Resync);
    assert!(matches!(
        native_event(&native).await,
        zeroship_data_orm::cdc::SubscriptionMessage::Resync
    ));

    db.execute(
        "UPDATE zeroship.worker_instances SET status = 'gone' WHERE id = $1",
        &[&first_id],
    )
    .await
    .unwrap();
    assert!(
        receive(&mut first).await.is_err(),
        "revoked worker kept its stream"
    );
    worker
        .batch_execute(&format!(
            "INSERT INTO \"{app}\".orders VALUES (6, 'private')"
        ))
        .await
        .unwrap();
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
    db.execute(
        "DELETE FROM zeroship.worker_instances WHERE id = $1 OR id = $2",
        &[&first_id, &second_id],
    )
    .await
    .unwrap();
    db.batch_execute(&format!("DROP PUBLICATION \"{publication}\"; DROP SCHEMA \"{app}\" CASCADE; DROP OWNED BY \"{relay_role}\"; DROP OWNED BY \"{worker_role}\"")).await.unwrap();
    db.close().await;
    admin
        .batch_execute(&format!(
            "DROP ROLE \"{relay_role}\"; DROP ROLE \"{worker_role}\""
        ))
        .await
        .unwrap();
    admin.close().await;
}
