//! Integration tests for ISS-12b: the control-side orphaned-app reaper that
//! cleans up apps left owner-less by the ISS-12 account-erase reaper (auth).
//!
//! Why control owns this: when auth hard-deletes a user, `app_members` cascade-
//! deletes the owner link, but `zeroship.apps` has no FK to users — so the app
//! row (and its bundle/blobs in object storage) is never torn down. Auth has no
//! call path to control and no access to the blob store, so cleanup lives here,
//! where apps + the blob VFS + the per-app Hydra client are owned.
//!
//! These run the REAL path: a real `Registry` (PG-backed), a real `LocalFs`
//! VFS with a bundle written to disk, and the actual `cron::orphaned_app_reaper`
//! tick + the shared `api::purge_app`. No shims.
//!
//! All cases gate on `CONTROL_TEST_DB` / `PG_TEST_URL`; silent skip in dev.

use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use zeroship_bundle::{BlobError, BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::orphaned_app_reaper;
use zeroship_control::{
    api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB")
        .or_else(|_| std::env::var("PG_TEST_URL"))
        .ok()
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-reaper-{label}-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&p).expect("mk tmpdir");
    p
}

struct Fixture {
    state: Arc<AppState>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn build_state(db_url: &str, label: &str) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("dtmp-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    zeroship_control::bootstrap_console::seed_plans(&registry).await.expect("seed built-in plans");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());

    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));

    let (control_pg_client, control_pg_conn) =
        compio_postgres::connect(db_url, compio_postgres::NoTls)
            .await
            .expect("control-pg connect");
    compio::runtime::spawn(async move {
        let _ = control_pg_conn.run().await;
    })
    .detach();
    let control_pg = Arc::new(control_pg_client);

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        insecure_dev: false,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        // Unroutable: the per-app Hydra delete is best-effort, so a refused
        // connection must NOT fail the purge. (Port 9 = discard.)
        hydra_admin_url: "http://127.0.0.1:9".to_string(),
        app_base_domain: "zeroship.localhost".to_string(),
        trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
        expected_oauth_audience: "control.zeroship.ai".to_string(),
        static_policies: zeroship_authz::load_platform_policies()
            .expect("bundled authz policies parse"),
        pat_issuer: Arc::new(zeroship_control::token_handlers::PatIssuer::dev_insecure()),
        hydra_introspector: Arc::new(zeroship_core::hydra::HydraIntrospector::new(
            "http://127.0.0.1:9",
        )),
        logout_jti_cache: Arc::new(zeroship_core::logout_token::LogoutJtiCache::default()),
        metering_provider: zeroship_control::metering::provider::build_provider(
            &zeroship_control::metering::provider::MeteringProviderConfig::native(),
        )
        .expect("native provider builds"),
        pairwise_salt: [0u8; 32],
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

/// Insert an apps row directly (bypassing `create_app`, which would also seed an
/// owner member). `created_at` is back-dated so the row is past the reaper grace
/// unless the caller overrides. `system` defaults false.
async fn insert_app(
    state: &AppState,
    id: &Uuid,
    name: &str,
    system: bool,
    created_age: &str,
) {
    let api_key = format!("k-{}", id.simple());
    // PR4: plan_id is an FK into zeroship.plans — use the built-in free-plan
    // catalog id (seeded by `seed_plans` in the test setup).
    let free = zeroship_control::bootstrap_console::free_plan_id();
    state
        .control_pg
        .execute(
            &format!(
                "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash, system, created_at) \
                 VALUES ($1, $2, $5, $3, '', $4, NOW() - INTERVAL '{created_age}')"
            ),
            &[id, &name, &api_key, &system, &free],
        )
        .await
        .expect("insert app");
}

async fn app_exists(state: &AppState, id: &Uuid) -> bool {
    let rows = state
        .control_pg
        .query("SELECT 1 FROM zeroship.apps WHERE id = $1", &[id])
        .await
        .expect("query app");
    !rows.is_empty()
}

async fn seed_owner_user(state: &AppState) -> Uuid {
    let user_id = Uuid::new_v4();
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.users (id, email, name) VALUES ($1, $2::citext, $3)",
            &[
                &user_id,
                &format!("reaper-owner-{user_id}@zeroship.test"),
                &"reaper-owner",
            ],
        )
        .await
        .expect("seed owner user");
    user_id
}

