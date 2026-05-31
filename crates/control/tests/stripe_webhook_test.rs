//! Live-PG regression tests for Stripe webhook audit coverage.

#![allow(clippy::future_not_send)]

use std::path::PathBuf;
use std::sync::Arc;

use ntex::http::StatusCode;
use ntex::web::{self, test};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::{
    stripe_handlers, token_handlers, AppState, EnvStore, Quota, RateLimiter, Registry,
    SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .or_else(|_| std::env::var("AUTH_DB_URL"))
        .ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-stripe-webhook-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Fixture {
    async fn new(db_url: &str, label: &str) -> Self {
        let blob_root = tmpdir(&format!("blob-{label}"));
        let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
        let registry = Registry::new(db_url).await.expect("registry");
        let env_store =
            EnvStore::new(registry.clone(), "test-master-key", false).expect("env store");
        let stripe_store = StripeStore::new(registry.clone());
        let blob_store: Arc<dyn BlobStore> =
            Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
        let vfs: Arc<dyn BundleStore + Send + Sync> =
            Arc::new(LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"));
        let (auth_pg_client, auth_pg_conn) =
            compio_postgres::connect(db_url, compio_postgres::NoTls)
                .await
                .expect("auth-pg connect");
        compio::runtime::spawn(async move {
            let _ = auth_pg_conn.run().await;
        })
        .detach();

        let state = Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            vfs,
            blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new("test-master-key".to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: true,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            auth_pg: Arc::new(auth_pg_client),
            auth_db_url: db_url.to_string(),
            hydra_admin_url: "http://127.0.0.1:4445".to_string(),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            pat_issuer: Arc::new(token_handlers::PatIssuer::dev_insecure()),
            hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
                "http://127.0.0.1:9",
            )),
            logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
            pairwise_salt: [0u8; 32],
        });

        Self {
            state,
            blob_root,
            deploy_tmp_dir,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

macro_rules! init_control {
    ($fx:expr) => {{
        test::init_service(web::App::new().state($fx.state.clone()).service(
            web::resource("/internal/webhooks/stripe").route(web::post().to(stripe_handlers::webhook)),
        ))
        .await
    }};
}

#[compio::test]
async fn invoice_paid_webhook_records_app_audit_row() {
    let Some(db_url) = db_url() else {
        eprintln!("[stripe_webhook_test] DB URL not set - skipping");
        return;
    };
    let fx = Fixture::new(&db_url, "record-audit").await;
    let app = init_control!(fx);
    let creator_id = Uuid::new_v4();
    fx.state
        .stripe_store
        .link_account(creator_id, "acct_webhookAudit1")
        .await
        .expect("link stripe account");

    let event_id = format!("evt_audit_{}", Uuid::new_v4().simple());
    let stripe_object_id = format!("in_audit_{}", Uuid::new_v4().simple());
    let body = json!({
        "id": event_id,
        "type": "invoice.paid",
        "created": 1_777_017_600i64,
        "data": {
            "object": {
                "id": stripe_object_id,
                "amount_paid": 1234,
                "application_fee_amount": 185,
                "currency": "usd",
                "metadata": {
                    "creator_id": creator_id.to_string(),
                }
            }
        }
    });
    let req = test::TestRequest::post()
        .uri("/internal/webhooks/stripe")
        .header("content-type", "application/json")
        .set_payload(body.to_string())
        .to_request();
    let resp = test::call_service(&app, req).await;

    assert_eq!(resp.status(), StatusCode::OK);
    let response_body: Value =
        serde_json::from_slice(&test::read_body(resp).await).expect("webhook response json");
    assert_eq!(response_body["status"], "recorded");

    let (conn, conn_driver) = compio_postgres::connect(&db_url, compio_postgres::NoTls)
        .await
        .expect("registry conn");
    compio::runtime::spawn(async move {
        let _ = conn_driver.run().await;
    })
    .detach();
    let rows = conn
        .query(
            "SELECT resource, detail \
             FROM app_audit \
             WHERE creator_id = $1 AND action = 'record_payout' AND resource = $2",
            &[&creator_id, &event_id],
        )
        .await
        .expect("select payout audit");
    assert_eq!(rows.len(), 1);
    let detail: Value = rows[0].get("detail");
    assert_eq!(detail["creator_id"], creator_id.to_string());
    assert_eq!(detail["amount_cents"], 1234);
    assert_eq!(detail["stripe_event_id"], event_id);
    assert_eq!(detail["stripe_object_id"], stripe_object_id);
}
