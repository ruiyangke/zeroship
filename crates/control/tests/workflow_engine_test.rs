//! Live-PG tests for the DW-04 workflow engine scheduler.
//!
//! Requires `CONTROL_TEST_DB` pointing at a migrated disposable database. Tests
//! skip when it is unset, matching the rest of the control integration suite.

#![allow(clippy::await_holding_lock, clippy::future_not_send)]

mod common;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use compio_postgres::{connect, NoTls};
use futures::channel::oneshot;
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::workflow_engine::{
    self, RunUpdate, StepCheckpoint, StepDispatcher, StepRequest, StepResult,
    WorkflowEngineConfig,
};
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

static TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_tests() -> std::sync::MutexGuard<'static, ()> {
    TEST_GATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-wf-engine-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    pg: Arc<compio_postgres::Client>,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

async fn build_fixture(db_url: &str, label: &str) -> Fixture {
    let blob_root = tmpdir(&format!("blob-{label}"));
    let deploy_tmp_dir = tmpdir(&format!("deploy-{label}"));
    let registry = Registry::new(db_url).await.expect("registry");
    common::ensure_builtin_plans(&registry).await;
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let control_pg = Arc::new(pg(db_url).await);
    ensure_engine_columns(&control_pg).await;

    Fixture {
        state: Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "http://127.0.0.1:9".to_string(),
            worker_urls: Vec::new(),
            worker_key: SecretString::new(String::new()),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            insecure_dev: false,
            trust_proxy: false,
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::clone(&control_pg),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
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
        }),
        pg: control_pg,
        blob_root,
        deploy_tmp_dir,
    }
}

async fn ensure_engine_columns(pg: &compio_postgres::Client) {
    // The source migration is intentionally off-limits for DW-04. These columns
    // are the operator-steered scheduler contract; adding them here adapts only
    // the disposable test DB used by this integration binary.
    pg.batch_execute(
        "ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS claim_heartbeat_at timestamptz; \
         ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS claim_ttl_ms bigint; \
         ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS dispatch_nonce text; \
         ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS waiting_step_key text; \
         CREATE INDEX IF NOT EXISTS workflow_runs_dw04_due_idx \
           ON zeroship.workflow_runs (app_id, wake_at, id) \
           WHERE wake_at IS NOT NULL \
             AND state IN ('queued','running','sleeping','waiting');",
    )
    .await
    .expect("ensure DW-04 test columns");
}

async fn seed_app_and_deploy(fx: &Fixture, label: &str) -> (Uuid, String) {
    let app_id = Uuid::new_v4();
    let name = format!("wf-{label}-{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash')",
            &[
                &app_id,
                &name,
                &zeroship_control::bootstrap_console::free_plan_id(),
            ],
        )
        .await
        .expect("insert app");
    let deploy_id = format!("dep_{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.app_deploys (id, app_id, deploy_hash, manifest_json, activated_at) \
             VALUES ($1, $2, $3, '{}', now())",
            &[&deploy_id, &app_id, &format!("hash-{deploy_id}")],
        )
        .await
        .expect("insert deploy");
    (app_id, deploy_id)
}

#[allow(clippy::too_many_arguments)]
async fn seed_run(
    fx: &Fixture,
    app_id: Uuid,
    deploy_id: &str,
    state: &str,
    wake_delta_ms: i64,
    waiting_step_key: Option<&str>,
    claimed_by: Option<&str>,
    heartbeat_delta_ms: Option<i64>,
    dispatch_nonce: Option<&str>,
) -> String {
    let run_id = zeroship_core::typed_id::new_workflow_run_id();
    let wake_at = Utc::now() + ChronoDuration::milliseconds(wake_delta_ms);
    let heartbeat = heartbeat_delta_ms.map(|ms| Utc::now() + ChronoDuration::milliseconds(ms));
    let input = serde_json::json!({});
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, wake_at, \
                 claimed_by, claim_heartbeat_at, claim_ttl_ms, dispatch_nonce, \
                 waiting_step_key, started_at) \
             VALUES ($1, 'TestWorkflow', $2, $3, $4, $5, $6, \
                     $7, $8, 1000, $9, $10, now())",
            &[
                &run_id,
                &app_id,
                &deploy_id,
                &state,
                &input,
                &wake_at,
                &claimed_by,
                &heartbeat,
                &dispatch_nonce,
                &waiting_step_key,
            ],
        )
        .await
        .expect("insert workflow run");
    run_id
}

