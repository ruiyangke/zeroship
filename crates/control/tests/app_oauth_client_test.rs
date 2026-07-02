//! Live-Postgres integration tests for the per-app native OAuth client
//! lifecycle. These tests exercise the authoritative `zeroship.oauth_clients`
//! + `zeroship.app_oauth_clients` store, scope registry, route exposure, and
//! delete cascade without any remote OAuth-admin dependency.

use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore, ScopeDef};
use zeroship_control::app_oauth_client::{self, client_id_for_app, redirect_uris_for_hosts};
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

async fn pg(db_url: &str) -> Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

async fn seed_owner(pg: &Client, label: &str) -> Uuid {
    let owner_id = Uuid::new_v4();
    pg.execute(
        "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
        &[
            &owner_id,
            &format!("{label}-{owner_id}@zeroship.test"),
            &label,
        ],
    )
    .await
    .expect("seed owner user");
    owner_id
}

async fn count_oauth_client(pg: &Client, client_id: &str) -> i64 {
    pg.query(
        "SELECT COUNT(*)::BIGINT AS n FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&client_id],
    )
    .await
    .expect("count oauth client")[0]
        .get("n")
}

async fn redirect_uris(pg: &Client, client_id: &str) -> Vec<String> {
    pg.query(
        "SELECT redirect_uris FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&client_id],
    )
    .await
    .expect("select redirect_uris")[0]
        .get("redirect_uris")
}

async fn scopes(pg: &Client, client_id: &str) -> Vec<String> {
    pg.query(
        "SELECT scopes FROM zeroship.oauth_clients WHERE client_id = $1",
        &[&client_id],
    )
    .await
    .expect("select scopes")[0]
        .get("scopes")
}

#[compio::test]
async fn provision_asserts_native_db_scopes_routes_and_redirect_sync() {
    let Some(url) = db_url() else {
        eprintln!("[app_oauth_client_test] CONTROL_TEST_DB/PG_TEST_URL not set - skipping");
        return;
    };

    let registry = Registry::new(&url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let raw = pg(&url).await;
    let mut conn = pg(&url).await;

    let owner_id = seed_owner(&raw, "oac-owner").await;
    let app_name = format!("zs-1d-{}", Uuid::new_v4().simple());
    let app = registry
        .create_app(&app_name, &zeroship_control::bootstrap_console::free_plan_id(), &owner_id)
        .await
        .expect("create app");
    let app_id = app.id;
    let expected_client_id = client_id_for_app(&app_id);
    let scheme = "http";
    let apex = format!("{app_name}.zeroship.localhost");
    let declared = vec![ScopeDef {
        id: "read:billing".to_string(),
        label: "View billing".to_string(),
        description: Some("See invoices and plan.".to_string()),
    }];

    let client_id = app_oauth_client::ensure_app_client(
        &mut conn,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone()],
        &declared,
        false,
    )
    .await
    .expect("ensure_app_client");
    assert_eq!(client_id, expected_client_id);

    let want_uris = redirect_uris_for_hosts(scheme, &[apex.clone()]).unwrap();
    let oc = raw
        .query(
            "SELECT skip_consent, token_endpoint_auth_method, brokered, \
                    client_secret_hash, backchannel_logout_uri, redirect_uris, scopes, \
                    refresh_allowed \
             FROM zeroship.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("query oauth_clients");
    assert_eq!(oc.len(), 1, "oauth_clients row written");
    assert!(!oc[0].get::<_, bool>("skip_consent"));
    assert_eq!(
        oc[0].get::<_, String>("token_endpoint_auth_method"),
        "client_secret_basic"
    );
    assert!(oc[0].get::<_, bool>("brokered"));
    assert!(oc[0].get::<_, bool>("refresh_allowed"));
    assert!(
        oc[0]
            .get::<_, Option<String>>("client_secret_hash")
            .is_some_and(|hash| !hash.is_empty()),
        "control generated and stored a broker client_secret_hash"
    );
    assert_eq!(
        oc[0].get::<_, Option<String>>("backchannel_logout_uri").as_deref(),
        Some(format!("http://{apex}/oidc/backchannel-logout").as_str())
    );
    assert_eq!(oc[0].get::<_, Vec<String>>("redirect_uris"), want_uris);
    assert!(
        oc[0]
            .get::<_, Vec<String>>("scopes")
            .iter()
            .any(|scope| scope == "read:billing"),
        "oauth_clients.scopes mirror includes declared scope"
    );

    let ext = raw
        .query(
            "SELECT client_id, sector_identifier FROM zeroship.app_oauth_clients WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("query app_oauth_clients");
    assert_eq!(ext.len(), 1, "app_oauth_clients extension row written");
    assert_eq!(ext[0].get::<_, String>("client_id"), client_id);
    assert_eq!(
        ext[0].get::<_, String>("sector_identifier"),
        format!("http://{apex}")
    );

    let defs = raw
        .query(
            "SELECT scope_id, label, description FROM zeroship.app_scope_defs WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("query app_scope_defs");
    assert_eq!(defs.len(), 1, "one declared scope persisted");
    assert_eq!(defs[0].get::<_, String>("scope_id"), "read:billing");
    assert_eq!(defs[0].get::<_, String>("label"), "View billing");
    assert_eq!(
        defs[0].get::<_, Option<String>>("description").as_deref(),
        Some("See invoices and plan.")
    );

    let routes = registry.get_routes().await.expect("get_routes");
    let entry = routes.get(&app_id).expect("route entry for app");
    assert_eq!(entry.oauth_client_id.as_deref(), Some(client_id.as_str()));
    assert_eq!(
        entry.sector_identifier.as_deref(),
        Some(format!("http://{apex}").as_str())
    );

    app_oauth_client::ensure_app_client(
        &mut conn,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone()],
        &declared,
        false,
    )
    .await
    .expect("re-ensure is idempotent");
    assert_eq!(redirect_uris(&raw, &client_id).await.len(), want_uris.len());

    app_oauth_client::ensure_app_client(
        &mut conn,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone()],
        &[],
        false,
    )
    .await
    .expect("re-ensure with no scopes");
    let defs_after = raw
        .query(
            "SELECT scope_id FROM zeroship.app_scope_defs WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("query app_scope_defs after drop");
    assert!(defs_after.is_empty(), "dropped scope removed from registry");
    assert!(
        !scopes(&raw, &client_id)
            .await
            .iter()
            .any(|scope| scope == "read:billing"),
        "oauth_clients.scopes mirror dropped the removed declared scope"
    );

    let custom = "custom.example.test".to_string();
    let changed = app_oauth_client::sync_app_redirect_uris(
        &mut conn,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone(), custom.clone()],
    )
    .await
    .expect("sync redirect uris");
    assert!(changed, "adding a host must update the native redirect mirror");
    let extended = redirect_uris(&raw, &client_id).await;
    assert_eq!(extended.len(), 4, "2 hosts x 2 paths");
    assert!(extended.iter().any(|uri| uri.contains(&custom)));

    let noop = app_oauth_client::sync_app_redirect_uris(
        &mut conn,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone(), custom.clone()],
    )
    .await
    .expect("noop sync");
    assert!(!noop, "no-op deploy makes no native redirect update");

    app_oauth_client::ensure_app_client(
        &mut conn,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone()],
        &[],
        false,
    )
    .await
    .expect("apex-only re-ensure preserves custom redirect URIs");
    let preserved = redirect_uris(&raw, &client_id).await;
    assert_eq!(preserved.len(), 4, "ensure_app_client merges existing URIs");
    assert!(preserved.iter().any(|uri| uri.contains(&custom)));

    registry.delete_app(&app_id).await.expect("delete app");
}

