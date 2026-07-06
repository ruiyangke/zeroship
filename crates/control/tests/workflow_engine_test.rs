//! Live-PG tests for the DW-04 workflow engine scheduler.
//!
//! Requires `CONTROL_TEST_DB` pointing at a migrated disposable database. Each
//! test clones that migrated DB into its own throwaway database because the
//! engine claims due workflow runs globally across the connected database.
//! Tests skip when `CONTROL_TEST_DB` is unset, matching the rest of the control
//! integration suite.

#![allow(clippy::await_holding_lock, clippy::future_not_send)]

mod common;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use compio_postgres::{connect, NoTls};
use futures::channel::oneshot;
use ntex::web::{self, test};
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::workflow_engine::{
    self, DispatchOutcome, RunUpdate, StepCheckpoint, StepDispatcher, StepRequest, StepResult,
    WorkflowEngineConfig,
};
use zeroship_control::{
    workflow_instance_api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};

const TEST_CONTROL_KEY: &str = "test-control-key";
const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

static DB_CLONE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    _db: TestDatabase,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.blob_root);
        let _ = std::fs::remove_dir_all(&self.deploy_tmp_dir);
    }
}

struct TestDatabase {
    admin_url: String,
    name: String,
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        if self.name.is_empty() {
            return;
        }
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        let _ = std::thread::spawn(move || {
            let Ok(rt) = compio::runtime::Runtime::new() else {
                return;
            };
            rt.block_on(async move {
                let Ok(admin) = try_pg(&admin_url).await else {
                    return;
                };
                let _ = admin
                    .batch_execute(&format!(
                        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                        quote_ident(&name)
                    ))
                    .await;
            });
        })
        .join();
    }
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    try_pg(db_url).await.expect("pg connect")
}

async fn try_pg(
    db_url: &str,
) -> Result<compio_postgres::Client, compio_postgres::Error> {
    let (client, conn) = connect(db_url, NoTls).await?;
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    Ok(client)
}

async fn isolated_fixture(label: &str) -> Option<Fixture> {
    isolated_fixture_with_gateway(label, "http://127.0.0.1:9").await
}

async fn isolated_fixture_with_gateway(label: &str, gateway_url: &str) -> Option<Fixture> {
    let Some(base_url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return None;
    };
    Some(build_isolated_fixture_with_gateway(&base_url, label, gateway_url).await)
}

async fn build_isolated_fixture_with_gateway(
    base_url: &str,
    label: &str,
    gateway_url: &str,
) -> Fixture {
    let source_db = db_name_from_dsn(base_url)
        .unwrap_or_else(|| panic!("CONTROL_TEST_DB must include a database name: {base_url}"));
    assert_ne!(
        source_db, "postgres",
        "CONTROL_TEST_DB must point at a migrated disposable DB, not the postgres maintenance DB"
    );
    let db_name = fresh_test_db_name(label);
    let admin_url = dsn_for_db(base_url, "postgres");
    let isolated_url = dsn_for_db(base_url, &db_name);

    {
        let _clone_gate = DB_CLONE_GATE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let admin = pg(&admin_url).await;
        admin
            .batch_execute(&format!(
                "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                quote_ident(&db_name),
            ))
            .await
            .unwrap_or_else(|err| panic!("drop stale workflow engine test DB {db_name}: {err}"));
        admin
            .batch_execute(&format!(
                "CREATE DATABASE {} WITH TEMPLATE {}",
                quote_ident(&db_name),
                quote_ident(&source_db),
            ))
            .await
            .unwrap_or_else(|err| {
                panic!("create isolated workflow engine test DB {db_name} from {source_db}: {err}")
            });
    }

    let test_db = TestDatabase {
        admin_url,
        name: db_name,
    };
    let fx = build_fixture_with_gateway(&isolated_url, label, gateway_url, test_db).await;
    scrub_cloned_fixture_data(&fx.pg).await;
    fx
}

fn fresh_test_db_name(label: &str) -> String {
    let label = label
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let label = label.trim_matches('_');
    format!("zs_wf_engine_{}_{}", label, Uuid::new_v4().simple())
}

