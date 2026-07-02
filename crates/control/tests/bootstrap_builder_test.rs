//! Live-PG regression tests for the dev zeroship-builder OAuth bootstrap.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_control::bootstrap_builder::{
    bootstrap_builder_oauth_client, BuilderClientBootstrapConfig, BuilderClientBootstrapStatus,
    BUILDER_CLIENT_ID, BUILDER_CLIENT_NAME, DEFAULT_BUILDER_REDIRECT_URI,
};

fn db_url() -> Option<String> {
    std::env::var("AUTH_DB_URL")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

/// All three bootstrap tests operate on the *same* singleton OAuth client row
/// (`BUILDER_CLIENT_ID` is a hard-coded constant — only one builder client can
/// ever exist). The default test harness runs them concurrently, so one test's
/// `cleanup_builder_client` DELETE and another's INSERT race on the
/// `oauth_clients_pkey` (client_id) primary key, surfacing as `Db("db error")`.
/// Serialize them on a process-wide lock so each still exercises the real
/// check-then-insert path against live PG without stomping the shared row.
fn bootstrap_guard() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-builder-bootstrap-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

async fn pg(db_url: &str) -> Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    cleanup_builder_client(&client).await;
    client
}

async fn cleanup_builder_client(pg: &Client) {
    pg.execute(
        "DELETE FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&BUILDER_CLIENT_ID],
    )
    .await
    .expect("cleanup builder client");
}

fn config(secret_path: PathBuf, enabled: bool) -> BuilderClientBootstrapConfig {
    BuilderClientBootstrapConfig {
        enabled,
        redirect_uri: DEFAULT_BUILDER_REDIRECT_URI.to_string(),
        client_secret_path: secret_path,
        skip_consent: true,
    }
}

async fn count_builder_rows(pg: &Client) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&BUILDER_CLIENT_ID],
        )
        .await
        .expect("count builder rows");
    rows[0].get("n")
}

#[compio::test]
async fn bootstrap_inserts_builder_client_first_run() {
    let Some(db_url) = db_url() else {
        eprintln!("[bootstrap_builder_test] AUTH_DB_URL/PG_TEST_URL not set - skipping");
        return;
    };
    let _serial = bootstrap_guard();
    let pg = pg(&db_url).await;
    let root = tmpdir("first");
    let secret_path = root.join("builder-client-secret");
    let cfg = config(secret_path.clone(), true);

    let result = bootstrap_builder_oauth_client(&pg, &cfg)
        .await
        .expect("bootstrap builder client");

    assert_eq!(result.status, BuilderClientBootstrapStatus::Created);
    assert_eq!(count_builder_rows(&pg).await, 1);
    let secret = std::fs::read_to_string(&secret_path).expect("secret file");
    assert_eq!(secret.len(), 64);
    assert!(secret.bytes().all(|b| b.is_ascii_hexdigit()));

    let rows = pg
        .query(
            "SELECT client_name, redirect_uris, scopes, skip_consent, created_by, \
                    client_secret_hash, refresh_allowed, \
                    token_endpoint_auth_method \
             FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&BUILDER_CLIENT_ID],
        )
        .await
        .expect("select builder client");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("client_name"), BUILDER_CLIENT_NAME);
    assert_eq!(
        rows[0].get::<_, Vec<String>>("redirect_uris"),
        vec![DEFAULT_BUILDER_REDIRECT_URI.to_string()]
    );
    assert_eq!(
        rows[0].get::<_, Vec<String>>("scopes"),
        vec![
            "apps:read",
            "apps:write",
            "apps:deploy",
            "env:read",
            "env:write",
            "secrets:read",
            "secrets:write",
            "deployments:read",
            "deployments:rollback",
        ]
    );
    assert!(rows[0].get::<_, bool>("skip_consent"));
    assert!(rows[0].get::<_, Option<Uuid>>("created_by").is_none());
    assert!(rows[0].get::<_, bool>("refresh_allowed"));
    assert_eq!(
        rows[0].get::<_, String>("token_endpoint_auth_method"),
        "client_secret_basic"
    );
    let hash = rows[0]
        .get::<_, Option<String>>("client_secret_hash")
        .expect("client_secret_hash");
    assert!(
        zeroship_core::auth::validate_api_key(&secret, &hash),
        "stored hash validates the generated builder secret"
    );

    cleanup_builder_client(&pg).await;
    let _ = std::fs::remove_dir_all(root);
}

#[compio::test]
async fn bootstrap_is_idempotent_on_second_run() {
    let Some(db_url) = db_url() else {
        eprintln!("[bootstrap_builder_test] AUTH_DB_URL/PG_TEST_URL not set - skipping");
        return;
    };
    let _serial = bootstrap_guard();
    let pg = pg(&db_url).await;
    let root = tmpdir("idempotent");
    let secret_path = root.join("builder-client-secret");
    let cfg = config(secret_path.clone(), true);

    let first = bootstrap_builder_oauth_client(&pg, &cfg)
        .await
        .expect("first bootstrap");
    let first_secret = std::fs::read_to_string(&secret_path).expect("first secret");
    let second = bootstrap_builder_oauth_client(&pg, &cfg)
        .await
        .expect("second bootstrap");
    let second_secret = std::fs::read_to_string(&secret_path).expect("second secret");

    assert_eq!(first.status, BuilderClientBootstrapStatus::Created);
    assert_eq!(second.status, BuilderClientBootstrapStatus::AlreadyPresent);
    assert_eq!(first_secret, second_secret);
    assert_eq!(count_builder_rows(&pg).await, 1);

    cleanup_builder_client(&pg).await;
    let _ = std::fs::remove_dir_all(root);
}

#[compio::test]
async fn bootstrap_disabled_does_nothing() {
    let Some(db_url) = db_url() else {
        eprintln!("[bootstrap_builder_test] AUTH_DB_URL/PG_TEST_URL not set - skipping");
        return;
    };
    let _serial = bootstrap_guard();
    let pg = pg(&db_url).await;
    let root = tmpdir("disabled");
    let secret_path = root.join("builder-client-secret");
    let cfg = config(secret_path.clone(), false);

    let result = bootstrap_builder_oauth_client(&pg, &cfg)
        .await
        .expect("disabled bootstrap");

    assert_eq!(result.status, BuilderClientBootstrapStatus::Disabled);
    assert_eq!(count_builder_rows(&pg).await, 0);
    assert!(!secret_path.exists());

    cleanup_builder_client(&pg).await;
    let _ = std::fs::remove_dir_all(root);
}