async fn build_state(db_url: &str, app_base_domain: &str) -> Arc<AppState> {
    let blob_root = std::env::temp_dir().join(format!("oac-blob-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&blob_root).expect("mkdir blob root");
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
    let env_store =
        EnvStore::new(registry.clone(), "test-master-key-deadbeefcafebabe", false).expect("env");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let control_pg = Arc::new(pg(db_url).await);

    Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new("test-master-key-deadbeefcafebabe".to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: true,
        trust_proxy: false,
        deploy_tmp_dir: blob_root.clone(),
        control_pg,
        app_base_domain: app_base_domain.to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies().expect("authz policies"),
        pat_issuer: Arc::new(zeroship_authn::PatIssuer::dev_insecure()),
        auth_provider: zeroship_control::platform_auth_provider(
            "https://auth.zeroship.test/oauth2",
            Some("http://127.0.0.1:9/oauth2/.well-known/jwks.json".to_string()),
        ),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
    })
}

#[compio::test]
async fn appstate_provision_then_purge_deletes_native_oauth_rows() {
    let Some(url) = db_url() else {
        eprintln!("[app_oauth_client_test] CONTROL_TEST_DB/PG_TEST_URL not set - skipping");
        return;
    };

    let app_base_domain = "zeroship.localhost";
    let state = build_state(&url, app_base_domain).await;
    let owner_id = seed_owner(&state.control_pg, "oac-state-owner").await;

    let app_name = format!("zs-1d-state-{}", Uuid::new_v4().simple());
    let app = state
        .registry
        .create_app(
            &app_name,
            &zeroship_control::bootstrap_console::free_plan_id(),
            &owner_id,
        )
        .await
        .expect("create app");
    let app_id = app.id;
    let client_id = state
        .provision_app_oauth_client(&app_id, &app_name, &[])
        .await
        .expect("provision via AppState");
    assert_eq!(client_id, client_id_for_app(&app_id));

    let expected_apex = format!("{app_name}.{app_base_domain}");
    let uris = redirect_uris(&state.control_pg, &client_id).await;
    assert!(
        uris.iter()
            .any(|uri| uri == &format!("http://{expected_apex}/__zeroship/auth/callback")),
        "apex host derived by provision_app_oauth_client must match gateway host {expected_apex}: {uris:?}"
    );
    let routes = state.registry.get_routes().await.expect("get_routes");
    let entry = routes.get(&app_id).expect("route entry");
    assert_eq!(
        entry.sector_identifier.as_deref(),
        Some(format!("http://{expected_apex}").as_str())
    );
    assert_eq!(count_oauth_client(&state.control_pg, &client_id).await, 1);

    assert!(api::purge_app(&state, &app_id)
        .await
        .expect("purge app"));
    assert_eq!(count_oauth_client(&state.control_pg, &client_id).await, 0);
    let ext_count: i64 = state
        .control_pg
        .query(
            "SELECT COUNT(*)::BIGINT AS n FROM zeroship.app_oauth_clients WHERE app_id = $1",
            &[&app_id],
        )
        .await
        .expect("count app_oauth_clients")[0]
        .get("n");
    assert_eq!(ext_count, 0, "app_oauth_clients row cascaded with app delete");
}
