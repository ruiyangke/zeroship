//! Live-infra integration test for the per-app OAuth client lifecycle
//! (auth-sdk Slice 1d). FAITHFUL path: it provisions a real per-app public
//! PKCE client against a live Hydra admin API, asserts the client shape via
//! that same API, asserts the control-plane DB rows, and asserts
//! `Registry::get_routes` LEFT-JOINs `control.app_oauth_clients` to surface
//! `RouteEntry.oauth_client_id` / `sector_identifier`.
//!
//! REQUIRES the docker-compose stack (Hydra admin + a migrated Postgres):
//!   - `CONTROL_TEST_DB` or `PG_TEST_URL` — DSN of a Postgres with the
//!     `db/changelog` migrations applied (incl. 0005_control_app_oauth_clients).
//!   - `HYDRA_ADMIN_URL` — Hydra admin API base (e.g. http://127.0.0.1:4445).
//! Skips (prints why, returns) when either is absent. The pure
//! reconciliation/derivation logic is covered by the crate's `app_oauth_client`
//! unit tests, which run without any infra.

use std::sync::Arc;

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;
use zeroship_auth::hydra_client::HydraAdmin;
use zeroship_bundle::{BlobStore, BundleStore, LocalDiskBlobStore, LocalFs};
use zeroship_control::app_oauth_client::{
    self, client_id_for_app, redirect_uris_for_hosts,
};
use zeroship_control::{
    oidc_rp, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
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

/// Full faithful lifecycle: create app → ensure_app_client → assert Hydra +
/// DB + get_routes → reconcile (add host) → delete.
#[compio::test]
async fn provision_asserts_hydra_db_and_routes() {
    let (Some(url), Ok(hydra_url)) = (db_url(), std::env::var("HYDRA_ADMIN_URL")) else {
        eprintln!(
            "[app_oauth_client_test] CONTROL_TEST_DB/PG_TEST_URL or HYDRA_ADMIN_URL not set - skipping"
        );
        return;
    };

    let registry = Registry::new(&url).await.expect("registry");
    let raw = pg(&url).await;
    let mut conn = pg(&url).await; // owned, mutable — for the transactional upsert
    let hydra = HydraAdmin::new(hydra_url.clone());

    // 1. Create a uniquely-named app so reruns don't collide.
    let app_name = format!("zs-1d-{}", Uuid::new_v4().simple());
    let app = registry
        .create_app(&app_name, "free")
        .await
        .expect("create app");
    let app_id = app.id;
    let expected_client_id = client_id_for_app(&app_id);
    let scheme = "http";
    let apex = format!("{app_name}.zeroship.localhost");

    // Best-effort cleanup of any Hydra residue from a prior aborted run.
    let _ = hydra.delete_client(&expected_client_id).await;

    // 2. Provision.
    let client_id = app_oauth_client::ensure_app_client(
        &mut conn,
        &hydra,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone()],
    )
    .await
    .expect("ensure_app_client");
    assert_eq!(client_id, expected_client_id);

    // 3. Assert the Hydra client shape via the admin API (FAITHFUL).
    let hydra_client = hydra
        .get_client(&client_id)
        .await
        .expect("get_client")
        .expect("client exists in hydra");
    assert_eq!(hydra_client.token_endpoint_auth_method, "none", "public PKCE");
    assert_eq!(hydra_client.subject_type, "public");
    assert!(hydra_client.grant_types.iter().any(|g| g == "authorization_code"));
    assert!(hydra_client.grant_types.iter().any(|g| g == "refresh_token"));
    assert!(hydra_client.response_types.iter().any(|r| r == "code"));
    assert!(hydra_client.scope.contains("openid"));
    assert!(hydra_client.scope.contains("offline_access"));
    assert!(!hydra_client.skip_consent, "per-app clients never skip consent");
    assert_eq!(
        hydra_client.backchannel_logout_uri.as_deref(),
        Some(format!("http://{apex}/oidc/backchannel-logout").as_str()),
        "per-app BCL identity"
    );
    let want_uris = redirect_uris_for_hosts(scheme, &[apex.clone()]).unwrap();
    for uri in &want_uris {
        assert!(
            hydra_client.redirect_uris.contains(uri),
            "hydra redirect_uris missing {uri}"
        );
    }

    // 4. Assert the control-plane DB rows.
    let oc = raw
        .query(
            "SELECT skip_consent, hydra_client_id FROM control.oauth_clients WHERE client_id = $1",
            &[&client_id],
        )
        .await
        .expect("query oauth_clients");
    assert_eq!(oc.len(), 1, "oauth_clients row written");
    assert!(!oc[0].get::<_, bool>("skip_consent"));
    assert_eq!(oc[0].get::<_, String>("hydra_client_id"), client_id);

    let ext = raw
        .query(
            "SELECT client_id, sector_identifier FROM control.app_oauth_clients WHERE app_id = $1",
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

    // 5. get_routes LEFT JOIN surfaces the OAuth fields on the RouteEntry.
    let routes = registry.get_routes().await.expect("get_routes");
    let entry = routes.get(&app_id).expect("route entry for app");
    assert_eq!(entry.oauth_client_id.as_deref(), Some(client_id.as_str()));
    assert_eq!(
        entry.sector_identifier.as_deref(),
        Some(format!("http://{apex}").as_str())
    );

    // 6. Idempotent re-provision, same host → no Hydra redirect change.
    app_oauth_client::ensure_app_client(
        &mut conn, &hydra, &app_id, &app_name, scheme, &[apex.clone()],
    )
    .await
    .expect("re-ensure is idempotent");
    let after = hydra
        .get_client(&client_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(
        after.redirect_uris.len(),
        want_uris.len(),
        "no redirect_uri drift on idempotent re-run"
    );

    // 7. Reconcile: add a custom domain → diff-then-PUT extends redirect_uris.
    let custom = "custom.example.test".to_string();
    let did_put = app_oauth_client::sync_app_redirect_uris(
        &mut conn,
        &hydra,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone(), custom.clone()],
    )
    .await
    .expect("sync redirect uris");
    assert!(did_put, "adding a host must PUT");
    let extended = hydra
        .get_client(&client_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(extended.redirect_uris.len(), 4, "2 hosts × 2 paths");
    assert!(extended
        .redirect_uris
        .iter()
        .any(|u| u.contains(&custom)));

    // 8. A no-op sync (same host set) must NOT PUT.
    let noop = app_oauth_client::sync_app_redirect_uris(
        &mut conn,
        &hydra,
        &app_id,
        &app_name,
        scheme,
        &[apex.clone(), custom.clone()],
    )
    .await
    .expect("noop sync");
    assert!(!noop, "no-op deploy makes no Hydra PUT");

    // 9. Cleanup: delete the Hydra client + app row (DB rows cascade).
    app_oauth_client::delete_app_client(&hydra, &app_id)
        .await
        .expect("delete hydra client");
    assert!(
        hydra.get_client(&client_id).await.expect("get").is_none(),
        "hydra client deleted"
    );
    registry.delete_app(&app_id).await.expect("delete app");
}

/// Build a minimal real `AppState` pointed at the live PG + Hydra so the
/// PRODUCTION entry points (`provision_app_oauth_client` /
/// `delete_app_oauth_client`) and their apex-host derivation are exercised
/// end-to-end — not just the lower-level `ensure_app_client`. `insecure_dev =
/// true` so `app_scheme()` yields `http` (matching the dev compose stack).
async fn build_state(db_url: &str, hydra_admin_url: &str, app_base_domain: &str) -> Arc<AppState> {
    let blob_root = std::env::temp_dir().join(format!("oac-blob-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&blob_root).expect("mkdir blob root");
    let registry = Registry::new(db_url).await.expect("registry");
    let env_store =
        EnvStore::new(registry.clone(), "test-master-key-deadbeefcafebabe", false).expect("env");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let vfs: Arc<dyn BundleStore + Send + Sync> =
        Arc::new(LocalFs::new(blob_root.join("legacy-bundles")).expect("vfs"));
    let oidc_rp = Arc::new(oidc_rp::ConsoleOidcRp::new(
        "http://localhost:4444",
        "console.zeroship.ai",
        "test-oidc-secret".to_string(),
        b"test-stash-key".to_vec(),
    ));
    let auth_pg = Arc::new(pg(db_url).await);

    Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        vfs,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new("test-master-key-deadbeefcafebabe".to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: true,
        trust_proxy: false,
        deploy_tmp_dir: blob_root.clone(),
        oidc_rp,
        auth_pg,
        auth_db_url: db_url.to_string(),
        hydra_admin_url: hydra_admin_url.to_string(),
        app_base_domain: app_base_domain.to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies().expect("authz policies"),
        pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
    })
}

/// Drives the PRODUCTION entry points end-to-end: `provision_app_oauth_client`
/// (create path) → assert the derived apex host matches the gateway's subdomain
/// scheme (`{name}.{app_base_domain}`) → `delete_app_oauth_client` removes the
/// Hydra client. Regression for the two MAJOR findings:
///   - the delete path now issues the Hydra DELETE (no orphaned client), and
///   - the apex-host derivation in `provision_app_oauth_client` is the one the
///     gateway resolves, not just the lower-level `ensure_app_client`.
#[compio::test]
async fn appstate_provision_then_delete_end_to_end() {
    let (Some(url), Ok(hydra_url)) = (db_url(), std::env::var("HYDRA_ADMIN_URL")) else {
        eprintln!(
            "[app_oauth_client_test] CONTROL_TEST_DB/PG_TEST_URL or HYDRA_ADMIN_URL not set - skipping"
        );
        return;
    };

    let app_base_domain = "zeroship.localhost";
    let state = build_state(&url, &hydra_url, app_base_domain).await;
    let hydra = HydraAdmin::new(hydra_url.clone());

    let app_name = format!("zs-1d-state-{}", Uuid::new_v4().simple());
    let app = state
        .registry
        .create_app(&app_name, "free")
        .await
        .expect("create app");
    let app_id = app.id;
    let expected_client_id = client_id_for_app(&app_id);
    // The host the gateway's subdomain extractor resolves for this app.
    let expected_apex = format!("{app_name}.{app_base_domain}");

    // Clean any residue, then provision through the PRODUCTION wrapper.
    let _ = hydra.delete_client(&expected_client_id).await;
    let client_id = state
        .provision_app_oauth_client(&app_id, &app_name)
        .await
        .expect("provision via AppState");
    assert_eq!(client_id, expected_client_id);

    // The derived apex host (sector + redirect_uris) matches the gateway host.
    let hydra_client = hydra
        .get_client(&client_id)
        .await
        .expect("get_client")
        .expect("client exists");
    assert!(
        hydra_client
            .redirect_uris
            .iter()
            .any(|u| u == &format!("http://{expected_apex}/__zs/auth/callback")),
        "apex host derived by provision_app_oauth_client must match the gateway host {expected_apex}: {:?}",
        hydra_client.redirect_uris
    );
    let ext = state
        .registry
        .get_routes()
        .await
        .expect("get_routes");
    let entry = ext.get(&app_id).expect("route entry");
    assert_eq!(
        entry.sector_identifier.as_deref(),
        Some(format!("http://{expected_apex}").as_str())
    );

    // PRODUCTION delete path: AppState::delete_app_oauth_client must remove the
    // Hydra client (the M1 orphan-leak regression).
    state
        .delete_app_oauth_client(&app_id)
        .await
        .expect("delete via AppState");
    assert!(
        hydra.get_client(&client_id).await.expect("get").is_none(),
        "AppState::delete_app_oauth_client left the Hydra client live (orphan leak)"
    );

    // Idempotent: a second delete (already-gone client → Hydra 404) is a no-op.
    state
        .delete_app_oauth_client(&app_id)
        .await
        .expect("second delete is a no-op");

    state.registry.delete_app(&app_id).await.expect("delete app");
}