fn db_name_from_dsn(dsn: &str) -> Option<String> {
    let trimmed = dsn.trim_start();
    if trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://") {
        let base = trimmed.split_once('?').map_or(trimmed, |(base, _)| base);
        let scheme_end = base.find("://").map_or(0, |idx| idx + 3);
        let path_start = base[scheme_end..].find('/')? + scheme_end;
        let db = &base[path_start + 1..];
        return (!db.is_empty()).then(|| db.to_string());
    }

    dsn.split_whitespace().find_map(|tok| {
        let (key, value) = tok.split_once('=')?;
        key.eq_ignore_ascii_case("dbname")
            .then(|| value.trim_matches('\'').trim_matches('"').to_string())
    })
}

fn dsn_for_db(dsn: &str, db: &str) -> String {
    let trimmed = dsn.trim_start();
    if trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://") {
        let (base, query) = trimmed
            .split_once('?')
            .map_or((trimmed, None), |(base, query)| (base, Some(query)));
        let scheme_end = base.find("://").map_or(0, |idx| idx + 3);
        let new_base = base[scheme_end..].find('/').map_or_else(
            || format!("{base}/{db}"),
            |rel| {
                let path_start = scheme_end + rel;
                format!("{}/{}", &base[..path_start], db)
            },
        );
        return match query {
            Some(query) => format!("{new_base}?{query}"),
            None => new_base,
        };
    }

    let mut parts = Vec::new();
    for tok in dsn.split_whitespace() {
        if !tok
            .split_once('=')
            .is_some_and(|(key, _)| key.eq_ignore_ascii_case("dbname"))
        {
            parts.push(tok.to_string());
        }
    }
    parts.push(format!("dbname={db}"));
    parts.join(" ")
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

async fn scrub_cloned_fixture_data(pg: &compio_postgres::Client) {
    pg.batch_execute(
        "TRUNCATE TABLE \
             zeroship.workflow_steps, \
             zeroship.workflow_runs, \
             zeroship.workflow_signals, \
             zeroship.workflow_subscriptions, \
             zeroship.app_deploys, \
             zeroship.apps \
         CASCADE;",
    )
    .await
    .expect("scrub cloned workflow fixture data");
}

async fn build_fixture_with_gateway(
    db_url: &str,
    label: &str,
    gateway_url: &str,
    test_db: TestDatabase,
) -> Fixture {
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
            control_key: SecretString::new(TEST_CONTROL_KEY.to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "http://127.0.0.1:9".to_string(),
            gateway_url: gateway_url.to_string(),
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
        _db: test_db,
    }
}

async fn spend_blocked_gateway() -> web::HttpResponse {
    web::HttpResponse::PaymentRequired().json(&serde_json::json!({"code": "SPEND_LIMIT"}))
}

async fn ensure_engine_columns(pg: &compio_postgres::Client) {
    pg.batch_execute(
        "ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS dispatch_nonce text; \
         ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS waiting_step_key text; \
         ALTER TABLE zeroship.workflow_runs \
           ADD COLUMN IF NOT EXISTS paused_from_status text; \
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
             VALUES ($1, $2, $3, $4, now())",
            &[
                &deploy_id,
                &app_id,
                &format!("hash-{deploy_id}"),
                &serde_json::json!({"version":1,"workflows":["TestWorkflow"]}).to_string(),
            ],
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
    lease_delta_ms: Option<i64>,
    dispatch_nonce: Option<&str>,
) -> String {
    let run_id = zeroship_core::typed_id::new_workflow_run_id();
    let wake_at = Utc::now() + ChronoDuration::milliseconds(wake_delta_ms);
    let lease_expires = lease_delta_ms.map(|ms| Utc::now() + ChronoDuration::milliseconds(ms));
    let input = serde_json::json!({});
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, wake_at, \
                 claimed_by, lease_expires, dispatch_nonce, \
                 waiting_step_key, started_at) \
             VALUES ($1, 'TestWorkflow', $2, $3, $4, $5, $6, \
                     $7, $8, $9, $10, now())",
            &[
                &run_id,
                &app_id,
                &deploy_id,
                &state,
                &input,
                &wake_at,
                &claimed_by,
                &lease_expires,
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

fn authed(req: test::TestRequest, app_id: Uuid) -> test::TestRequest {
    let token =
        zeroship_core::auth::derive_app_scoped_control_token(TEST_CONTROL_KEY, &app_id.to_string());
    req.header("authorization", format!("Bearer {token}"))
        .header(workflow_instance_api::APP_ID_HEADER, app_id.to_string())
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
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
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
        DispatchOutcome::Completed(StepResult {
            run_id: request.run_id,
            dispatch_nonce: request.dispatch_nonce,
            checkpoints: Vec::new(),
            run_update: RunUpdate::Completed {
                output: Some(serde_json::json!({"released": true})),
            },
        })
    }
}

#[derive(Debug, Default)]
struct CompleteDispatcher;

#[async_trait(?Send)]
impl StepDispatcher for CompleteDispatcher {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        DispatchOutcome::Completed(StepResult {
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
        })
    }
}