fn config(owner: &str) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        batch_apps: 16,
        per_app_fair_limit: 8,
        max_inflight_per_app: 64,
        max_inflight_dispatch: 64,
        claim_ttl_ms: 1_000,
        heartbeat_ms: 60_000,
        owner_id: owner.to_string(),
    }
}

#[derive(Clone, Default)]
struct BlockingDispatcher {
    requests: Arc<Mutex<Vec<StepRequest>>>,
    releases: Arc<Mutex<VecDeque<oneshot::Receiver<()>>>>,
}

impl BlockingDispatcher {
    fn with_capacity(n: usize) -> (Self, Vec<oneshot::Sender<()>>) {
        let mut receivers = VecDeque::new();
        let mut senders = Vec::new();
        for _ in 0..n {
            let (tx, rx) = oneshot::channel();
            senders.push(tx);
            receivers.push_back(rx);
        }
        (
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                releases: Arc::new(Mutex::new(receivers)),
            },
            senders,
        )
    }

    fn requests(&self) -> Vec<StepRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for BlockingDispatcher {
    async fn dispatch(&self, request: StepRequest) -> StepResult {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let release = self
            .releases
            .lock()
            .expect("releases lock")
            .pop_front()
            .expect("release receiver available");
        let _ = release.await;
        StepResult {
            run_id: request.run_id,
            dispatch_nonce: request.dispatch_nonce,
            checkpoints: Vec::new(),
            run_update: RunUpdate::Completed {
                output: Some(serde_json::json!({"released": true})),
            },
        }
    }
}

#[derive(Debug, Default)]
struct CompleteDispatcher;

#[async_trait(?Send)]
impl StepDispatcher for CompleteDispatcher {
    async fn dispatch(&self, request: StepRequest) -> StepResult {
        StepResult {
            run_id: request.run_id,
            dispatch_nonce: request.dispatch_nonce,
            checkpoints: vec![StepCheckpoint::completed_run(
                0,
                "done",
                serde_json::json!({"ok": true}),
            )],
            run_update: RunUpdate::Completed {
                output: Some(serde_json::json!({"ok": true})),
            },
        }
    }
}

