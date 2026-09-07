//! Regression test for the billing-schema redesign (change 3): the
//! `GET /api/apps/:id/spend-limit` handler is rewritten to the double LEFT JOIN
//! (`apps ⋈ app_spend_limit ⋈ app_spend_state`), reading the override from
//! `app_spend_limit.spend_limit_cents` (the config table it MOVED to) and the
//! `state` from `app_spend_state`. The `override ?? plan_default` resolution is
//! unchanged.
//!
//! FAITHFUL by construction: drives the REAL `api::get_spend_limit` ntex handler
//! through a REAL `AuthzGuard` (a real BillingRead PAT) against a live, migrated
//! Postgres. After `SpendEngine::set_limit` writes the override to
//! `app_spend_limit`, the endpoint must return it. (RED before the rewrite: the
//! handler read `s.spend_limit_cents` from `app_spend_state`, a column that no
//! longer exists there — the query would error.)
//!
//! Gated on a configured test database
//! (`common::require_control_db`); an absent or unmigrated one REFUSES
//! the run.

#![allow(clippy::future_not_send)]

use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use ntex::web::{self, test};
use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::spend::SpendEngine;
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn db_url() -> String {
    common::require_control_db()
}

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-spendlimit-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

async fn build_state(db_url: &str) -> (Arc<AppState>, PathBuf, PathBuf) {
    let blob_root = tmpdir("blob");
    let deploy_tmp_dir = tmpdir("dtmp");
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );
    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();

    let state = Arc::new(AppState {
        service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
        registry,
        env_store,
        stripe_store,
        blob_store,
            workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new("sk_test_mock".to_string()),
        stripe_base_url: "http://127.0.0.1:9".to_string(),
        gateway_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg: Arc::new(control_pg_client),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        auth_provider: zeroship_control::platform_auth_provider("https://auth.zeroship.test/oauth2", Some(common::platform_jwks_url())),
        // No platform deploy-token mint here: that is control's OUTBOUND
        // destination for the device flow, and no fixture below drives one.
        provider_registry: zeroship_control::metering::provider::builtin_registry(),
        billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
        billing_stream: None,
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    });
    (state, blob_root, deploy_tmp_dir)
}

/// Seed a plan and an app on it, OWNED by `owner_id`.
///
/// The owner SEAT is what authorizes `billing:read` on the app: an app reaches
/// its authority root through `apps.project_id -> projects.organization_id`, so
/// ownership is an `organization_members` row on the organization behind the
/// app's project. This helper used to insert the app with no membership at all
/// and rely on the caller holding the deleted universal-allow platform role.
async fn make_app_with_plan_default(
    state: &AppState,
    plan_default: i64,
    owner_id: uuid::Uuid,
) -> Uuid {
    let plan_id = format!("pln_sl_{}", Uuid::new_v4().simple());
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'sl', 0, 0, NULL, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', $2)",
            &[&plan_id, &plan_default],
        )
        .await
        .expect("seed plan");
    let app_id: Uuid = common::seed_app(
        &state.control_pg,
        &format!("sl-{}", Uuid::new_v4()),
        &plan_id,
    )
    .await;
    common::seat_app_organization_member(&state.control_pg, &app_id, &owner_id, "owner").await;
    app_id
}

fn spend_limit_route(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/apps/{id}/spend-limit")
            .route(web::get().to(api::get_spend_limit)),
    );
}

#[compio::test]
async fn get_spend_limit_returns_the_app_spend_limit_override() {
    let url = db_url();
    let (state, blob_root, deploy_tmp_dir) = build_state(&url).await;
    let pat = common::authz_fixture::seeded_principal(&state).await;

    // Plan default 1000c; no override yet ⇒ effective == plan default.
    let app = make_app_with_plan_default(&state, 1000, pat.user_id).await;

    let svc = test::init_service(
        web::App::new().state(state.clone()).configure(spend_limit_route),
    )
    .await;

    let req = test::TestRequest::get()
        .uri(&format!("/api/apps/{app}/spend-limit"))
        .header("authorization", pat.bearer())
        .to_request();
    let body: serde_json::Value = test::read_response_json(&svc, req).await;
    assert_eq!(body["override_cents"], serde_json::Value::Null, "no override yet");
    assert_eq!(body["plan_default_cents"], 1000);
    assert_eq!(body["effective_limit_cents"], 1000, "effective == plan default when no override");

    // Set an override via the REAL SpendEngine (writes app_spend_limit ONLY).
    let engine = SpendEngine::new(state.registry.clone());
    engine.set_limit(&app, Some(250)).await.expect("set override");

    // The endpoint must now reflect the override read from app_spend_limit.
    let req2 = test::TestRequest::get()
        .uri(&format!("/api/apps/{app}/spend-limit"))
        .header("authorization", pat.bearer())
        .to_request();
    let body2: serde_json::Value = test::read_response_json(&svc, req2).await;
    assert_eq!(
        body2["override_cents"], 250,
        "GET /spend-limit returns the app_spend_limit override (double LEFT JOIN)",
    );
    assert_eq!(
        body2["effective_limit_cents"], 250,
        "effective == override when set (override ?? plan_default unchanged)",
    );

    pat.cleanup(&state).await;
    let _ = std::fs::remove_dir_all(&blob_root);
    let _ = std::fs::remove_dir_all(&deploy_tmp_dir);

    // Teardown: the spend engine, the service, and the app state all hold (or
    // share) a Postgres connection, and locals are dropped only after the body
    // returns - by which point the runtime is gone and the sockets can no
    // longer be closed. Drop them explicitly, then wait for the close to land.
    drop(engine);
    drop(svc);
    drop(state);
    common::drain_pg().await;
}