#[derive(Clone)]
struct GatedCheckpointDispatcher {
    requests: Arc<Mutex<Vec<StepRequest>>>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    checkpoint: StepCheckpoint,
    run_update: RunUpdate,
}

impl GatedCheckpointDispatcher {
    fn new(
        checkpoint: StepCheckpoint,
        run_update: RunUpdate,
    ) -> (Self, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                release: Arc::new(Mutex::new(Some(rx))),
                checkpoint,
                run_update,
            },
            tx,
        )
    }

    fn requests(&self) -> Vec<StepRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for GatedCheckpointDispatcher {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let release = self
            .release
            .lock()
            .expect("release lock")
            .take()
            .expect("release receiver available");
        let _ = release.await;
        DispatchOutcome::Completed(StepResult {
            run_id: request.run_id,
            dispatch_nonce: request.dispatch_nonce,
            checkpoints: vec![self.checkpoint.clone()],
            run_update: self.run_update.clone(),
        })
    }
}

#[derive(Debug, Default)]
struct CompleteAfterA;

#[async_trait(?Send)]
impl StepDispatcher for CompleteAfterA {
    async fn dispatch(&self, request: StepRequest) -> DispatchOutcome {
        assert!(
            request
                .journal
                .iter()
                .any(|step| step.ordinal == 0 && step.name == "a" && step.state == "completed"),
            "resume dispatch should replay the landed a checkpoint: {:?}",
            request.journal
        );
        DispatchOutcome::Completed(StepResult {
            run_id: request.run_id,
            dispatch_nonce: request.dispatch_nonce,
            checkpoints: vec![StepCheckpoint::completed_run(
                1,
                "b",
                serde_json::json!({"ok": true}),
            )],
            run_update: RunUpdate::Completed {
                output: Some(serde_json::json!({"ok": true})),
            },
        })
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

async fn wait_for_gated_requests(dispatcher: &GatedCheckpointDispatcher, n: usize) {
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
    let Some(fx) = isolated_fixture("claim").await else {
        return;
    };
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
    let Some(fx) = isolated_fixture("concurrent").await else {
        return;
    };
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
    let Some(fx) = isolated_fixture("stale").await else {
        return;
    };
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
    let Some(fx) = isolated_fixture("sleep").await else {
        return;
    };
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
    let Some(fx) = isolated_fixture("apply").await else {
        return;
    };
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
                SET state='running', claimed_by=$1, dispatch_nonce=$2, lease_expires=$3 \
              WHERE id=$4",
            &[
                &"owner-apply",
                &"wfd_apply",
                &(Utc::now() + ChronoDuration::seconds(60)),
                &run_id,
            ],
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
async fn pause_resume_controls_cover_due_skip_and_restore_state() {
    let Some(fx) = isolated_fixture("pause-resume").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "pause-resume").await;
    let cases = [
        ("queued", None, -1_000),
        ("running", None, -1_000),
        ("sleeping", Some("sleep:0:cooldown"), 3_600_000),
        ("waiting", Some("wait:0:go:go:60000"), 3_600_000),
    ];
    let mut runs: Vec<(String, String, DateTime<Utc>)> = Vec::new();
    for (state, waiting_key, wake_delta_ms) in cases {
        let run_id = seed_run(
            &fx,
            app_id,
            &deploy_id,
            state,
            wake_delta_ms,
            waiting_key,
            None,
            None,
            None,
        )
        .await;
        let wake_at: DateTime<Utc> = fx
            .pg
            .query_one("SELECT wake_at FROM zeroship.workflow_runs WHERE id = $1", &[&run_id])
            .await
            .expect("load wake")
            .get("wake_at");
        runs.push((run_id, state.to_string(), wake_at));
    }

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    for (run_id, original_state, original_wake) in &runs {
        let resp = test::call_service(
            &app,
            authed(
                test::TestRequest::post()
                    .uri(&format!("/internal/workflows/runs/{run_id}/pause")),
                app_id,
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        let row = fx
            .pg
            .query_one(
                "SELECT state, wake_at, paused_from_status \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1",
                &[run_id],
            )
            .await
            .expect("load paused run");
        assert_eq!(row.get::<_, String>("state"), "paused");
        assert_eq!(
            row.get::<_, Option<String>>("paused_from_status").as_deref(),
            Some(original_state.as_str())
        );
        let paused_wake: DateTime<Utc> = row.get("wake_at");
        assert!(
            paused_wake
                .signed_duration_since(*original_wake)
                .num_milliseconds()
                .abs()
                <= 1,
            "pause should preserve wake_at for {original_state}"
        );
    }

    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(CompleteDispatcher),
        config("owner-paused-skip"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 0, "due paused rows must be skipped by the claim query");

    for (run_id, original_state, original_wake) in &runs {
        let resp = test::call_service(
            &app,
            authed(
                test::TestRequest::post()
                    .uri(&format!("/internal/workflows/runs/{run_id}/resume")),
                app_id,
            )
            .to_request(),
        )
        .await;
        assert_eq!(resp.status(), ntex::http::StatusCode::OK);
        let row = fx
            .pg
            .query_one(
                "SELECT state, wake_at, paused_from_status \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1",
                &[run_id],
            )
            .await
            .expect("load resumed run");
        assert_eq!(row.get::<_, String>("state"), original_state.as_str());
        assert_eq!(row.get::<_, Option<String>>("paused_from_status"), None);
        let resumed_wake: DateTime<Utc> = row.get("wake_at");
        assert!(
            resumed_wake
                .signed_duration_since(*original_wake)
                .num_milliseconds()
                .abs()
                <= 1,
            "resume should preserve wake_at for {original_state}"
        );
    }

    let due_runs: Vec<String> = runs
        .iter()
        .filter(|(_, state, _)| state == "queued" || state == "running")
        .map(|(run_id, _, _)| run_id.clone())
        .collect();
    workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(CompleteDispatcher),
        config("owner-resumed-complete"),
    )
    .await
    .expect("tick resumed");
    wait_for_completed(&fx, &due_runs).await;
}

#[compio::test]
async fn pause_mid_dispatch_lands_checkpoint_but_suppresses_requeue() {
    let Some(fx) = isolated_fixture("pause-mid").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "pause-mid").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "queued",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;
    let (dispatcher, release) = GatedCheckpointDispatcher::new(
        StepCheckpoint::completed_run(0, "a", serde_json::json!({"ok": true})),
        RunUpdate::Queued,
    );
    let dispatcher = Arc::new(dispatcher);
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-pause-mid"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_gated_requests(&dispatcher, 1).await;

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/pause")),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let row = fx
        .pg
        .query_one(
            "SELECT state, paused_from_status, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("paused running row");
    assert_eq!(row.get::<_, String>("state"), "paused");
    assert_eq!(
        row.get::<_, Option<String>>("paused_from_status").as_deref(),
        Some("running")
    );
    assert_eq!(row.get::<_, Option<String>>("claimed_by").as_deref(), Some("owner-pause-mid"));
    assert!(row.get::<_, Option<String>>("dispatch_nonce").is_some());

    let _ = release.send(());
    for _ in 0..100 {
        let row = fx
            .pg
            .query_one(
                "SELECT state, paused_from_status, claimed_by \
                   FROM zeroship.workflow_runs WHERE id = $1",
                &[&run_id],
            )
            .await
            .expect("post-apply paused row");
        let steps = fx
            .pg
            .query(
                "SELECT ordinal, name, state FROM zeroship.workflow_steps WHERE run_id = $1",
                &[&run_id],
            )
            .await
            .expect("steps");
        if steps.len() == 1 && row.get::<_, Option<String>>("claimed_by").is_none() {
            assert_eq!(row.get::<_, String>("state"), "paused");
            assert_eq!(
                row.get::<_, Option<String>>("paused_from_status").as_deref(),
                Some("queued")
            );
            break;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    let steps = fx
        .pg
        .query(
            "SELECT ordinal, name, state FROM zeroship.workflow_steps WHERE run_id = $1",
            &[&run_id],
        )
        .await
        .expect("landed steps");
    assert_eq!(steps.len(), 1, "paused apply must land exactly one checkpoint");
    assert_eq!(steps[0].get::<_, i32>("ordinal"), 0);
    assert_eq!(steps[0].get::<_, String>("name"), "a");

    let skipped = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(CompleteAfterA),
        config("owner-paused-after-apply"),
    )
    .await
    .expect("paused tick");
    assert_eq!(skipped, 0, "paused run must not be re-claimed after checkpoint");

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/resume")),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(CompleteAfterA),
        config("owner-paused-resume"),
    )
    .await
    .expect("resume tick");
    wait_for_completed(&fx, &[run_id.clone()]).await;
    let rows = fx
        .pg
        .query(
            "SELECT ordinal, name, state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("final steps");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, String>("name"), "a");
    assert_eq!(rows[1].get::<_, String>("name"), "b");
}

#[compio::test]
async fn cancel_mid_dispatch_discards_late_outcome() {
    let Some(fx) = isolated_fixture("cancel-mid").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "cancel-mid").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "queued",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;
    let (dispatcher, release) = GatedCheckpointDispatcher::new(
        StepCheckpoint::completed_run(0, "a", serde_json::json!({"ok": true})),
        RunUpdate::Queued,
    );
    let dispatcher = Arc::new(dispatcher);
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-cancel-mid"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_gated_requests(&dispatcher, 1).await;

    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/cancel")),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let _ = release.send(());
    compio::time::sleep(Duration::from_millis(100)).await;

    let row = fx
        .pg
        .query_one(
            "SELECT state, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("cancelled row");
    assert_eq!(row.get::<_, String>("state"), "cancelled");
    assert_eq!(row.get::<_, Option<DateTime<Utc>>>("wake_at"), None);
    assert_eq!(row.get::<_, Option<String>>("claimed_by"), None);
    assert_eq!(row.get::<_, Option<String>>("dispatch_nonce"), None);
    let steps = fx
        .pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
            &[&run_id],
        )
        .await
        .expect("count steps");
    assert_eq!(
        steps[0].get::<_, i64>("n"),
        0,
        "late outcome after cancel must be discarded"
    );
    let claimed = workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(CompleteDispatcher),
        config("owner-cancelled-skip"),
    )
    .await
    .expect("cancelled tick");
    assert_eq!(claimed, 0);
}

#[compio::test]
async fn restart_rewinds_prefix_requeues_and_guards_completed_compensation() {
    let Some(fx) = isolated_fixture("restart").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "restart").await;
    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    let guarded = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "completed",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = NULL, output = $2 WHERE id = $1",
            &[&guarded, &serde_json::json!({"ok": true})],
        )
        .await
        .expect("complete guarded run");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, output, output_kind, \
                 batch_id, batch_width, finished_at, compensation_state, compensation_finished_at) \
             VALUES \
                ($1, 0, 'a', 0, 'run', 'completed', $2, 'inline', 'wfd_seed', 1, now(), 'completed', now()), \
                ($1, 1, 'b', 0, 'run', 'completed', $3, 'inline', 'wfd_seed', 1, now(), NULL, NULL)",
            &[
                &guarded,
                &serde_json::json!({"a": true}),
                &serde_json::json!({"b": true}),
            ],
        )
        .await
        .expect("seed guarded steps");
    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{guarded}/restart"))
                .set_json(&serde_json::json!({"from": {"name": "b"}})),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        ntex::http::StatusCode::CONFLICT,
        "partial restart past completed compensation must be rejected"
    );
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["error"], "RestartError");
    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{guarded}/restart"))
                .set_json(&serde_json::json!({})),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(
        resp.status(),
        ntex::http::StatusCode::OK,
        "full restart is allowed even after completed compensation"
    );
    let steps = fx
        .pg
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
            &[&guarded],
        )
        .await
        .expect("guarded steps after full restart");
    assert_eq!(steps[0].get::<_, i64>("n"), 0);

    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "completed",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = NULL, output = $2 WHERE id = $1",
            &[&run_id, &serde_json::json!({"done": true})],
        )
        .await
        .expect("complete run");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, output, output_kind, \
                 batch_id, batch_width, finished_at) \
             VALUES \
                ($1, 0, 'a', 0, 'run', 'completed', $2, 'inline', 'wfd_seed', 1, now()), \
                ($1, 1, 'b', 0, 'run', 'completed', $3, 'inline', 'wfd_seed', 1, now())",
            &[
                &run_id,
                &serde_json::json!({"a": true}),
                &serde_json::json!({"b": true}),
            ],
        )
        .await
        .expect("seed restart steps");
    let resp = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{run_id}/restart"))
                .set_json(&serde_json::json!({"from": {"name": "b"}})),
            app_id,
        )
        .to_request(),
    )
    .await;
    assert_eq!(resp.status(), ntex::http::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    assert_eq!(body["runId"], run_id);
    assert_eq!(body["state"], "queued");
    assert_eq!(body["restartedFromOrdinal"], 1);
    let rows = fx
        .pg
        .query(
            "SELECT state, wake_at, next_ordinal, restarted_from_ordinal \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("restart run row");
    assert_eq!(rows[0].get::<_, String>("state"), "queued");
    assert!(rows[0].get::<_, Option<DateTime<Utc>>>("wake_at").is_some());
    assert_eq!(rows[0].get::<_, i32>("next_ordinal"), 1);
    assert_eq!(rows[0].get::<_, Option<i32>>("restarted_from_ordinal"), Some(1));
    let rows = fx
        .pg
        .query(
            "SELECT ordinal, name \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("restart prefix");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, i32>("ordinal"), 0);
    assert_eq!(rows[0].get::<_, String>("name"), "a");

    workflow_engine::tick_with_dispatcher(
        &fx.state,
        Arc::new(CompleteAfterA),
        config("owner-restart-complete"),
    )
    .await
    .expect("restart tick");
    wait_for_completed(&fx, &[run_id.clone()]).await;
    let rows = fx
        .pg
        .query(
            "SELECT ordinal, name, state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("restart final steps");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].get::<_, String>("name"), "a");
    assert_eq!(rows[1].get::<_, String>("name"), "b");
}

#[compio::test]
async fn per_app_cap_does_not_livelock_queued_runs() {
    let Some(fx) = isolated_fixture("cap").await else {
        return;
    };
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

#[ntex::test]
async fn gateway_402_backpressure_parks_claim_without_step_attempt() {
    let gateway = test::server(|| async {
        web::App::new().service(
            web::resource("/__zeroship/internal/workflow-dispatch")
                .route(web::post().to(spend_blocked_gateway)),
        )
    })
    .await;
    let Some(fx) = isolated_fixture_with_gateway("gw-402", &gateway.url("")).await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "gw-402").await;
    let run_id = seed_run(
        &fx, app_id, &deploy_id, "queued", -1_000, None, None, None, None,
    )
    .await;

    let claimed = workflow_engine::tick(&fx.state).await.expect("tick");
    assert_eq!(claimed, 1);

    for _ in 0..100 {
        let rows = fx
            .pg
            .query(
                "SELECT state, claimed_by, dispatch_nonce \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1",
                &[&run_id],
            )
            .await
            .expect("load run");
        let state: String = rows[0].get("state");
        let claimed_by: Option<String> = rows[0].get("claimed_by");
        let dispatch_nonce: Option<String> = rows[0].get("dispatch_nonce");
        if state == "queued" && claimed_by.is_none() && dispatch_nonce.is_none() {
            let step_rows = fx
                .pg
                .query(
                    "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
                    &[&run_id],
                )
                .await
                .expect("count workflow steps");
            assert_eq!(
                step_rows[0].get::<_, i64>("n"),
                0,
                "backpressure must park without inserting an attempt/no-reply step"
            );
            return;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
    }

    let rows = fx
        .pg
        .query(
            "SELECT state, claimed_by, dispatch_nonce FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load final run state");
    panic!(
        "run did not park after gateway 402: state={:?} claimed_by={:?} dispatch_nonce={:?}",
        rows[0].get::<_, String>("state"),
        rows[0].get::<_, Option<String>>("claimed_by"),
        rows[0].get::<_, Option<String>>("dispatch_nonce")
    );
}