async fn wait_for_requests(dispatcher: &BlockingDispatcher, n: usize) {
    for _ in 0..100 {
        if dispatcher.requests().len() >= n {
            return;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "timed out waiting for {n} dispatch requests, got {}",
        dispatcher.requests().len()
    );
}

async fn wait_for_completed(fx: &Fixture, run_ids: &[String]) {
    for _ in 0..100 {
        let rows = fx
            .pg
            .query(
                "SELECT COUNT(*)::bigint AS n \
                   FROM zeroship.workflow_runs \
                  WHERE id = ANY($1) AND state = 'completed'",
                &[&run_ids],
            )
            .await
            .expect("count completed runs");
        if rows[0].get::<_, i64>("n") == i64::try_from(run_ids.len()).unwrap() {
            return;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }

    let rows = fx
        .pg
        .query(
            "SELECT id, state, claimed_by FROM zeroship.workflow_runs WHERE id = ANY($1) ORDER BY id",
            &[&run_ids],
        )
        .await
        .expect("list incomplete runs");
    let states: Vec<(String, String, Option<String>)> = rows
        .into_iter()
        .map(|r| (r.get("id"), r.get("state"), r.get("claimed_by")))
        .collect();
    panic!("blocked dispatches did not complete after release: {states:?}");
}

#[compio::test]
async fn tick_claims_due_run_and_sets_owner_and_nonce() {
    let _gate = lock_tests();
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "claim").await;
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "claim").await;
    let run_id = seed_run(
        &fx, app_id, &deploy_id, "queued", -1_000, None, None, None, None,
    )
    .await;
    let (dispatcher, releases) = BlockingDispatcher::with_capacity(1);

    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(dispatcher.clone()),
        config("owner-claim"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_requests(&dispatcher, 1).await;

    let rows = fx
        .pg
        .query(
            "SELECT claimed_by, dispatch_nonce, state FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("select run");
    let claimed_by: Option<String> = rows[0].get("claimed_by");
    let nonce: Option<String> = rows[0].get("dispatch_nonce");
    let state: String = rows[0].get("state");
    assert_eq!(claimed_by.as_deref(), Some("owner-claim"));
    assert!(nonce.as_deref().is_some_and(|n| n.starts_with("wfd_")));
    assert_eq!(state, "running");

    for release in releases {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &[run_id]).await;
}

#[compio::test]
async fn concurrent_ticks_claim_disjoint_rows() {
    let _gate = lock_tests();
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "concurrent").await;
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "concurrent").await;
    for _ in 0..8 {
        seed_run(
            &fx, app_id, &deploy_id, "queued", -1_000, None, None, None, None,
        )
        .await;
    }
    let (d1, r1) = BlockingDispatcher::with_capacity(4);
    let (d2, r2) = BlockingDispatcher::with_capacity(4);
    let mut c1 = config("owner-concurrent-a");
    c1.per_app_fair_limit = 4;
    c1.max_inflight_per_app = 8;
    let mut c2 = config("owner-concurrent-b");
    c2.per_app_fair_limit = 4;
    c2.max_inflight_per_app = 8;

    let (a, b) = futures::join!(
        workflow_engine::tick_with_dispatcher(&fx.state, Arc::new(d1.clone()), c1),
        workflow_engine::tick_with_dispatcher(&fx.state, Arc::new(d2.clone()), c2),
    );
    assert_eq!(a.expect("tick a"), 4);
    assert_eq!(b.expect("tick b"), 4);
    wait_for_requests(&d1, 4).await;
    wait_for_requests(&d2, 4).await;

    let s1: std::collections::BTreeSet<_> =
        d1.requests().into_iter().map(|r| r.run_id).collect();
    let s2: std::collections::BTreeSet<_> =
        d2.requests().into_iter().map(|r| r.run_id).collect();
    assert_eq!(s1.len(), 4);
    assert_eq!(s2.len(), 4);
    assert!(s1.is_disjoint(&s2), "SKIP LOCKED claim sets overlap");

    let claimed_run_ids: Vec<String> = s1.into_iter().chain(s2).collect();
    for release in r1.into_iter().chain(r2) {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &claimed_run_ids).await;
}

#[compio::test]
async fn stale_lease_is_taken_over_after_ttl() {
    let _gate = lock_tests();
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "stale").await;
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "stale").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "running",
        -1_000,
        None,
        Some("dead-owner"),
        Some(-10_000),
        Some("wfd_dead"),
    )
    .await;
    let (dispatcher, releases) = BlockingDispatcher::with_capacity(1);

    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(dispatcher.clone()),
        config("owner-stale"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_requests(&dispatcher, 1).await;

    let row = fx
        .pg
        .query(
            "SELECT claimed_by, dispatch_nonce FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("select run");
    let claimed_by: Option<String> = row[0].get("claimed_by");
    let nonce: Option<String> = row[0].get("dispatch_nonce");
    assert_eq!(claimed_by.as_deref(), Some("owner-stale"));
    assert_ne!(nonce.as_deref(), Some("wfd_dead"));

    for release in releases {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &[run_id]).await;
}

#[compio::test]
async fn sleep_suspension_resolves_into_journal_row_at_wake() {
    let _gate = lock_tests();
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "sleep").await;
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "sleep").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "sleeping",
        -1_000,
        Some("sleep:0:cooldown"),
        None,
        None,
        None,
    )
    .await;
    let (dispatcher, releases) = BlockingDispatcher::with_capacity(1);

    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(dispatcher.clone()),
        config("owner-sleep"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_requests(&dispatcher, 1).await;

    let rows = fx
        .pg
        .query(
            "SELECT kind, state FROM zeroship.workflow_steps WHERE run_id = $1 AND ordinal = 0",
            &[&run_id],
        )
        .await
        .expect("select step");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>("kind"), "sleep");
    assert_eq!(rows[0].get::<_, String>("state"), "completed");

    for release in releases {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &[run_id]).await;
}