// ---------------------------------------------------------------------------
// (a) owner-less app + a bundle in the VFS → reaper deletes BOTH
// ---------------------------------------------------------------------------
#[compio::test]
async fn reaper_deletes_ownerless_app_and_its_bundle() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "ownerless").await;
    let state = &fx.state;

    let app_id = Uuid::new_v4();
    let name = format!("orphan-{}", &app_id.simple().to_string()[..12]);
    insert_app(state, &app_id, &name, false, "10 minutes").await;

    // Write a manifest for this app so purge has an artifact to delete.
    state
        .blob_store
        .put_manifest(&app_id, "deployone", br#"{"v":1}"#)
        .await
        .expect("put manifest");
    assert!(
        state.blob_store.get_manifest(&app_id, "deployone").await.is_ok(),
        "precondition: manifest exists"
    );

    let report = orphaned_app_reaper::tick(state).await.expect("reaper tick");
    assert!(
        report.purged >= 1,
        "reaper should report at least the one orphan purged, got {}",
        report.purged
    );

    assert!(
        !app_exists(state, &app_id).await,
        "owner-less app row must be deleted"
    );
    assert!(
        matches!(
            state.blob_store.get_manifest(&app_id, "deployone").await,
            Err(BlobError::NotFound(_))
        ),
        "owner-less app manifests must be deleted from the blob store"
    );
}

// ---------------------------------------------------------------------------
// (b) app WITH an owner member → untouched
// ---------------------------------------------------------------------------
#[compio::test]
async fn reaper_leaves_owned_app_untouched() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "owned").await;
    let state = &fx.state;

    let owner = seed_owner_user(state).await;
    let name = format!("owned-{}", &Uuid::new_v4().simple().to_string()[..12]);
    // create_app seeds the owner membership atomically.
    let app = state
        .registry
        .create_app(&name, &zeroship_control::bootstrap_console::free_plan_id(), &owner)
        .await
        .expect("create_app")
        .id;

    orphaned_app_reaper::tick(state).await.expect("reaper tick");

    assert!(
        app_exists(state, &app).await,
        "an app WITH an owner member must never be reaped"
    );

    // cleanup
    let _ = state.registry.delete_app(&app).await;
    let _ = state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&owner])
        .await;
}

// ---------------------------------------------------------------------------
// (c) THE CONSOLE-SAFETY TEST: system = true, owner-less → NEVER reaped
// ---------------------------------------------------------------------------
#[compio::test]
async fn reaper_never_touches_system_app() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "system").await;
    let state = &fx.state;

    // The console is owner-less BY CONSTRUCTION (bootstrap_console seeds no
    // app_members row). Mirror that exactly: owner-less + system = true + old.
    let app_id = Uuid::new_v4();
    let name = format!("sys-console-{}", &app_id.simple().to_string()[..12]);
    insert_app(state, &app_id, &name, true, "1 year").await;

    orphaned_app_reaper::tick(state).await.expect("reaper tick");

    assert!(
        app_exists(state, &app_id).await,
        "a system (platform-owned) app with no owner MUST NOT be reaped — \
         this is the platform-console-safety guarantee"
    );

    // cleanup
    let _ = state.registry.delete_app(&app_id).await;
}

// ---------------------------------------------------------------------------
// (d) owner-less app YOUNGER than the grace → not yet reaped
// ---------------------------------------------------------------------------
#[compio::test]
async fn reaper_respects_grace_window() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "grace").await;
    let state = &fx.state;

    let app_id = Uuid::new_v4();
    let name = format!("fresh-{}", &app_id.simple().to_string()[..12]);
    // Just created (owner-less but younger than the 5-minute grace).
    insert_app(state, &app_id, &name, false, "30 seconds").await;

    orphaned_app_reaper::tick(state).await.expect("reaper tick");

    assert!(
        app_exists(state, &app_id).await,
        "an owner-less app younger than the grace window must not be reaped yet"
    );

    // cleanup
    let _ = state.registry.delete_app(&app_id).await;
}

// ---------------------------------------------------------------------------
// (e) shared-path test: api::purge_app removes BOTH the DB row and the VFS blob
// ---------------------------------------------------------------------------
#[compio::test]
async fn purge_app_removes_db_row_and_vfs_blob() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "purge").await;
    let state = &fx.state;

    let app_id = Uuid::new_v4();
    let name = format!("purge-{}", &app_id.simple().to_string()[..12]);
    insert_app(state, &app_id, &name, false, "1 minute").await;

    state
        .blob_store
        .put_manifest(&app_id, "deployone", br#"{"v":1}"#)
        .await
        .expect("put manifest");

    let deleted = api::purge_app(state, &app_id).await.expect("purge_app");
    assert!(deleted, "purge_app reports the DB row was deleted");

    assert!(
        !app_exists(state, &app_id).await,
        "purge_app must delete the DB row"
    );
    assert!(
        matches!(
            state.blob_store.get_manifest(&app_id, "deployone").await,
            Err(BlobError::NotFound(_))
        ),
        "purge_app must delete the app's manifests"
    );
}
