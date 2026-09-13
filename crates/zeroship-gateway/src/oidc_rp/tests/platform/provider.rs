use super::*;
use ed25519_dalek::SigningKey;
use std::io::Write;
use std::path::Path;
use zeroship_auth::{
    config::AuthConfig,
    oidc::{BrokerSecrets, Issuer},
};

pub struct Provider {
    // Stop the HTTP runtime before removing the files its handlers read.
    _server: test::TestServer,
    pub base: String,
    pub issuer: Arc<Issuer>,
    pub signing: SigningKey,
    pub files: tempfile::TempDir,
}

impl Provider {
    pub async fn start(database: &Database) -> Self {
        let files = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut url = database.url.clone();
        url.set_username("zeroship_auth").unwrap();
        url.set_password(Some("zeroship_auth")).unwrap();
        let config = Arc::new(config(files.path(), url.as_str(), &base));
        let signing = SigningKey::from_bytes(&[42; 32]);
        let issuer = Arc::new(
            Issuer::from_signing_key(&signing, PAIRWISE_SALT, ISSUER.to_owned())
                .unwrap()
                .with_broker_secrets(BrokerSecrets::new(BROKER_MASTER.to_vec(), None).unwrap()),
        );
        let server_issuer = issuer.clone();
        let (ready_tx, ready_rx) = flume::bounded(1);
        let server = test::server_with(test::config().listener(listener), move || {
            let config = config.clone();
            let issuer = server_issuer.clone();
            let url = url.clone();
            let ready = ready_tx.clone();
            async move {
                // Construct and drive the non-Send client on the provider's runtime.
                let (client, connection) =
                    compio_postgres::connect(url.as_str(), compio_postgres::NoTls)
                        .await
                        .expect("connect the auth service role");
                let driver = compio::runtime::spawn(async move { connection.run().await });
                issuer
                    .publish_active_key(&client)
                    .await
                    .expect("publish this database's signing key");
                let pool = zeroship_auth::oidc::refresh::RefreshSessionPool::new(url.as_str(), 1);
                let app = web::App::new()
                    .state(config)
                    .state(Arc::new(client))
                    .state(driver)
                    .state(issuer)
                    .state(pool)
                    .middleware(zeroship_auth::headers::SecurityHeaders::default())
                    .configure(zeroship_auth::server::configure(false, false));
                ready.send(()).unwrap();
                app
            }
        })
        .await;
        ready_rx
            .recv_async()
            .await
            .expect("provider initialized with its database and key");
        Self {
            _server: server,
            base,
            issuer,
            signing,
            files,
        }
    }
}

fn config(directory: &Path, database_url: &str, base: &str) -> AuthConfig {
    let write = |name: &str, bytes: &[u8]| {
        let path = directory.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path).unwrap().write_all(bytes).unwrap();
        path.to_str().unwrap().to_owned()
    };
    let database = write("database-url", database_url.as_bytes());
    let stash = write("stash", b"provider-fixture-stash-secret-32-bytes");
    let totp = write(
        "totp",
        b"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
    );
    let refresh = write(
        "refresh-hmac",
        b"1:000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n",
    );
    let idempotency = write(
        "refresh-idempotency",
        b"provider-fixture-idempotency-secret",
    );
    let mut config = AuthConfig::parse_from([
        "zeroship-auth",
        "--addr",
        "127.0.0.1:0",
        "--database-url-file",
        &database,
        "--stash-signing-key-file",
        &stash,
        "--totp-enc-key-file",
        &totp,
        "--mail-from-email",
        "test@zeroship.test",
        "--mail-from-name",
        "Test",
        "--public-url",
        base,
    ]);
    config.settings.refresh_hash_key_file = zeroship_core::config::Operational::new(refresh.into());
    config.settings.refresh_idem_key_file =
        zeroship_core::config::Operational::new(idempotency.into());
    config
}

#[ntex::test]
async fn failed_flow_releases_provider_connections_and_secret_files() {
    use futures::FutureExt;
    use std::{cell::RefCell, panic::AssertUnwindSafe};
    let directory = RefCell::new(None);
    let failure = AssertUnwindSafe(Database::migrated(async |database| {
        let app = App::seed(database, REDIRECT_URI).await;
        let provider = Provider::start(database).await;
        *directory.borrow_mut() = Some(provider.files.path().to_owned());
        let granted = tokens(&provider, &app).await;
        assert!(granted.refresh_token.is_some());
        panic!("intentional provider fixture failure");
    }))
    .catch_unwind()
    .await
    .expect_err("propagate the failed flow after draining its database");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"intentional provider fixture failure")
    );
    assert!(
        !directory.borrow().as_ref().unwrap().exists(),
        "the provider leaked its secret directory"
    );
}