#[compio::test]
async fn apply_outcome_checkpoints_idempotently() {
    let _gate = lock_tests();
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "apply").await;
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "apply").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "running",
        -1_000,
        None,
        Some("owner-apply"),
        Some(0),
        Some("wfd_apply"),
    )
    .await;
    let result = StepResult {
        run_id: run_id.clone(),
        dispatch_nonce: "wfd_apply".to_string(),
        checkpoints: vec![StepCheckpoint::completed_run(
            0,
            "charge",
            serde_json::json!({"charged": true}),
        )],
        run_update: RunUpdate::Completed {
            output: Some(serde_json::json!({"charged": true})),
        },
    };

    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-apply", result.clone())
            .await
            .expect("apply first")
    );
    // Recreate the same lease to drive the same StepResult through the
    // insert path a second time; ON CONFLICT keeps the checkpoint singular.
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state='running', claimed_by=$1, dispatch_nonce=$2, claim_heartbeat_at=now() \
              WHERE id=$3",
            &[&"owner-apply", &"wfd_apply", &run_id],
        )
        .await
        .expect("reclaim for idempotent reapply");
    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-apply", result)
            .await
            .expect("apply second")
    );

    let rows = fx
        .pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1 AND ordinal = 0",
            &[&run_id],
        )
        .await
        .expect("count steps");
    assert_eq!(rows[0].get::<_, i64>("n"), 1);
}

#[compio::test]
async fn per_app_cap_does_not_livelock_queued_runs() {
    let _gate = lock_tests();
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_fixture(&url, "cap").await;
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "cap").await;
    let mut run_ids = Vec::new();
    for _ in 0..6 {
        run_ids.push(
            seed_run(
                &fx, app_id, &deploy_id, "queued", -1_000, None, None, None, None,
            )
            .await,
        );
    }
    let mut cfg = config("owner-cap");
    cfg.per_app_fair_limit = 6;
    cfg.max_inflight_per_app = 2;
    cfg.max_inflight_dispatch = 2;
    let dispatcher = Arc::new(CompleteDispatcher);

    for _ in 0..20 {
        workflow_engine::tick_with_dispatcher(&fx.state, Arc::clone(&dispatcher), cfg.clone())
            .await
            .expect("tick");
        compio::time::sleep(Duration::from_millis(50)).await;
        let rows = fx
            .pg
            .query(
                "SELECT COUNT(*)::bigint AS n \
                   FROM zeroship.workflow_runs \
                  WHERE id = ANY($1) AND state = 'completed'",
                &[&run_ids],
            )
            .await
            .expect("count completed");
        if rows[0].get::<_, i64>("n") == i64::try_from(run_ids.len()).unwrap() {
            return;
        }
    }
    let rows = fx
        .pg
        .query(
            "SELECT id, state, claimed_by FROM zeroship.workflow_runs WHERE id = ANY($1) ORDER BY id",
            &[&run_ids],
        )
        .await
        .expect("list runs");
    let states: Vec<(String, String, Option<String>)> = rows
        .into_iter()
        .map(|r| (r.get("id"), r.get("state"), r.get("claimed_by")))
        .collect();
    panic!("queued runs did not all complete under cap: {states:?}");
}
