//! Integration tests for ISS-12b: the control-side orphaned-app reaper that
//! archives apps left owner-less by the ISS-12 account-erase reaper (auth).
//!
//! Why control owns this: when auth hard-deletes a user, `app_members` cascade-
//! deletes the owner link, but `zeroship.apps` has no FK to users — so the app
//! row can keep serving. Auth has no call path to control, so lifecycle cleanup
//! lives here. Archive retains the app row, manifests, billing attribution, and
//! database state.
//!
//! These run the REAL path: a real `Registry` (PG-backed), a real `LocalFs`
//! VFS with a bundle written to disk, and the actual
//! `cron::orphaned_app_reaper` tick. No shims.
//!
//! All cases gate on a configured test database
//! (`common::require_control_db`); an absent or unmigrated one REFUSES
//! the run.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::orphaned_app_reaper;
use zeroship_control::{AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore};

use crate::common;

fn db_url() -> String {
    crate::common::require_control_db()
}

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

// These tests all exercise the production sweep against the same live test
// database. Serialize them so one test's sweep cannot archive another test's
// freshly inserted ownerless app before that test asserts its own report.
static REAPER_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Take the serialization token, ignoring poison.
///
/// The guard protects NO shared data - it is `Mutex<()>`, an ordering token -
/// so a previous test's panic leaves nothing inconsistent behind for the next
/// one to observe. `.expect(...)` on the poison therefore converted ONE real
/// failure into four, three of which reported `PoisonError { .. }` and said
/// nothing about their own subject. One fleet-count assertion failing here used
/// to take every other case in the module with it through the poisoned lock, and
/// none of them said which verdict it would have reached.
fn reaper_test_lock() -> std::sync::MutexGuard<'static, ()> {
    REAPER_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    zeroship_control::plan_catalog::seed_plans(&registry).await.expect("seed built-in plans");
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
    let control_pg = Arc::new(control_pg_client);

    let state = Arc::new(AppState {
        registry,
        env_store,
        stripe_store,
        blob_store,
            workflow_blob_store,
        control_key: SecretString::new("test-control-key".to_string()),
        master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
        stripe_webhook_secret: SecretString::new(String::new()),
        stripe_secret_key: SecretString::new(String::new()),
        stripe_base_url: "https://api.stripe.com".to_string(),
        gateway_url: "http://127.0.0.1:9".to_string(),
        worker_urls: Vec::new(),
        worker_key: SecretString::new(String::new()),
        admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
        origin_scheme: zeroship_core::config::OriginScheme::Https,
        trust_proxy: false,
        deploy_tmp_dir: deploy_tmp_dir.clone(),
        control_pg,
        // Unroutable: there are no worker calls on the DB-only delete path, so a refused
        // connection must NOT fail the purge. (Port 9 = discard.)
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
        mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
        pairwise_salt: [0u8; 32],
        projected_charge_cache: std::sync::Arc::new(
            zeroship_control::billing_read::ProjectedChargeCache::default(),
        ),
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
    // plan_id is an FK into zeroship.plans — use the built-in free-plan
    // catalog id (seeded by `seed_plans` in the test setup).
    let free = zeroship_control::plan_catalog::free_plan_id();
    // An OWNER-LESS project: this reaper's whole subject is an app whose
    // organization has no owner, so the fixture has to be able to build one.
    let project = common::unowned_project(&state.control_pg).await;
    state
        .control_pg
        .execute(
            &format!(
                "INSERT INTO zeroship.apps \
                     (id, name, plan_id, system, created_at, project_id, organization_id) \
                 SELECT $1, $2, $4, $3, NOW() - INTERVAL '{created_age}', p.id, \
                        p.organization_id \
                   FROM zeroship.projects p WHERE p.id = $5"
            ),
            &[id, &name, &system, &free, &project],
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

async fn app_is_archived(state: &AppState, id: &Uuid) -> bool {
    state
        .control_pg
        .query(
            "SELECT archived_at IS NOT NULL AS archived FROM zeroship.apps WHERE id = $1",
            &[id],
        )
        .await
        .expect("query app archive state")
        .first()
        .is_some_and(|row| row.get("archived"))
}

/// The instant this app was archived, or `None` while it is live.
///
/// Read so idempotence can be asserted about THIS APP rather than about the
/// reaper's fleet-wide count. See
/// `reaper_archives_ownerless_app_and_retains_its_bundle`.
async fn app_archived_at(
    state: &AppState,
    id: &Uuid,
) -> Option<chrono::DateTime<chrono::Utc>> {
    state
        .control_pg
        .query("SELECT archived_at FROM zeroship.apps WHERE id = $1", &[id])
        .await
        .expect("query app archived_at")
        .first()
        .and_then(|row| row.get("archived_at"))
}

async fn cleanup_app_row(state: &AppState, id: &Uuid) {
    state
        .control_pg
        .execute("DELETE FROM zeroship.apps WHERE id = $1", &[id])
        .await
        .expect("test cleanup app");
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
// (a) owner-less app + a bundle in the VFS -> archive row, retain bundle
// ---------------------------------------------------------------------------
// REAPER_TEST_LOCK guards `Mutex<()>` - a pure test-serialization token,
// not shared mutable data accessed across the await. compio::test runs
// each test on its own single-threaded runtime, so the held guard cannot
// deadlock another task's poll the way it could under a work-stealing
// executor.
#[allow(clippy::await_holding_lock)]
#[compio::test]
async fn reaper_archives_ownerless_app_and_retains_its_bundle() {
    let url = db_url();
    let _guard = reaper_test_lock();
    let fx = build_state(&url, "ownerless").await;
    let state = &fx.state;

    let app_id = Uuid::new_v4();
    let name = format!("orphan-{}", &app_id.simple().to_string()[..12]);
    insert_app(state, &app_id, &name, false, "10 minutes").await;

    // Write a manifest for this app so archive retention is observable.
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
        report.archived >= 1,
        "reaper should report at least the one orphan archived, got {}",
        report.archived
    );

    assert!(
        app_exists(state, &app_id).await && app_is_archived(state, &app_id).await,
        "owner-less app row must be retained and archived"
    );
    assert!(
        state.blob_store.get_manifest(&app_id, "deployone").await.is_ok(),
        "archive must retain the app's manifests"
    );
    // IDEMPOTENCE IS ABOUT THIS APP, NOT ABOUT THE REPORT'S COUNT.
    //
    // `tick` sweeps the whole fleet: every non-system, unarchived, owner-less
    // app past the five-minute grace, archived one at a time in a loop. This
    // used to assert `retry.archived == 0`, which is a claim that NOTHING in the
    // fleet became reapable while the first tick ran - and on a database that
    // has been used, the first tick has dozens of apps to walk and takes long
    // enough that apps created a few minutes earlier cross the grace boundary
    // during it. A fleet count is not this test's subject and it is not stable:
    // on a database that already holds same-aged orphans from an earlier run,
    // some cross the grace boundary mid-tick, so the second run against one
    // database saw a different count than the first and cascaded into this
    // module's other cases through the poisoned `REAPER_TEST_LOCK`. Orphans
    // crossing grace mid-tick is the reaper WORKING; nothing about that number
    // says the orphan below was touched twice.
    //
    // So the claim is made about the app the test owns, and about SELECTION
    // rather than about the count: the detection query must no longer return
    // this id. That is the property `retry.archived == 0` was reaching for -
    // `archived_at IS NULL` in `find_orphaned_apps` is what makes it true - and
    // it is the half the count cannot express on its own.
    //
    // The count is not a substitute here even in principle: `archive_app`
    // writes `archived_at = COALESCE(archived_at, NOW())`, so re-archiving an
    // already-archived app moves NOTHING observable on the row and still
    // returns `Ok(Some(_))`, incrementing `archived`. Selection is the only
    // place the defect would show.
    let archived_at = app_archived_at(state, &app_id).await;
    assert!(archived_at.is_some(), "precondition: the orphan is archived");
    let still_selected = orphaned_app_reaper::find_orphaned_apps(state)
        .await
        .expect("re-run the detection query");
    assert!(
        !still_selected.contains(&app_id),
        "an archived orphan is selected again: {app_id} is still in the reap \
         candidate set ({} candidate(s) total)",
        still_selected.len()
    );
    let retry = orphaned_app_reaper::tick(state).await.expect("retry reaper tick");
    assert_eq!(
        app_archived_at(state, &app_id).await,
        archived_at,
        "an archived orphan was archived AGAIN by the next tick (the retry \
         reported found={} archived={})",
        retry.found,
        retry.archived
    );

    cleanup_app_row(state, &app_id).await;

    // Teardown: the fixture holds the only handle to this test's Postgres
    // connection, and locals are dropped only after the body returns - by which
    // point the runtime is gone and the socket can no longer be closed. Drop it
    // explicitly, then wait for the close to land.
    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// (b) app WITH an owner member → untouched
// ---------------------------------------------------------------------------
// See the allow on `reaper_archives_ownerless_app_and_retains_its_bundle` above.
#[allow(clippy::await_holding_lock)]
#[compio::test]
async fn reaper_leaves_owned_app_untouched() {
    let url = db_url();
    let _guard = reaper_test_lock();
    let fx = build_state(&url, "owned").await;
    let state = &fx.state;

    let owner = seed_owner_user(state).await;
    let name = format!("owned-{}", &Uuid::new_v4().simple().to_string()[..12]);
    // create_app seeds the owner membership atomically.
    let app = state
        .registry
        .create_app(&name, &zeroship_control::plan_catalog::free_plan_id(), &owner, None)
        .await
        .expect("create_app")
        .id;

    orphaned_app_reaper::tick(state).await.expect("reaper tick");

    assert!(
        app_exists(state, &app).await,
        "an app WITH an owner member must never be reaped"
    );

    // cleanup
    cleanup_app_row(state, &app).await;
    let _ = state
        .control_pg
        .execute("DELETE FROM zeroship.users WHERE id = $1", &[&owner])
        .await;

    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// (c) THE CONSOLE-SAFETY TEST: system = true, owner-less → NEVER reaped
// ---------------------------------------------------------------------------
// See the allow on `reaper_archives_ownerless_app_and_retains_its_bundle` above.
#[allow(clippy::await_holding_lock)]
#[compio::test]
async fn reaper_never_touches_system_app() {
    let url = db_url();
    let _guard = reaper_test_lock();
    let fx = build_state(&url, "system").await;
    let state = &fx.state;

    // A system-owned app can be owner-less BY CONSTRUCTION (no
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
    cleanup_app_row(state, &app_id).await;

    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// (d) owner-less app YOUNGER than the grace → not yet reaped
// ---------------------------------------------------------------------------
// See the allow on `reaper_archives_ownerless_app_and_retains_its_bundle` above.
#[allow(clippy::await_holding_lock)]
#[compio::test]
async fn reaper_respects_grace_window() {
    let url = db_url();
    let _guard = reaper_test_lock();
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
    cleanup_app_row(state, &app_id).await;

    drop(fx);
    common::drain_pg().await;
}

// ---------------------------------------------------------------------------
// (e) direct archive is the same retained-state transition used by the reaper
// ---------------------------------------------------------------------------
// See the allow on `reaper_archives_ownerless_app_and_retains_its_bundle` above.
#[allow(clippy::await_holding_lock)]
#[compio::test]
async fn direct_archive_retains_db_row_and_vfs_blob() {
    let url = db_url();
    let _guard = reaper_test_lock();
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

    state
        .registry
        .archive_app(&app_id)
        .await
        .expect("archive app")
        .expect("app exists");

    assert!(
        app_exists(state, &app_id).await && app_is_archived(state, &app_id).await,
        "archive retains the DB row"
    );
    assert!(
        state.blob_store.get_manifest(&app_id, "deployone").await.is_ok(),
        "archive retains the app's manifests"
    );

    cleanup_app_row(state, &app_id).await;

    drop(fx);
    common::drain_pg().await;
}
