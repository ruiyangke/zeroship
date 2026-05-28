//! Live-PG regression tests for the dev zeroship-builder OAuth bootstrap.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

use compio_postgres::{connect, Client, NoTls};
use ntex::web::{self, HttpResponse};
use serde_json::{json, Value};
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
    zeroship_auth::store::migrations::migrate(&client)
        .await
        .expect("auth migrations");
    cleanup_builder_client(&client).await;
    client
}

async fn cleanup_builder_client(pg: &Client) {
    pg.execute(
        "DELETE FROM control.oauth_clients WHERE client_id = $1",
        &[&BUILDER_CLIENT_ID],
    )
    .await
    .expect("cleanup builder client");
}

fn config(hydra: &MockHydra, secret_path: PathBuf, enabled: bool) -> BuilderClientBootstrapConfig {
    BuilderClientBootstrapConfig {
        enabled,
        hydra_admin_url: hydra.base.clone(),
        redirect_uri: DEFAULT_BUILDER_REDIRECT_URI.to_string(),
        client_secret_path: secret_path,
    }
}

#[derive(Clone, Debug)]
struct RecordedHydraRequest {
    method: String,
    path: String,
    body: Value,
}

#[derive(Default)]
struct MockHydraState {
    requests: Vec<RecordedHydraRequest>,
}

struct MockHydra {
    base: String,
    state: Arc<Mutex<MockHydraState>>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockHydra {
    fn start() -> Self {
        let state = Arc::new(Mutex::new(MockHydraState::default()));
        let factory_state = state.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("mock-hydra-builder-bootstrap")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let state = factory_state.clone();
                        async move {
                            web::App::new().state(state).service(
                                web::resource("/admin/clients")
                                    .route(web::post().to(mock_create_client)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send mock hydra addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("mock hydra starts");
        Self {
            base: format!("http://{addr}"),
            state,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn requests(&self) -> Vec<RecordedHydraRequest> {
        self.state.lock().expect("mock hydra state").requests.clone()
    }
}

impl Drop for MockHydra {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn mock_create_client(
    body: web::types::Json<Value>,
    state: web::types::State<Arc<Mutex<MockHydraState>>>,
) -> HttpResponse {
    let body = body.into_inner();
    state
        .lock()
        .expect("mock hydra state")
        .requests
        .push(RecordedHydraRequest {
            method: "POST".to_string(),
            path: "/admin/clients".to_string(),
            body: body.clone(),
        });
    HttpResponse::Created().json(&body)
}

async fn count_builder_rows(pg: &Client) -> i64 {
    let rows = pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM control.oauth_clients WHERE client_id = $1",
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
    let pg = pg(&db_url).await;
    let hydra = MockHydra::start();
    let root = tmpdir("first");
    let secret_path = root.join("builder-client-secret");
    let cfg = config(&hydra, secret_path.clone(), true);

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
            "SELECT client_name, redirect_uris, scopes, skip_consent, created_by, hydra_client_id \
             FROM control.oauth_clients WHERE client_id = $1",
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
    assert!(!rows[0].get::<_, bool>("skip_consent"));
    assert!(rows[0].get::<_, Option<Uuid>>("created_by").is_none());
    assert_eq!(rows[0].get::<_, String>("hydra_client_id"), BUILDER_CLIENT_ID);

    let requests = hydra.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/admin/clients");
    assert_eq!(requests[0].body["client_id"], BUILDER_CLIENT_ID);
    assert_eq!(requests[0].body["client_name"], BUILDER_CLIENT_NAME);
    assert_eq!(
        requests[0].body["redirect_uris"],
        json!([DEFAULT_BUILDER_REDIRECT_URI])
    );
    assert_eq!(
        requests[0].body["grant_types"],
        json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(requests[0].body["response_types"], json!(["code"]));
    assert_eq!(
        requests[0].body["scope"],
        "apps:read apps:write apps:deploy env:read env:write secrets:read secrets:write deployments:read deployments:rollback"
    );
    assert_eq!(
        requests[0].body["token_endpoint_auth_method"],
        "client_secret_basic"
    );
    assert_eq!(requests[0].body["skip_consent"], false);
    assert_eq!(requests[0].body["client_secret"], secret);

    cleanup_builder_client(&pg).await;
    let _ = std::fs::remove_dir_all(root);
}

#[compio::test]
async fn bootstrap_is_idempotent_on_second_run() {
    let Some(db_url) = db_url() else {
        eprintln!("[bootstrap_builder_test] AUTH_DB_URL/PG_TEST_URL not set - skipping");
        return;
    };
    let pg = pg(&db_url).await;
    let hydra = MockHydra::start();
    let root = tmpdir("idempotent");
    let secret_path = root.join("builder-client-secret");
    let cfg = config(&hydra, secret_path.clone(), true);

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
    assert_eq!(hydra.requests().len(), 1);

    cleanup_builder_client(&pg).await;
    let _ = std::fs::remove_dir_all(root);
}

#[compio::test]
async fn bootstrap_disabled_does_nothing() {
    let Some(db_url) = db_url() else {
        eprintln!("[bootstrap_builder_test] AUTH_DB_URL/PG_TEST_URL not set - skipping");
        return;
    };
    let pg = pg(&db_url).await;
    let hydra = MockHydra::start();
    let root = tmpdir("disabled");
    let secret_path = root.join("builder-client-secret");
    let cfg = config(&hydra, secret_path.clone(), false);

    let result = bootstrap_builder_oauth_client(&pg, &cfg)
        .await
        .expect("disabled bootstrap");

    assert_eq!(result.status, BuilderClientBootstrapStatus::Disabled);
    assert_eq!(count_builder_rows(&pg).await, 0);
    assert!(!secret_path.exists());
    assert!(hydra.requests().is_empty());

    cleanup_builder_client(&pg).await;
    let _ = std::fs::remove_dir_all(root);
}
