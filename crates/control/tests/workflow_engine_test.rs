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
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use compio_postgres::{connect, NoTls};
use compio_postgres::types::ToSql;
use futures::channel::oneshot;
use ntex::web::{self, test};
use serial_test::serial;
use uuid::Uuid;
use zeroship_bundle::{sha256_hex, BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::{
    deploy_retention, workflow_blob_gc, workflow_retention, workflow_schedules,
    workflow_signal_fanout,
};
use zeroship_control::cron::workflow_engine::{
    self, DispatchOutcome, GatewayStepDispatcher, RunUpdate, StepCheckpoint, StepDispatcher,
    StepRequest, StepResult, WorkflowEngineConfig,
};
use zeroship_control::{
    workflow_instance_api, AppState, EnvStore, Quota, RateLimiter, Registry, SecretString,
    StripeStore,
};
use zeroship_plugin_workflow::advance::{
    collect_post_apply_registrations_on_conn, WorkflowAdvanceNackKind,
    WorkflowAdvanceRegistration, WorkflowAdvanceResponse, WorkflowRunDispatchRequest,
};
use zeroship_plugin_workflow::claim::{claim_workflow_run_on_conn, WorkflowClaimOutcome};
use zeroship_plugin_workflow::engine::STUCK_STRIKE_LIMIT_FIELD;
use zeroship_plugin_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_workflow_scheduler::{
    self as scheduler_store_engine, SchedulerConfig as StoreSchedulerConfig, TimerWheel,
    WakeHandle, WorkflowSchedulerStore,
};

const TEST_CONTROL_KEY: &str = "test-control-key";
const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const TEST_WORKER_OWNER: &str = "test-worker-owner";

static DB_CLONE_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
static TIMING_TEST_GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

fn local_workflow_blob_path(root: &std::path::Path, hash: &str) -> PathBuf {
    let (shard, rest) = hash.split_at(2);
    root.join("wfblob").join(shard).join(rest)
}

fn set_local_workflow_blob_mtime(root: &std::path::Path, hash: &str, modified: SystemTime) {
    let path = local_workflow_blob_path(root, hash);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_or_else(|err| panic!("open workflow blob {path:?}: {err}"));
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap_or_else(|err| panic!("set workflow blob mtime {path:?}: {err}"));
}

struct Fixture {
    state: Arc<AppState>,
    pg: TestPg,
    db_url: String,
    blob_root: PathBuf,
    deploy_tmp_dir: PathBuf,
    scheduler_store: WorkflowSchedulerStore,
    _db: TestDatabase,
}

#[derive(Clone)]
struct TestPg {
    inner: Arc<compio_postgres::Client>,
    default_app_id: Arc<Mutex<Option<Uuid>>>,
}

impl TestPg {
    fn new(inner: Arc<compio_postgres::Client>) -> Self {
        Self {
            inner,
            default_app_id: Arc::new(Mutex::new(None)),
        }
    }

    fn set_default_app_id(&self, app_id: Uuid) {
        *self
            .default_app_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(app_id);
    }

    fn workflow_sql_for_app(app_id: Uuid, sql: &str) -> String {
        let tables = WorkflowTables::for_app_id(&app_id);
        sql.replace("zeroship.workflow_runs", &tables.runs)
            .replace("zeroship.workflow_steps", &tables.steps)
            .replace("zeroship.workflow_signals", &tables.signals)
            .replace(
                "zeroship.workflow_subscriptions",
                &tables.subscriptions,
            )
            .replace("zeroship.workflow_blobs", &tables.blobs)
    }

    fn rewrite(&self, sql: &str) -> String {
        let app_id = *self
            .default_app_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        app_id.map_or_else(|| sql.to_string(), |app_id| Self::workflow_sql_for_app(app_id, sql))
    }

    async fn batch_execute(&self, sql: &str) -> Result<(), compio_postgres::Error> {
        let sql = self.rewrite(sql);
        self.inner.batch_execute(&sql).await
    }

    async fn execute(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, compio_postgres::Error> {
        let sql = self.rewrite(sql);
        self.inner.execute(&sql, params).await
    }

    async fn query(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<Vec<compio_postgres::Row>, compio_postgres::Error> {
        let sql = self.rewrite(sql);
        self.inner.query(&sql, params).await
    }

    async fn query_one(
        &self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<compio_postgres::Row, compio_postgres::Error> {
        let sql = self.rewrite(sql);
        self.inner.query_one(&sql, params).await
    }
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
        zeroship_test_support::skip("skip: CONTROL_TEST_DB not set");
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

async fn scrub_cloned_fixture_data(pg: &TestPg) {
    pg.batch_execute(
        "TRUNCATE TABLE \
             zeroship.workflow_schedules, \
             zeroship.workflow_rollout_config, \
             zeroship.workflow_broadcasts, \
             zeroship.workflow_signal_keys, \
             zeroship.app_deploys, \
             zeroship.apps \
         CASCADE;",
    )
    .await
    .expect("scrub cloned workflow fixture data");
    pg.batch_execute(
        "DO $$ \
         BEGIN \
           IF to_regclass('workflow_scheduler.inflight') IS NOT NULL THEN \
             TRUNCATE TABLE workflow_scheduler.inflight; \
           END IF; \
           IF to_regclass('workflow_scheduler.timers') IS NOT NULL THEN \
             TRUNCATE TABLE workflow_scheduler.timers; \
           END IF; \
           IF to_regclass('zeroship.workflow_e2e_side_effects') IS NOT NULL THEN \
             TRUNCATE TABLE zeroship.workflow_e2e_side_effects; \
           END IF; \
           IF to_regclass('zeroship.workflow_e2e_effect_attempts') IS NOT NULL THEN \
             TRUNCATE TABLE zeroship.workflow_e2e_effect_attempts; \
           END IF; \
           IF to_regclass('zeroship.workflow_e2e_effect_commits') IS NOT NULL THEN \
             TRUNCATE TABLE zeroship.workflow_e2e_effect_commits; \
           END IF; \
         END $$;",
    )
    .await
    .expect("scrub cloned workflow e2e effect tables");
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
    let scheduler_store = WorkflowSchedulerStore::new(db_url.to_string());
    scheduler_store
        .provision()
        .await
        .expect("provision workflow scheduler store");
    let setup_pg = pg(db_url).await;
    setup_pg
        .execute(
            "UPDATE zeroship.plans SET workflows_allowed = true \
              WHERE id IN ($1, $2, $3)",
            &[
                &zeroship_control::bootstrap_console::free_plan_id(),
                &zeroship_control::bootstrap_console::pro_plan_id(),
                &zeroship_control::bootstrap_console::unlimited_plan_id(),
            ],
        )
        .await
        .expect("enable workflow built-in plans for test");
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY, false).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );
    let control_pg = Arc::new(pg(db_url).await);
    let test_pg = TestPg::new(Arc::clone(&control_pg));

    Fixture {
        state: Arc::new(AppState {
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
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
            provider_registry: zeroship_control::metering::provider::builtin_registry(),
            billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
            billing_stream: None,
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
        pg: test_pg,
        db_url: db_url.to_string(),
        blob_root,
        deploy_tmp_dir,
        scheduler_store,
        _db: test_db,
    }
}

async fn spend_blocked_gateway() -> web::HttpResponse {
    web::HttpResponse::PaymentRequired().json(&serde_json::json!({"code": "SPEND_LIMIT"}))
}

async fn seed_app_and_deploy(fx: &Fixture, label: &str) -> (Uuid, String) {
    seed_app_and_deploy_on_plan(
        fx,
        label,
        &zeroship_control::bootstrap_console::free_plan_id(),
    )
    .await
}

async fn seed_app_and_deploy_on_plan(
    fx: &Fixture,
    label: &str,
    plan_id: &str,
) -> (Uuid, String) {
    let app_id = Uuid::new_v4();
    let name = format!("wf-{label}-{}", Uuid::new_v4().simple());
    fx.pg
        .execute(
            "INSERT INTO zeroship.apps (id, name, plan_id, api_key, api_key_hash, workflows_enabled) \
             VALUES ($1, $2, $3, 'test-api-key', 'test-api-key-hash', true)",
            &[&app_id, &name, &plan_id],
        )
        .await
        .expect("insert app");
    PgStore::provision(fx.pg.inner.as_ref(), &app_id)
        .await
        .expect("provision workflow journal");
    fx.pg.set_default_app_id(app_id);
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

async fn seed_additional_deploy(fx: &Fixture, app_id: Uuid, label: &str) -> String {
    let deploy_id = format!("dep_{}_{}", label, Uuid::new_v4().simple());
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
        .expect("insert additional deploy");
    deploy_id
}

async fn age_deploy(fx: &Fixture, deploy_id: &str, activated_at: DateTime<Utc>) {
    fx.pg
        .execute(
            "UPDATE zeroship.app_deploys \
                SET activated_at = $2, created_at = $2 \
              WHERE id = $1",
            &[&deploy_id, &activated_at],
        )
        .await
        .expect("age deploy");
}

async fn put_manifest_for_deploy(fx: &Fixture, app_id: Uuid, deploy_id: &str) -> String {
    let row = fx
        .pg
        .query_one(
            "SELECT deploy_hash, manifest_json \
               FROM zeroship.app_deploys \
              WHERE id = $1 AND app_id = $2",
            &[&deploy_id, &app_id],
        )
        .await
        .expect("load deploy manifest");
    let deploy_hash: String = row.get("deploy_hash");
    let manifest_json: String = row.get("manifest_json");
    fx.state
        .blob_store
        .put_manifest(&app_id, &deploy_hash, manifest_json.as_bytes())
        .await
        .expect("put deploy manifest");
    deploy_hash
}

async fn seed_workflow_cap_plan(fx: &Fixture, label: &str, run_cap: i64, app_cap: i64) -> String {
    let plan_id = format!("pln_wf_cap_{}_{}", label, Uuid::new_v4().simple());
    let runtime = serde_json::json!({
        "cpu_limit_ms": 50,
        "wall_timeout_ms": 5000,
        "heap_limit_mb": 64,
        "workflow_journal_max_bytes": run_cap,
        "workflow_app_journal_max_bytes": app_cap,
    });
    let net = serde_json::json!({
        "max_sockets": 4,
        "egress_ceiling_bytes": 10485760,
    });
    fx.pg
        .execute(
            "INSERT INTO zeroship.plans \
                (id, name, runtime_limits_json, net_policy_limits_json, spend_limit_default_cents, workflows_allowed) \
             VALUES ($1, $2, $3, $4, 0, true)",
            &[&plan_id, &format!("wf-cap-{label}"), &runtime, &net],
        )
        .await
        .expect("insert workflow cap plan");
    plan_id
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
    fx.pg.set_default_app_id(app_id);
    PgStore::provision(fx.pg.inner.as_ref(), &app_id)
        .await
        .expect("provision workflow journal for run seed");
    let run_id = zeroship_core::typed_id::new_workflow_run_id();
    let wake_at = Utc::now() + ChronoDuration::milliseconds(wake_delta_ms);
    let lease_expires = lease_delta_ms.map(|ms| Utc::now() + ChronoDuration::milliseconds(ms));
    let input = serde_json::json!({});
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, wake_at, \
                 claimed_by, lease_expires, dispatch_nonce, \
                 waiting_step_key, started_at, terminal_at) \
             VALUES ($1, 'TestWorkflow', $2, $3, $4, $5, $6, \
                     $7, $8, $9, $10, now(), \
                     CASE WHEN $4 IN ('completed','failed','cancelled','stalled') THEN now() ELSE NULL END)",
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
    if matches!(
        state,
        "queued" | "running" | "sleeping" | "waiting" | "compensating"
    ) {
        fx.scheduler_store
            .register_timer(&run_id, app_id, wake_at)
            .await
            .expect("register seeded workflow timer");
    }
    run_id
}

async fn wait_for_scheduler_timer(fx: &Fixture, run_id: &str) -> DateTime<Utc> {
    for _ in 0..120 {
        if let Some(row) = fx
            .pg
            .query(
                "SELECT wake_at FROM workflow_scheduler.timers WHERE run_id = $1",
                &[&run_id],
            )
            .await
            .expect("load scheduler timer")
            .into_iter()
            .next()
        {
            return row.get("wake_at");
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("scheduler timer for {run_id} did not appear");
}

/// Wait until the scheduler timer for `run_id` exists AND its `wake_at` has PASSED.
///
/// [`wait_for_scheduler_timer`] only waits for the timer ROW to appear and returns its
/// `wake_at`; it does not wait for that instant to arrive. Its name reads like "wait for
/// the timer to fire", which it is not, and call sites discarded the returned value with
/// `let _ =` - throwing away the one piece of information that says whether waiting is
/// still needed.
///
/// A tick fired against a not-yet-due timer correctly claims nothing, so the test fails
/// with `left: 0`. Measured in the full-binary run that led here:
/// `zero_progress_frontier_trips_stuck_strikes_to_stalled` failed exactly that way while
/// already calling the existence-only helper.
async fn wait_until_scheduler_timer_due(fx: &Fixture, run_id: &str) {
    let wake_at = wait_for_scheduler_timer(fx, run_id).await;
    for _ in 0..200 {
        if Utc::now() >= wake_at {
            return;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("scheduler timer for {run_id} never came due (wake_at {wake_at}) within 4s");
}

async fn register_existing_run_timer(fx: &Fixture, run_id: &str) {
    let row = fx
        .pg
        .query_one(
            "SELECT app_id, state, wake_at \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load run for scheduler registration");
    let state: String = row.get("state");
    let wake_at: Option<DateTime<Utc>> = row.get("wake_at");
    if matches!(
        state.as_str(),
        "queued" | "running" | "sleeping" | "waiting" | "compensating"
    ) {
        let app_id: Uuid = row.get("app_id");
        let wake_at = wake_at.expect("active workflow run should have wake_at for test register");
        fx.scheduler_store
            .register_timer(run_id, app_id, wake_at)
            .await
            .expect("register existing workflow timer");
    }
}

fn schedule_policy_config(owner: &str) -> workflow_schedules::ScheduleSweepConfig {
    workflow_schedules::ScheduleSweepConfig {
        batch_size: 4,
        claim_ttl_ms: 1_500,
        backfill_hard_max: 8,
        owner_id: owner.to_string(),
    }
}

fn aligned_planned_instant(ticks_before_now: i64, interval_ms: i64) -> DateTime<Utc> {
    let now_ms = Utc::now().timestamp_millis();
    let aligned_now_ms = now_ms - now_ms.rem_euclid(interval_ms);
    DateTime::<Utc>::from_timestamp_millis(
        aligned_now_ms - ticks_before_now.saturating_mul(interval_ms),
    )
    .expect("valid planned instant")
}

#[allow(clippy::too_many_arguments)]
async fn insert_interval_schedule(
    fx: &Fixture,
    app_id: Uuid,
    deploy_id: &str,
    name: &str,
    workflow_name: &str,
    overlap: &str,
    catch_up: &str,
    catch_up_max: i32,
    interval_ms: i64,
    next_fire_at: DateTime<Utc>,
) -> String {
    let schedule_id = zeroship_core::typed_id::new_workflow_schedule_id();
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_schedules \
                (id, app_id, deploy_id, deploy_hash, name, workflow_name, kind, \
                 interval_ms, anchor, input_json, overlap, catch_up, catch_up_max, \
                 next_fire_at, enabled, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 'interval', \
                     $7, 'epoch', $8, $9, $10, $11, $12, true, now())",
            &[
                &schedule_id,
                &app_id,
                &deploy_id,
                &format!("hash-{deploy_id}"),
                &name,
                &workflow_name,
                &interval_ms,
                &serde_json::json!({"schedule": name}),
                &overlap,
                &catch_up,
                &catch_up_max,
                &next_fire_at,
            ],
        )
        .await
        .expect("insert interval workflow schedule");
    schedule_id
}

async fn force_schedule_due(fx: &Fixture, schedule_id: &str, next_fire_at: DateTime<Utc>) {
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_schedules \
                SET next_fire_at = $2, claimed_by = NULL, claimed_at = NULL, lease_expires = NULL \
              WHERE id = $1",
            &[&schedule_id, &next_fire_at],
        )
        .await
        .expect("force schedule due");
}

async fn schedule_run_count(fx: &Fixture, schedule_id: &str) -> i64 {
    let prefix = format!("sched:{schedule_id}:%");
    fx.pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE dedup_key LIKE $1",
            &[&prefix],
        )
        .await
        .expect("count schedule runs")
        .get("n")
}

async fn schedule_run_started_instants(
    fx: &Fixture,
    schedule_id: &str,
) -> Vec<DateTime<Utc>> {
    let prefix = format!("sched:{schedule_id}:%");
    fx.pg
        .query(
            "SELECT started_at \
               FROM zeroship.workflow_runs \
              WHERE dedup_key LIKE $1 \
              ORDER BY started_at",
            &[&prefix],
        )
        .await
        .expect("load schedule run instants")
        .into_iter()
        .map(|row| row.get("started_at"))
        .collect()
}

async fn schedule_run_scheduler_timer_count(fx: &Fixture, schedule_id: &str) -> i64 {
    let prefix = format!("sched:{schedule_id}:%");
    fx.pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM workflow_scheduler.timers t \
               JOIN zeroship.workflow_runs r ON r.id = t.run_id \
              WHERE r.dedup_key LIKE $1",
            &[&prefix],
        )
        .await
        .expect("count schedule run scheduler timers")
        .get("n")
}

async fn schedule_fire_row(
    fx: &Fixture,
    schedule_id: &str,
) -> (Option<DateTime<Utc>>, Option<i64>, DateTime<Utc>) {
    let row = fx
        .pg
        .query_one(
            "SELECT last_fire_at, last_fired_epoch, next_fire_at \
               FROM zeroship.workflow_schedules \
              WHERE id = $1",
            &[&schedule_id],
        )
        .await
        .expect("load workflow schedule fire row");
    (
        row.get("last_fire_at"),
        row.get("last_fired_epoch"),
        row.get("next_fire_at"),
    )
}

fn config(owner: &str) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        batch_apps: 16,
        per_app_fair_limit: 8,
        max_inflight_per_app: 64,
        max_inflight_dispatch: 64,
        claim_ttl_ms: 1_000,
        heartbeat_ms: 60_000,
        stuck_strike_limit: 3,
        max_child_depth: workflow_engine::DEFAULT_MAX_CHILD_DEPTH,
        max_live_descendants: workflow_engine::DEFAULT_MAX_LIVE_DESCENDANTS,
        max_start_many_batch: workflow_engine::DEFAULT_MAX_START_MANY_BATCH,
        journal_limits: Default::default(),
        owner_id: owner.to_string(),
    }
}

fn timing_test_guard() -> std::sync::MutexGuard<'static, ()> {
    TIMING_TEST_GATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

async fn claim_for_test_dispatch(
    state: &Arc<AppState>,
    request: WorkflowRunDispatchRequest,
) -> Result<StepRequest, DispatchOutcome> {
    let claim_config = config(TEST_WORKER_OWNER);
    match claim_workflow_run_on_conn(
        state.control_pg.as_ref(),
        &request,
        &claim_config,
    )
    .await
    {
        Ok(WorkflowClaimOutcome::Claimed(request)) => Ok(request),
        Ok(WorkflowClaimOutcome::Terminal(registrations)) => Err(DispatchOutcome::Completed(
            WorkflowAdvanceResponse::ack(request.run_id, registrations),
        )),
        Ok(WorkflowClaimOutcome::ClaimLost) => Err(DispatchOutcome::Completed(
            WorkflowAdvanceResponse::nack(
                request.run_id,
                WorkflowAdvanceNackKind::ClaimLost,
                "workflow claim lost",
            ),
        )),
        Ok(WorkflowClaimOutcome::Backpressure(reason)) => Err(DispatchOutcome::Completed(
            WorkflowAdvanceResponse::nack(
                request.run_id,
                WorkflowAdvanceNackKind::Backpressure,
                reason,
            ),
        )),
        Err(e) => Err(DispatchOutcome::Completed(WorkflowAdvanceResponse::nack(
            request.run_id,
            WorkflowAdvanceNackKind::ApplyFailed,
            format!("claim workflow run: {e}"),
        ))),
    }
}

async fn apply_like_worker(
    state: &Arc<AppState>,
    request: &StepRequest,
    result: StepResult,
) -> DispatchOutcome {
    let run_id = result.run_id.clone();
    let apply_config = WorkflowEngineConfig {
        owner_id: request.owner_id.clone(),
        stuck_strike_limit: request.stuck_strike_limit,
        max_child_depth: request.max_child_depth,
        max_live_descendants: request.max_live_descendants,
        max_start_many_batch: request.max_start_many_batch,
        journal_limits: request.journal_limits,
        ..WorkflowEngineConfig::default()
    };
    workflow_engine::apply_step_result_without_scheduler_sync_with_config(
        state,
        apply_config,
        result,
    )
        .await
        .expect("test dispatcher worker-style apply");
    let registrations = collect_post_apply_registrations_on_conn(
        state.control_pg.as_ref(),
        request.app_id,
        &run_id,
        true,
    )
    .await
    .expect("test dispatcher worker-style registrations");
    DispatchOutcome::Completed(WorkflowAdvanceResponse::ack(run_id, registrations))
}

fn authed(req: test::TestRequest, app_id: Uuid) -> test::TestRequest {
    let token =
        zeroship_core::auth::derive_app_scoped_control_token(TEST_CONTROL_KEY, &app_id.to_string());
    req.header("authorization", format!("Bearer {token}"))
        .header(workflow_instance_api::APP_ID_HEADER, app_id.to_string())
}

#[derive(Clone)]
struct BlockingDispatcher {
    state: Arc<AppState>,
    requests: Arc<Mutex<Vec<StepRequest>>>,
    releases: Arc<Mutex<VecDeque<oneshot::Receiver<()>>>>,
}

impl BlockingDispatcher {
    fn with_capacity(state: Arc<AppState>, n: usize) -> (Self, Vec<oneshot::Sender<()>>) {
        let mut receivers = VecDeque::new();
        let mut senders = Vec::new();
        for _ in 0..n {
            let (tx, rx) = oneshot::channel();
            senders.push(tx);
            receivers.push_back(rx);
        }
        (
            Self {
                state,
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
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
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
        let result = StepResult::from_checkpoints(
            request.run_id.clone(),
            request.dispatch_nonce.clone(),
            Vec::new(),
            RunUpdate::Completed {
                output: Some(serde_json::json!({"released": true})),
                output_ref: None,
            },
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone)]
struct CompleteDispatcher {
    state: Arc<AppState>,
}

impl CompleteDispatcher {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::clone(state),
        }
    }
}

#[async_trait(?Send)]
impl StepDispatcher for CompleteDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        let result = StepResult::from_checkpoints(
            request.run_id.clone(),
            request.dispatch_nonce.clone(),
            vec![StepCheckpoint::completed_run(
                0,
                "done",
                serde_json::json!({"ok": true}),
            )],
            RunUpdate::Completed {
                output: Some(serde_json::json!({"ok": true})),
                output_ref: None,
            },
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone, Default)]
struct PreserveAckDispatcher {
    requests: Arc<Mutex<Vec<String>>>,
}

impl PreserveAckDispatcher {
    fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for PreserveAckDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.run_id.clone());
        DispatchOutcome::Completed(WorkflowAdvanceResponse::ack(
            request.run_id.clone(),
            vec![WorkflowAdvanceRegistration::preserve(
                request.run_id,
                request.app_id,
            )],
        ))
    }
}

#[derive(Clone)]
struct JoinChildrenDispatcher {
    state: Arc<AppState>,
}

impl JoinChildrenDispatcher {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::clone(state),
        }
    }
}

#[async_trait(?Send)]
impl StepDispatcher for JoinChildrenDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        let next_child = request
            .journal
            .iter()
            .filter(|step| step.kind == "child" && step.state == "running")
            .min_by_key(|step| step.ordinal)
            .cloned();

        if let Some(child) = next_child {
            let mut checkpoint =
                running_child_checkpoint(child.ordinal, child.child_run_id.as_deref().unwrap_or(""));
            checkpoint.name = child.name;
            checkpoint.name_occurrence = child.name_occurrence;
            let result = StepResult::from_checkpoints(
                request.run_id.clone(),
                request.dispatch_nonce.clone(),
                vec![checkpoint],
                RunUpdate::Waiting { wake_at: None },
            );
            return apply_like_worker(&self.state, &request, result).await;
        }

        let result = StepResult::from_checkpoints(
            request.run_id.clone(),
            request.dispatch_nonce.clone(),
            Vec::new(),
            RunUpdate::Completed {
                output: Some(serde_json::json!({"joined": 3})),
                output_ref: None,
            },
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone)]
struct GatedCheckpointDispatcher {
    state: Arc<AppState>,
    requests: Arc<Mutex<Vec<StepRequest>>>,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    checkpoint: StepCheckpoint,
    run_update: RunUpdate,
}

impl GatedCheckpointDispatcher {
    fn new(
        state: Arc<AppState>,
        checkpoint: StepCheckpoint,
        run_update: RunUpdate,
    ) -> (Self, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();
        (
            Self {
                state,
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
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
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
        let result = StepResult::from_checkpoints(
            request.run_id.clone(),
            request.dispatch_nonce.clone(),
            vec![self.checkpoint.clone()],
            self.run_update.clone(),
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone)]
struct CompleteAfterA {
    state: Arc<AppState>,
}

impl CompleteAfterA {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::clone(state),
        }
    }
}

#[async_trait(?Send)]
impl StepDispatcher for CompleteAfterA {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        assert!(
            request
                .journal
                .iter()
                .any(|step| step.ordinal == 0 && step.name == "a" && step.state == "completed"),
            "resume dispatch should replay the landed a checkpoint: {:?}",
            request.journal
        );
        let result = StepResult::from_checkpoints(
            request.run_id.clone(),
            request.dispatch_nonce.clone(),
            vec![StepCheckpoint::completed_run(
                1,
                "b",
                serde_json::json!({"ok": true}),
            )],
            RunUpdate::Completed {
                output: Some(serde_json::json!({"ok": true})),
                output_ref: None,
            },
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone)]
struct CaughtStepFailureDispatcher {
    state: Arc<AppState>,
    requests: Arc<Mutex<Vec<StepRequest>>>,
}

impl CaughtStepFailureDispatcher {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::clone(state),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<StepRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for CaughtStepFailureDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let saw_failed_step = request
            .journal
            .iter()
            .any(|step| step.ordinal == 0 && step.name == "may-fail" && step.state == "failed");
        let outcomes = if saw_failed_step {
            serde_json::json!([
                {
                    "kind": "StepCompleted",
                    "ordinal": 1,
                    "name": "after-catch",
                    "nameOccurrence": 0,
                    "output": {"continued": true}
                },
                {"kind": "RunCompleted", "output": {"caught": true}}
            ])
        } else {
            serde_json::json!([
                {
                    "kind": "StepFailed",
                    "ordinal": 0,
                    "name": "may-fail",
                    "nameOccurrence": 0,
                    "error": {
                        "type": "PermanentError",
                        "message": "expected caught failure",
                        "retryable": false
                    }
                }
            ])
        };
        let result = batch_step_result(
            &request.run_id,
            &request.dispatch_nonce,
            outcomes,
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone)]
struct UncaughtStepFailureDispatcher {
    state: Arc<AppState>,
    requests: Arc<Mutex<Vec<StepRequest>>>,
}

impl UncaughtStepFailureDispatcher {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::clone(state),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<StepRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for UncaughtStepFailureDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let saw_failed_step = request
            .journal
            .iter()
            .any(|step| step.ordinal == 0 && step.name == "uncaught" && step.state == "failed");
        let outcomes = if saw_failed_step {
            serde_json::json!([
                {
                    "kind": "RunFailed",
                    "error": {
                        "type": "PermanentError",
                        "message": "uncaught failure escaped run()",
                        "retryable": false
                    }
                }
            ])
        } else {
            serde_json::json!([
                {
                    "kind": "StepFailed",
                    "ordinal": 0,
                    "name": "uncaught",
                    "nameOccurrence": 0,
                    "error": {
                        "type": "PermanentError",
                        "message": "expected uncaught failure",
                        "retryable": false
                    }
                }
            ])
        };
        let result = batch_step_result(
            &request.run_id,
            &request.dispatch_nonce,
            outcomes,
        );
        apply_like_worker(&self.state, &request, result).await
    }
}

#[derive(Clone)]
struct ZeroProgressDispatcher {
    state: Arc<AppState>,
    requests: Arc<Mutex<Vec<StepRequest>>>,
}

impl ZeroProgressDispatcher {
    fn new(state: &Arc<AppState>) -> Self {
        Self {
            state: Arc::clone(state),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn requests(&self) -> Vec<StepRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for ZeroProgressDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let request = match claim_for_test_dispatch(&self.state, request).await {
            Ok(request) => request,
            Err(outcome) => return outcome,
        };
        self.requests
            .lock()
            .expect("requests lock")
            .push(request.clone());
        let result = StepResult::requeue(request.run_id.clone(), request.dispatch_nonce.clone());
        apply_like_worker(&self.state, &request, result).await
    }
}

async fn wait_for_requests(dispatcher: &BlockingDispatcher, n: usize) {
    for _ in 0..1_000 {
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

async fn wait_for_preserve_requests(dispatcher: &PreserveAckDispatcher, n: usize) {
    for _ in 0..1_000 {
        if dispatcher.requests().len() >= n {
            return;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "timed out waiting for {n} preserve dispatch requests, got {}",
        dispatcher.requests().len()
    );
}

async fn wait_for_gated_requests(dispatcher: &GatedCheckpointDispatcher, n: usize) {
    for _ in 0..1_000 {
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
            let mut all_acked = true;
            for run_id in run_ids {
                if fx
                    .scheduler_store
                    .inflight(run_id)
                    .await
                    .expect("load scheduler inflight")
                    .is_some()
                {
                    all_acked = false;
                    break;
                }
            }
            if all_acked {
                return;
            }
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

async fn pg_json_size(fx: &Fixture, value: &serde_json::Value) -> i64 {
    fx.pg
        .query_one(
            "SELECT pg_column_size($1::jsonb)::bigint AS bytes",
            &[value],
        )
        .await
        .expect("pg_column_size jsonb")
        .get("bytes")
}

async fn wait_for_cancelled_without_steps(fx: &Fixture, run_id: &str) {
    let mut stable_observations = 0;
    let mut last_state = None;
    for _ in 0..100 {
        let row = fx
            .pg
            .query_one(
                "SELECT state, wake_at, claimed_by, dispatch_nonce \
                   FROM zeroship.workflow_runs WHERE id = $1",
                &[&run_id],
            )
            .await
            .expect("cancelled row");
        let step_rows = fx
            .pg
            .query(
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
                &[&run_id],
            )
            .await
            .expect("count steps");
        let state: String = row.get("state");
        let wake_at: Option<DateTime<Utc>> = row.get("wake_at");
        let claimed_by: Option<String> = row.get("claimed_by");
        let dispatch_nonce: Option<String> = row.get("dispatch_nonce");
        let step_count: i64 = step_rows[0].get("n");

        if state == "cancelled"
            && wake_at.is_none()
            && claimed_by.is_none()
            && dispatch_nonce.is_none()
            && step_count == 0
        {
            stable_observations += 1;
            if stable_observations >= 5 {
                return;
            }
        } else {
            stable_observations = 0;
        }
        last_state = Some((state, wake_at, claimed_by, dispatch_nonce, step_count));
        compio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("late outcome after cancel was not stably discarded: {last_state:?}");
}

/// Assert how many runs a tick claimed, and on mismatch say what is in the database.
///
/// `fire_once` returns only a `usize`, so the fifteen bare `assert_eq!(second, 1)`
/// call sites in this file all fail the same uninformative way: `left: 2, right: 1`
/// and nothing else. That tells you a tick claimed a number nobody expected. It does
/// not tell you WHICH runs, in what state, when they were due, or who holds the
/// claim - which is exactly the information needed to tell "mine was taken" apart
/// from "I took someone else's".
///
/// That gap is not hypothetical. An intermittent failure in this file has now
/// survived three separate explanations (a missing due-timer wait, concurrent
/// interference through the process-global inflight counter, and a counter leak on an
/// unpolled future - refuted by a passing wait, a serialised run, and a run that
/// changed the symptom from `left: 0` to `left: 2` respectively). Each round cost a
/// full-binary run to learn one bit, because the assertion discards everything except
/// the count.
///
/// Dumps the whole `workflow_runs` table rather than the run under test: the
/// interesting case is a tick claiming a row the test did not seed, and filtering to
/// the expected run id would hide precisely that.
async fn assert_tick_claimed(fx: &Fixture, claimed: usize, expected: usize, label: &str) {
    if claimed == expected {
        return;
    }
    let rows = fx
        .pg
        .query(
            "SELECT id, state, wake_at, claimed_by, dispatch_nonce, stuck_strikes, app_id \
               FROM zeroship.workflow_runs ORDER BY id",
            &[],
        )
        .await
        .expect("dump workflow_runs for the claim-count diagnostic");
    let mut dump = String::new();
    for row in &rows {
        let id: String = row.get("id");
        let state: String = row.get("state");
        let wake_at: Option<DateTime<Utc>> = row.get("wake_at");
        let claimed_by: Option<String> = row.get("claimed_by");
        let nonce: Option<String> = row.get("dispatch_nonce");
        let strikes: i16 = row.get("stuck_strikes");
        let app_id: uuid::Uuid = row.get("app_id");
        dump.push_str(&format!(
            "  id={id} state={state} wake_at={wake_at:?} claimed_by={claimed_by:?} \
             nonce={nonce:?} strikes={strikes} app={app_id}\n"
        ));
    }
    panic!(
        "{label}: tick claimed {claimed} runs, expected {expected}.\n\
         All {} row(s) in this fixture's OWN database:\n{dump}\
         If a row appears here that this test did not seed, the tick is claiming \
         across fixtures. If only the seeded row appears, it was claimed a different \
         number of times than expected - compare claimed_by and dispatch_nonce.",
        rows.len(),
    );
}

async fn wait_for_run_state(
    fx: &Fixture,
    run_id: &str,
    expected_state: &str,
) -> (Option<DateTime<Utc>>, i16, Option<serde_json::Value>) {
    let mut last = None;
    for _ in 0..100 {
        let row = fx
            .pg
            .query_one(
                "SELECT state, wake_at, claimed_by, dispatch_nonce, stuck_strikes, error \
                   FROM zeroship.workflow_runs WHERE id = $1",
                &[&run_id],
            )
            .await
            .expect("load run state");
        let state: String = row.get("state");
        let wake_at: Option<DateTime<Utc>> = row.get("wake_at");
        let claimed_by: Option<String> = row.get("claimed_by");
        let dispatch_nonce: Option<String> = row.get("dispatch_nonce");
        let strikes: i16 = row.get("stuck_strikes");
        let error: Option<serde_json::Value> = row.get("error");
        if state == expected_state && claimed_by.is_none() && dispatch_nonce.is_none() {
            return (wake_at, strikes, error);
        }
        last = Some((state, wake_at, claimed_by, dispatch_nonce, strikes, error));
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("run {run_id} did not reach clear state {expected_state}: {last:?}");
}

async fn workflow_step_summaries(fx: &Fixture, run_id: &str) -> Vec<(i32, String, String, String)> {
    fx.pg
        .query(
            "SELECT ordinal, name, kind, state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("load workflow step summaries")
        .into_iter()
        .map(|row| {
            (
                row.get("ordinal"),
                row.get("name"),
                row.get("kind"),
                row.get("state"),
            )
        })
        .collect()
}

fn batch_step_result(run_id: &str, dispatch_nonce: &str, outcomes: serde_json::Value) -> StepResult {
    serde_json::from_value(serde_json::json!({
        "runId": run_id,
        "dispatchNonce": dispatch_nonce,
        "outcomes": outcomes,
    }))
    .expect("batch StepResult JSON")
}

fn child_outcome(name: &str, input: serde_json::Value, cascade: bool) -> serde_json::Value {
    serde_json::json!({
        "kind": "Child",
        "ordinal": 0,
        "name": name,
        "nameOccurrence": 0,
        "childWorkflowName": "TestWorkflow",
        "input": input,
        "options": { "cascade": cascade }
    })
}

fn running_child_checkpoint(ordinal: i32, child_run_id: &str) -> StepCheckpoint {
    StepCheckpoint {
        ordinal,
        name: "ChildEchoWorkflow".to_string(),
        name_occurrence: ordinal,
        kind: "child".to_string(),
        state: "running".to_string(),
        output: None,
        output_ref: None,
        error: None,
        wake_at: None,
        signal_type: Some(workflow_engine::child_signal_type(ordinal)),
        max_signal_age_ms: None,
        consumed_signal_id: None,
        topic: None,
        child_run_id: Some(child_run_id.to_string()),
        child_workflow_name: Some("ChildEchoWorkflow".to_string()),
        child_input: None,
        child_options: None,
        compensation_state: None,
        compensation_max_attempts: 1,
    }
}

async fn assert_due_scheduler_presence(fx: &Fixture, run_id: &str) {
    let row = fx
        .pg
        .query_one(
            "SELECT state, wake_at \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load due run");
    let state: String = row.get("state");
    let wake_at: DateTime<Utc> = row
        .get::<_, Option<DateTime<Utc>>>("wake_at")
        .unwrap_or_else(|| panic!("run {run_id} should have a due wake in state {state}"));
    assert!(
        wake_at <= Utc::now() + ChronoDuration::milliseconds(100),
        "run {run_id} wake_at should be due, got {wake_at:?}"
    );

    let (timer, inflight) = scheduler_presence(fx, run_id).await;
    assert!(
        timer.is_some() || inflight.is_some(),
        "run {run_id} has due journal wake_at {wake_at:?} in state {state} but is absent from scheduler timers and inflight"
    );
}

async fn assert_scheduler_presence(fx: &Fixture, run_id: &str, context: &str) {
    let (timer, inflight) = scheduler_presence(fx, run_id).await;
    assert!(
        timer.is_some() || inflight.is_some(),
        "run {run_id} should remain in scheduler timers or inflight after {context}"
    );
}

async fn scheduler_presence(
    fx: &Fixture,
    run_id: &str,
) -> (
    Option<zeroship_workflow_scheduler::store::TimerRow>,
    Option<zeroship_workflow_scheduler::store::InflightTimer>,
) {
    let timer = fx
        .scheduler_store
        .timer(run_id)
        .await
        .expect("load scheduler timer");
    let inflight = fx
        .scheduler_store
        .inflight(run_id)
        .await
        .expect("load scheduler inflight");
    (timer, inflight)
}

#[test]
fn child_dedup_key_is_parent_and_ordinal_deterministic() {
    assert_eq!(
        workflow_engine::child_dedup_key("run_parent", 7),
        "child:run_parent:7"
    );
    assert_eq!(
        workflow_engine::child_dedup_key("run_parent", 7),
        workflow_engine::child_dedup_key("run_parent", 7)
    );
    assert_ne!(
        workflow_engine::child_dedup_key("run_parent", 7),
        workflow_engine::child_dedup_key("run_parent", 8)
    );
}

#[compio::test]
#[serial]
async fn control_scheduler_reconcile_seeds_from_per_app_journal() {
    let Some(fx) = isolated_fixture("scheduler-boot-reconcile").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "scheduler-boot-reconcile").await;
    let due_run = seed_run(
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
    let future_run = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "sleeping",
        60_000,
        Some("sleep:0:later"),
        None,
        None,
        None,
    )
    .await;

    fx.pg
        .batch_execute("TRUNCATE TABLE workflow_scheduler.inflight, workflow_scheduler.timers")
        .await
        .expect("clear scheduler store before boot reconcile");
    let store = WorkflowSchedulerStore::new(fx.db_url.clone());
    store.provision().await.expect("provision scheduler store");

    let seeded = workflow_engine::reconcile_scheduler_from_journal(&fx.state)
        .await
        .expect("control scheduler reconcile");
    assert_eq!(seeded, 2);
    assert!(store.timer(&due_run).await.expect("due timer").is_some());
    assert!(store.timer(&future_run).await.expect("future timer").is_some());

    let mut wheel = TimerWheel::new(WakeHandle::new());
    let fired = scheduler_store_engine::fire_once(
        &store,
        &mut wheel,
        &StoreSchedulerConfig::default(),
    )
    .await
    .expect("store fire once");
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].run_id, due_run);

    let now = Utc::now() + ChronoDuration::milliseconds(180_000);
    let next_deadline = now + ChronoDuration::milliseconds(120_000);
    let lapsed = store
        .claim_lapsed_inflight(now, 16, next_deadline)
    .await
    .expect("claim lapsed inflight");
    assert_eq!(lapsed.len(), 1);

    // Teardown: the fixture (and any dispatcher/service built from its state)
    // holds a Postgres connection, and locals are dropped only after the body
    // returns - by which point the runtime is gone and the socket can no
    // longer be closed. Drop them explicitly, then wait for the close to land.
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn claim_journal_preserves_same_name_child_occurrences() {
    let Some(fx) = isolated_fixture("child-journal-occurrence").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-journal-occurrence").await;
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

    for ordinal in 0..3 {
        let signal_type = workflow_engine::child_signal_type(ordinal);
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_steps \
                    (run_id, ordinal, name, name_occurrence, kind, state, signal_type, batch_id, batch_width) \
                 VALUES ($1, $2, 'ChildEchoWorkflow', $3, 'child', 'running', $4, 'wfd_seed_children', 3)",
                &[&run_id, &ordinal, &ordinal, &signal_type],
            )
            .await
            .expect("insert same-name child journal row");
    }

    let (dispatcher, releases) = BlockingDispatcher::with_capacity(Arc::clone(&fx.state), 1);
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(dispatcher.clone()),
        config("owner-child-journal-occurrence"),
    )
    .await
    .expect("claim child replay journal");
    assert_eq!(claimed, 1);
    wait_for_requests(&dispatcher, 1).await;

    let requests = dispatcher.requests();
    let journal = &requests[0].journal;
    assert_eq!(
        journal
            .iter()
            .map(|step| (step.ordinal, step.name.clone(), step.name_occurrence))
            .collect::<Vec<_>>(),
        vec![
            (0, "ChildEchoWorkflow".to_string(), 0),
            (1, "ChildEchoWorkflow".to_string(), 1),
            (2, "ChildEchoWorkflow".to_string(), 2),
        ]
    );
    let journal_json = serde_json::to_value(journal).expect("journal serializes");
    assert_eq!(journal_json[1]["nameOccurrence"], 1);
    assert_eq!(journal_json[2]["nameOccurrence"], 2);

    for release in releases {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &[run_id]).await;

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn child_spawn_is_idempotent_and_terminal_hook_wakes_parent() {
    let Some(fx) = isolated_fixture("child-spawn-idempotent").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-spawn-idempotent").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "running",
        -1_000,
        None,
        Some("owner-child"),
        Some(60_000),
        Some("wfd_child"),
    )
    .await;
    let result = batch_step_result(
        &parent,
        "wfd_child",
        serde_json::json!([child_outcome("ChildOnce", serde_json::json!({"n": 1}), true)]),
    );

    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-child", result.clone())
            .await
            .expect("first child spawn apply")
    );
    let row = fx
        .pg
        .query_one(
            "SELECT r.state, r.waiting_step_key, s.child_run_id, s.kind, s.state AS step_state \
               FROM zeroship.workflow_runs r \
               JOIN zeroship.workflow_steps s ON s.run_id = r.id \
              WHERE r.id = $1 AND s.ordinal = 0",
            &[&parent],
        )
        .await
        .expect("parent child step");
    assert_eq!(row.get::<_, String>("state"), "waiting");
    assert_eq!(
        row.get::<_, Option<String>>("waiting_step_key"),
        Some("child:0:ChildOnce".to_string())
    );
    assert_eq!(row.get::<_, String>("kind"), "child");
    assert_eq!(row.get::<_, String>("step_state"), "running");
    let child_id: String = row.get::<_, Option<String>>("child_run_id").expect("child id");

    let child_row = fx
        .pg
        .query_one(
            "SELECT dedup_key, parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, state, input \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&child_id],
        )
        .await
        .expect("child run");
    assert_eq!(
        child_row.get::<_, Option<String>>("dedup_key"),
        Some(workflow_engine::child_dedup_key(&parent, 0))
    );
    assert_eq!(child_row.get::<_, Option<String>>("parent_run_id"), Some(parent.clone()));
    assert_eq!(
        child_row.get::<_, Option<String>>("parent_wait_step_key"),
        Some(workflow_engine::child_signal_type(0))
    );
    assert!(child_row.get::<_, bool>("parent_cascade"));
    assert_eq!(child_row.get::<_, i16>("tree_depth"), 1);
    assert_eq!(child_row.get::<_, String>("state"), "queued");
    assert_eq!(
        child_row.get::<_, serde_json::Value>("input"),
        serde_json::json!({"n": 1})
    );

    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state='running', claimed_by=$1, dispatch_nonce=$2, lease_expires=$3 \
              WHERE id=$4",
            &[
                &"owner-child",
                &"wfd_child",
                &(Utc::now() + ChronoDuration::seconds(60)),
                &parent,
            ],
        )
        .await
        .expect("reclaim parent for child replay");
    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-child", result)
            .await
            .expect("replayed child spawn apply")
    );
    let child_count = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE parent_run_id = $1",
            &[&parent],
        )
        .await
        .expect("count children");
    assert_eq!(child_count.get::<_, i64>("n"), 1, "replay must not double-spawn");

    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state='running', claimed_by=$1, dispatch_nonce=$2, lease_expires=$3 \
              WHERE id=$4",
            &[
                &"owner-child-terminal",
                &"wfd_child_terminal",
                &(Utc::now() + ChronoDuration::seconds(60)),
                &child_id,
            ],
        )
        .await
        .expect("claim child for terminal apply");
    let child_terminal = StepResult::from_checkpoints(
        child_id.clone(),
        "wfd_child_terminal".to_string(),
        Vec::new(),
        RunUpdate::Completed {
            output: Some(serde_json::json!({"child": "ok"})),
            output_ref: None,
        },
    );
    assert!(
        workflow_engine::apply_step_result(
            &fx.state,
            "owner-child-terminal",
            child_terminal.clone(),
        )
        .await
        .expect("child terminal apply")
    );
    assert!(
        fx.scheduler_store
            .timer(&parent)
            .await
            .expect("load parent timer after child terminal")
            .is_some(),
        "child terminal apply must register the parent wake"
    );
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state='running', claimed_by=$1, dispatch_nonce=$2, lease_expires=$3 \
              WHERE id=$4",
            &[
                &"owner-child-terminal",
                &"wfd_child_terminal",
                &(Utc::now() + ChronoDuration::seconds(60)),
                &child_id,
            ],
        )
        .await
        .expect("reclaim child to exercise duplicate terminal hook");
    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-child-terminal", child_terminal)
            .await
            .expect("idempotent child terminal reapply")
    );
    let signals = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_signals \
              WHERE run_id = $1 AND type = $2",
            &[&parent, &workflow_engine::child_signal_type(0)],
        )
        .await
        .expect("count child join signals");
    assert_eq!(signals.get::<_, i64>("n"), 1, "terminal hook must be idempotent");
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = NULL WHERE id = $1",
            &[&parent],
        )
        .await
        .expect("simulate lost parent wake");

    fx.scheduler_store
        .ack_register_next(&parent, app_id, Utc::now())
        .await
        .expect("register lost parent wake safety-net timer");
    let rearmed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-child-rearm-sync"),
    )
    .await
    .expect("parent rearm sync tick");
    assert_eq!(
        rearmed, 1,
        "worker-side claim should consume the repaired parent wake"
    );

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-child-rearm"),
    )
    .await
    .expect("parent rearm tick");
    assert_eq!(claimed, 0, "parent wake should already be retired");
    let parent_step = fx
        .pg
        .query_one(
            "SELECT state, output \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = 0",
            &[&parent],
        )
        .await
        .expect("resolved child step");
    assert_eq!(parent_step.get::<_, String>("state"), "completed");
    assert_eq!(
        parent_step.get::<_, Option<serde_json::Value>>("output"),
        Some(serde_json::json!({"child": "ok"}))
    );

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn concurrent_child_terminals_keep_claimed_parent_registered() {
    let Some(fx) = isolated_fixture("child-join-parent-strand").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-join-parent-strand").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "waiting",
        -1_000,
        Some("child:0:ChildEchoWorkflow"),
        Some("owner-parent-park"),
        Some(60_000),
        Some("wfd_parent_park"),
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET wake_at = NULL \
              WHERE id = $1",
            &[&parent],
        )
        .await
        .expect("park parent without durable wake");
    fx.scheduler_store
        .ack_terminal(&parent)
        .await
        .expect("remove parent from scheduler store");

    let mut child_ids = Vec::new();
    for ordinal in 0..3 {
        let ordinal = ordinal as i32;
        let child_id = zeroship_core::typed_id::new_workflow_run_id();
        let dedup_key = workflow_engine::child_dedup_key(&parent, ordinal);
        let signal_type = workflow_engine::child_signal_type(ordinal);
        let owner = format!("owner-child-{ordinal}");
        let nonce = format!("wfd_child_{ordinal}");
        let lease_expires = Utc::now() + ChronoDuration::seconds(60);
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_runs \
                    (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, \
                     parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at, \
                     claimed_by, lease_expires, dispatch_nonce) \
                 VALUES ($1, 'ChildEchoWorkflow', $2, $3, 'running', $4, $5, now(), \
                         $6, $7, true, 1, now(), $8, $9, $10)",
                &[
                    &child_id,
                    &app_id,
                    &deploy_id,
                    &serde_json::json!({"ordinal": ordinal}),
                    &dedup_key,
                    &parent,
                    &signal_type,
                    &owner,
                    &lease_expires,
                    &nonce,
                ],
            )
            .await
            .expect("insert child run");
        let checkpoint = running_child_checkpoint(ordinal, &child_id);
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_steps \
                    (run_id, ordinal, name, name_occurrence, kind, state, signal_type, child_run_id, batch_id, batch_width) \
                 VALUES ($1, $2, $3, $4, 'child', 'running', $5, $6, 'wfd_seed_children', 3)",
                &[
                    &parent,
                    &checkpoint.ordinal,
                    &checkpoint.name,
                    &checkpoint.name_occurrence,
                    &signal_type,
                    &child_id,
                ],
            )
            .await
            .expect("insert parent child step");
        child_ids.push((ordinal, child_id));
    }

    for (ordinal, child_id) in &child_ids {
        let owner = format!("owner-child-{ordinal}");
        let nonce = format!("wfd_child_{ordinal}");
        let child_terminal = StepResult::from_checkpoints(
            child_id.clone(),
            nonce,
            Vec::new(),
            RunUpdate::Completed {
                output: Some(serde_json::json!({"ordinal": ordinal})),
                output_ref: None,
            },
        );
        assert!(
            workflow_engine::apply_step_result(&fx.state, &owner, child_terminal)
                .await
                .expect("child terminal apply"),
            "child terminal apply should commit for ordinal {ordinal}"
        );
        assert_due_scheduler_presence(&fx, &parent).await;
        if *ordinal == 0 {
            fx.pg
                .execute(
                    "UPDATE zeroship.workflow_runs \
                        SET wake_at = NULL, waiting_step_key = 'child:1:ChildEchoWorkflow' \
                      WHERE id = $1",
                    &[&parent],
                )
                .await
                .expect("simulate parent parking on next child after first join");
            workflow_engine::register_run_timer(&fx.state, &parent)
                .await
                .expect("sync no-wake waiting parent with live children");
            assert_scheduler_presence(
                &fx,
                &parent,
                "no-wake parent park with live child steps",
            )
            .await;
        }
    }

    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET claimed_by = NULL, lease_expires = NULL, dispatch_nonce = NULL \
              WHERE id = $1",
            &[&parent],
        )
        .await
        .expect("release simulated parent park claim");

    let dispatcher = Arc::new(JoinChildrenDispatcher::new(&fx.state));
    let mut completed = false;
    for _ in 0..160 {
        workflow_engine::fire_once(
            &fx.scheduler_store,
            &fx.state,
            Arc::clone(&dispatcher),
            config("owner-child-join-drive"),
        )
        .await
        .expect("drive parent join");
        let row = fx
            .pg
            .query_one(
                "SELECT state \
                   FROM zeroship.workflow_runs \
                  WHERE id = $1",
                &[&parent],
            )
            .await
            .expect("load parent state");
        if row.get::<_, String>("state") == "completed" {
            completed = true;
            break;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        completed,
        "parent join did not settle completed: {:?}",
        {
            let row = fx
                .pg
                .query_one(
                    "SELECT state, claimed_by, dispatch_nonce, wake_at \
                       FROM zeroship.workflow_runs \
                      WHERE id = $1",
                    &[&parent],
                )
                .await
                .expect("load unsettled parent");
            (
                row.get::<_, String>("state"),
                row.get::<_, Option<String>>("claimed_by"),
                row.get::<_, Option<String>>("dispatch_nonce"),
                row.get::<_, Option<DateTime<Utc>>>("wake_at"),
            )
        }
    );

    let parent_row = fx
        .pg
        .query_one(
            "SELECT state, output, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&parent],
        )
        .await
        .expect("load completed parent");
    assert_eq!(parent_row.get::<_, String>("state"), "completed");
    assert_eq!(
        parent_row.get::<_, Option<serde_json::Value>>("output"),
        Some(serde_json::json!({"joined": 3}))
    );
    assert_eq!(parent_row.get::<_, Option<String>>("claimed_by"), None);
    assert_eq!(parent_row.get::<_, Option<String>>("dispatch_nonce"), None);
    assert_eq!(
        workflow_step_summaries(&fx, &parent).await,
        vec![
            (
                0,
                "ChildEchoWorkflow".to_string(),
                "child".to_string(),
                "completed".to_string(),
            ),
            (
                1,
                "ChildEchoWorkflow".to_string(),
                "child".to_string(),
                "completed".to_string(),
            ),
            (
                2,
                "ChildEchoWorkflow".to_string(),
                "child".to_string(),
                "completed".to_string(),
            ),
        ]
    );

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn claim_compensation_drain_registers_parent_wake() {
    let Some(fx) = isolated_fixture("claim-comp-drain-parent").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "claim-comp-drain-parent").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "waiting",
        -1_000,
        Some("child:0:CompensatingChild"),
        None,
        None,
        None,
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = NULL WHERE id = $1",
            &[&parent],
        )
        .await
        .expect("park parent without scheduler wake");
    fx.scheduler_store
        .ack_terminal(&parent)
        .await
        .expect("remove parent scheduler row");

    let child = zeroship_core::typed_id::new_workflow_run_id();
    let signal_type = workflow_engine::child_signal_type(0);
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, \
                 parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at, \
                 compensation_target, error) \
             VALUES ($1, 'CompensatingChild', $2, $3, 'compensating', $4, $5, now(), \
                     $6, $7, true, 1, now(), 'cancelled', $8)",
            &[
                &child,
                &app_id,
                &deploy_id,
                &serde_json::json!({}),
                &workflow_engine::child_dedup_key(&parent, 0),
                &parent,
                &signal_type,
                &serde_json::json!({"type": "Cancelled", "message": "cancelled"}),
            ],
        )
        .await
        .expect("insert compensating child");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, output, output_kind, \
                 batch_id, batch_width, finished_at, compensation_state, compensation_finished_at) \
             VALUES ($1, 0, 'already-undone', 0, 'run', 'completed', $2, 'inline', \
                     'wfd_comp_drain', 1, now(), 'completed', now())",
            &[&child, &serde_json::json!({"ok": true})],
        )
        .await
        .expect("insert drained compensation step");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, signal_type, child_run_id, batch_id, batch_width) \
             VALUES ($1, 0, 'CompensatingChild', 0, 'child', 'running', $2, $3, 'wfd_parent_wait', 1)",
            &[&parent, &signal_type, &child],
        )
        .await
        .expect("insert parent child wait step");
    fx.scheduler_store
        .ack_register_next(&child, app_id, Utc::now())
        .await
        .expect("register compensating child");

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-claim-comp-drain"),
    )
    .await
    .expect("claim drained compensation");
    assert_eq!(claimed, 1);

    let mut parent_wake = None;
    for _ in 0..100 {
        let parent_row = fx
            .pg
            .query_one(
                "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
                &[&parent],
            )
            .await
            .expect("parent after child compensation drain");
        assert_eq!(parent_row.get::<_, String>("state"), "waiting");
        parent_wake = parent_row.get::<_, Option<DateTime<Utc>>>("wake_at");
        if parent_wake.is_some()
            && fx
                .scheduler_store
                .timer(&parent)
                .await
                .expect("load parent scheduler timer")
                .is_some()
        {
            break;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    let parent_wake = parent_wake.expect("child terminal hook should wake parent");
    assert!(
        parent_wake <= Utc::now() + ChronoDuration::milliseconds(100),
        "parent wake should be due-now after compensation drain, got {parent_wake:?}"
    );
    let timer = fx
        .scheduler_store
        .timer(&parent)
        .await
        .expect("load parent scheduler timer")
        .expect("claim drain should register parent timer");
    assert_eq!(timer.run_id, parent);

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn parent_cancel_cascades_cooperatively_to_descendants() {
    let Some(fx) = isolated_fixture("child-cascade").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-cascade").await;
    let parent = seed_run(&fx, app_id, &deploy_id, "queued", -1_000, None, None, None, None).await;
    let child = zeroship_core::typed_id::new_workflow_run_id();
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, \
                 parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at) \
             VALUES ($1, 'TestWorkflow', $2, $3, 'queued', $4, $5, now(), $6, $7, true, 1, now())",
            &[
                &child,
                &app_id,
                &deploy_id,
                &serde_json::json!({}),
                &workflow_engine::child_dedup_key(&parent, 0),
                &parent,
                &workflow_engine::child_signal_type(0),
            ],
        )
        .await
        .expect("insert child run");

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;
    let req = authed(
        test::TestRequest::post().uri(&format!("/internal/workflows/runs/{parent}/cancel")),
        app_id,
    )
    .to_request();
    // Status only: a retained `WebResponse` keeps the app state - and its
    // Postgres client - alive past the teardown at the end of this test.
    let status = test::call_service(&app, req).await.status();
    assert_eq!(status, ntex::http::StatusCode::OK);
    let child_after_cancel = fx
        .pg
        .query_one(
            "SELECT state, cancel_requested FROM zeroship.workflow_runs WHERE id = $1",
            &[&child],
        )
        .await
        .expect("child cancel requested");
    assert_eq!(child_after_cancel.get::<_, String>("state"), "queued");
    assert!(child_after_cancel.get::<_, bool>("cancel_requested"));

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-child-cascade"),
    )
    .await
    .expect("cooperative child cancel tick");
    assert!(
        claimed >= 1,
        "cancel pickup should dispatch at least the child run reference without replaying child code"
    );
    let mut latest_child_state = None;
    for _ in 0..100 {
        let row = fx
            .pg
            .query_one(
                "SELECT state, cancel_requested, claimed_by, dispatch_nonce \
                   FROM zeroship.workflow_runs WHERE id = $1",
                &[&child],
            )
            .await
            .expect("child terminal");
        let state: String = row.get("state");
        let cancel_requested: bool = row.get("cancel_requested");
        let claimed_by: Option<String> = row.get("claimed_by");
        let dispatch_nonce: Option<String> = row.get("dispatch_nonce");
        latest_child_state = Some((state, cancel_requested, claimed_by, dispatch_nonce));
        if latest_child_state
            .as_ref()
            .is_some_and(|(state, _, _, _)| state == "cancelled")
        {
            break;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    let (state, cancel_requested, claimed_by, dispatch_nonce) =
        latest_child_state.expect("child terminal state polled");
    assert_eq!(state, "cancelled");
    assert!(!cancel_requested);
    assert_eq!(claimed_by, None);
    assert_eq!(dispatch_nonce, None);

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn cancel_requested_inflight_child_apply_cancels_without_committing_step() {
    let Some(fx) = isolated_fixture("child-cascade-sleep").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-cascade-sleep").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "cancelled",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;
    let child = zeroship_core::typed_id::new_workflow_run_id();
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, \
                 parent_run_id, parent_wait_step_key, parent_cascade, tree_depth, started_at, \
                 claimed_by, lease_expires, dispatch_nonce) \
             VALUES ($1, 'TestWorkflow', $2, $3, 'running', $4, $5, now(), \
                     $6, $7, true, 1, now(), $8, $9, $10)",
            &[
                &child,
                &app_id,
                &deploy_id,
                &serde_json::json!({}),
                &workflow_engine::child_dedup_key(&parent, 0),
                &parent,
                &workflow_engine::child_signal_type(0),
                &"owner-child-sleep",
                &(Utc::now() + ChronoDuration::seconds(60)),
                &"wfd_child_sleep",
            ],
        )
        .await
        .expect("insert running child run");

    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET cancel_requested = true, wake_at = now() \
              WHERE parent_run_id = $1 \
                AND parent_cascade \
                AND state NOT IN ('completed','failed','cancelled','stalled')",
            &[&parent],
        )
        .await
        .expect("cascade child cancel");
    let future_wake = Utc::now() + ChronoDuration::seconds(60);
    let sleep_result = batch_step_result(
        &child,
        "wfd_child_sleep",
        serde_json::json!([
            {
                "kind": "Sleep",
                "ordinal": 0,
                "name": "child-block",
                "nameOccurrence": 0,
                "wakeAt": future_wake.to_rfc3339()
            }
        ]),
    );
    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-child-sleep", sleep_result)
            .await
            .expect("apply in-flight child cancel")
    );
    let child_terminal = fx
        .pg
        .query_one(
            "SELECT state, wake_at, cancel_requested, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&child],
        )
        .await
        .expect("child terminal");
    assert_eq!(child_terminal.get::<_, String>("state"), "cancelled");
    assert!(!child_terminal.get::<_, bool>("cancel_requested"));
    assert!(child_terminal
        .get::<_, Option<DateTime<Utc>>>("wake_at")
        .is_none());
    assert_eq!(child_terminal.get::<_, Option<String>>("claimed_by"), None);
    assert_eq!(child_terminal.get::<_, Option<String>>("dispatch_nonce"), None);
    let step_count = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1",
            &[&child],
        )
        .await
        .expect("count child workflow steps");
    assert_eq!(
        step_count.get::<_, i64>("n"),
        0,
        "cancelled in-flight child apply must not commit the stale sleep step"
    );
    let timer = fx
        .scheduler_store
        .timer(&child)
        .await
        .expect("load child timer after apply cancel");
    let inflight = fx
        .scheduler_store
        .inflight(&child)
        .await
        .expect("load child inflight after apply cancel");
    assert!(timer.is_none(), "child timer should clear after apply cancel");
    assert!(
        inflight.is_none(),
        "child inflight row should clear after apply cancel"
    );

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn cancel_requested_parked_child_dispatch_is_replay_free_many_iterations() {
    let Some(fx) = isolated_fixture("child-cancel-replay-free-many").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-cancel-replay-free-many").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "cancelled",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;
    let child_count = 64usize;
    let mut child_ids = Vec::with_capacity(child_count);
    for ordinal in 0..child_count {
        let child = zeroship_core::typed_id::new_workflow_run_id();
        let wake_at = Utc::now();
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_runs \
                    (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, \
                     waiting_step_key, parent_run_id, parent_wait_step_key, parent_cascade, \
                     tree_depth, cancel_requested, started_at) \
                 VALUES ($1, 'TestWorkflow', $2, $3, 'sleeping', $4, $5, $6, $7, \
                         $8, $9, true, 1, true, now())",
                &[
                    &child,
                    &app_id,
                    &deploy_id,
                    &serde_json::json!({"ordinal": ordinal}),
                    &workflow_engine::child_dedup_key(&parent, ordinal as i32),
                    &wake_at,
                    &format!("sleep:{ordinal}:child-block"),
                    &parent,
                    &workflow_engine::child_signal_type(ordinal as i32),
                ],
            )
            .await
            .expect("insert parked cancel-requested child");
        child_ids.push(child);
    }

    let mut cfg = config("owner-child-cancel-replay-free-many");
    cfg.per_app_fair_limit = child_count as i64;
    cfg.max_inflight_per_app = child_count as i64;
    cfg.max_inflight_dispatch = child_count;
    let mut terminal_pickups = 0usize;
    for child in &child_ids {
        let outcome = claim_workflow_run_on_conn(
            fx.pg.inner.as_ref(),
            &WorkflowRunDispatchRequest {
                run_id: child.clone(),
                app_id,
            },
            &cfg,
        )
        .await
        .expect("parked cancel replay-free pickup");
        match outcome {
            WorkflowClaimOutcome::Terminal(registrations) => {
                terminal_pickups += 1;
                assert!(
                    registrations.iter().any(|registration| {
                        registration.run_id == child.as_str()
                            && registration.terminal
                            && registration.next_wake_at.is_none()
                    }),
                    "terminal pickup should return a terminal scheduler registration for {child}"
                );
            }
            WorkflowClaimOutcome::Claimed(request) => {
                panic!(
                    "cancel-requested child {} was claimed for workflow execution with nonce {}",
                    request.run_id, request.dispatch_nonce
                );
            }
            WorkflowClaimOutcome::ClaimLost => {
                panic!("cancel-requested child {child} claim was unexpectedly lost");
            }
            WorkflowClaimOutcome::Backpressure(reason) => {
                panic!("cancel-requested child {child} hit backpressure: {reason}");
            }
        }
    }

    assert_eq!(
        terminal_pickups, child_count,
        "each parked cancel-requested child should terminate on pickup"
    );
    let step_count = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_steps \
              WHERE run_id = ANY($1)",
            &[&child_ids],
        )
        .await
        .expect("count parked-cancel child workflow steps");
    assert_eq!(
        step_count.get::<_, i64>("n"),
        0,
        "parked cancel pickup must not replay child workflow code"
    );
    let states = fx
        .pg
        .query(
            "SELECT id, state, cancel_requested, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs \
              WHERE id = ANY($1) \
              ORDER BY id",
            &[&child_ids],
        )
        .await
        .expect("load parked-cancel child states");
    assert_eq!(states.len(), child_count, "missing child rows");
    for row in states {
        assert_eq!(row.get::<_, String>("state"), "cancelled");
        assert!(!row.get::<_, bool>("cancel_requested"));
        assert!(row.get::<_, Option<DateTime<Utc>>>("wake_at").is_none());
        assert_eq!(row.get::<_, Option<String>>("claimed_by"), None);
        assert_eq!(row.get::<_, Option<String>>("dispatch_nonce"), None);
    }

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn cascade_cancel_repair_registers_all_sleeping_children_due_now() {
    let Some(fx) = isolated_fixture("child-cascade-sleep-two").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-cascade-sleep-two").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "cancelled",
        -1_000,
        None,
        None,
        None,
        None,
    )
    .await;

    let mut child_ids = Vec::new();
    for ordinal in 0..2 {
        let child = zeroship_core::typed_id::new_workflow_run_id();
        let future_wake = Utc::now() + ChronoDuration::seconds(60);
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_runs \
                    (id, workflow_name, app_id, deploy_id, state, input, dedup_key, wake_at, \
                     waiting_step_key, parent_run_id, parent_wait_step_key, parent_cascade, \
                     tree_depth, cancel_requested, started_at) \
                 VALUES ($1, 'TestWorkflow', $2, $3, 'sleeping', $4, $5, $6, $7, \
                         $8, $9, true, 1, true, now())",
                &[
                    &child,
                    &app_id,
                    &deploy_id,
                    &serde_json::json!({}),
                    &workflow_engine::child_dedup_key(&parent, ordinal),
                    &future_wake,
                    &format!("sleep:{ordinal}:child-block"),
                    &parent,
                    &workflow_engine::child_signal_type(ordinal),
                ],
            )
            .await
            .expect("insert sleeping cascade child");
        fx.scheduler_store
            .register_timer(&child, app_id, future_wake)
            .await
            .expect("register stale future child timer");
        child_ids.push(child);
    }

    for child in &child_ids {
        let timer = fx
            .scheduler_store
            .timer(child)
            .await
            .expect("load stale child timer")
            .expect("stale child timer exists");
        assert!(
            timer.wake_at > Utc::now(),
            "test must start with a future scheduler-store timer"
        );
    }

    let dispatcher = Arc::new(CompleteDispatcher::new(&fx.state));
    let mut claimed = 0usize;
    for _ in 0..120 {
        claimed = claimed.saturating_add(
            workflow_engine::fire_once(
                &fx.scheduler_store,
                &fx.state,
                Arc::clone(&dispatcher),
                config("owner-child-cascade-sleep-two"),
            )
            .await
            .expect("cascade cancel repair tick"),
        );
        let states = fx
            .pg
            .query(
                "SELECT state, cancel_requested, wake_at, claimed_by, dispatch_nonce \
                   FROM zeroship.workflow_runs \
                  WHERE id = ANY($1) \
                  ORDER BY id",
                &[&child_ids],
            )
            .await
            .expect("load child states");
        if states.iter().all(|row| row.get::<_, String>("state") == "cancelled") {
            assert_eq!(claimed, 2, "both sleeping children should be fired exactly once");
            for row in states {
                assert!(!row.get::<_, bool>("cancel_requested"));
                assert!(row.get::<_, Option<DateTime<Utc>>>("wake_at").is_none());
                assert_eq!(row.get::<_, Option<String>>("claimed_by"), None);
                assert_eq!(row.get::<_, Option<String>>("dispatch_nonce"), None);
            }
            for child in &child_ids {
                let step_count = fx
                    .pg
                    .query_one(
                        "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
                        &[child],
                    )
                    .await
                    .expect("count child workflow steps");
                assert_eq!(
                    step_count.get::<_, i64>("n"),
                    0,
                    "cancel pickup must not replay child workflow code"
                );
                let mut scheduler_cleared = false;
                for _ in 0..100 {
                    let timer = fx
                        .scheduler_store
                        .timer(child)
                        .await
                        .expect("load child timer after cancel");
                    let inflight = fx
                        .scheduler_store
                        .inflight(child)
                        .await
                        .expect("load child inflight after cancel");
                    if timer.is_none() && inflight.is_none() {
                        scheduler_cleared = true;
                        break;
                    }
                    compio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(scheduler_cleared, "child scheduler rows should clear after cancel ack");
            }
            // Teardown: this is an early return out of the polling loop, and
            // locals are dropped only after the body returns - by which point
            // the runtime is gone and the sockets can no longer be closed.
            // Drop them explicitly, then wait for the close to land.
            drop(dispatcher);
            drop(fx);
            common::drain_pg().await;
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }

    panic!(
        "sleeping children did not cancel after {claimed} dispatches: {:?}",
        fx.pg
            .query(
                "SELECT id, state, cancel_requested, wake_at \
                   FROM zeroship.workflow_runs \
                  WHERE id = ANY($1) \
                  ORDER BY id",
                &[&child_ids],
            )
            .await
            .expect("load final child states")
            .into_iter()
            .map(|row| {
                (
                    row.get::<_, String>("id"),
                    row.get::<_, String>("state"),
                    row.get::<_, bool>("cancel_requested"),
                    row.get::<_, Option<DateTime<Utc>>>("wake_at"),
                )
            })
            .collect::<Vec<_>>()
    );
}

#[compio::test]
async fn max_live_descendants_rejects_child_spawn_as_catchable_step_failure() {
    let Some(fx) = isolated_fixture("child-live-cap").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "child-live-cap").await;
    let parent = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "running",
        -1_000,
        None,
        Some("owner-child-cap"),
        Some(60_000),
        Some("wfd_child_cap"),
    )
    .await;
    let mut cfg = config("owner-child-cap");
    cfg.max_live_descendants = 0;
    let result = batch_step_result(
        &parent,
        "wfd_child_cap",
        serde_json::json!([child_outcome("ChildOverCap", serde_json::json!({}), false)]),
    );
    assert!(
        workflow_engine::apply_step_result_with_config(&fx.state, cfg, result)
            .await
            .expect("apply child over live cap")
    );
    let step = fx
        .pg
        .query_one(
            "SELECT state, error \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = 0",
            &[&parent],
        )
        .await
        .expect("failed child step");
    assert_eq!(step.get::<_, String>("state"), "failed");
    let error: Option<serde_json::Value> = step.get("error");
    assert_eq!(
        error.as_ref().and_then(|value| value.get("type")).and_then(serde_json::Value::as_str),
        Some("LimitExceededError")
    );
    let run = fx
        .pg
        .query_one(
            "SELECT state FROM zeroship.workflow_runs WHERE id = $1",
            &[&parent],
        )
        .await
        .expect("parent queued after catchable child cap");
    assert_eq!(run.get::<_, String>("state"), "queued");
    let child_count = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_runs WHERE parent_run_id = $1",
            &[&parent],
        )
        .await
        .expect("child count after cap");
    assert_eq!(child_count.get::<_, i64>("n"), 0);

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn blob_output_step_refcount_co_commits_with_journal_row() {
    let Some(fx) = isolated_fixture("blob-ref-commit").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "blob-ref-commit").await;
        let run_id = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "running",
            0,
            None,
            Some("owner-blob-ref"),
            Some(60_000),
            Some("nonce-blob-ref"),
        )
        .await;
        let bytes = vec![b'x'; 1024 * 1024 + 7];
        let hash = sha256_hex(&bytes);
        fx.state
            .workflow_blob_store
            .put_blob(&hash, &bytes)
            .await
            .expect("write workflow blob");

        let result = batch_step_result(
            &run_id,
            "nonce-blob-ref",
            serde_json::json!([
                {
                    "kind": "StepCompleted",
                    "ordinal": 0,
                    "name": "big",
                    "stepKind": "run",
                    "outputRef": {
                        "kind": "ref",
                        "ref": format!("wfblob:sha256:{hash}"),
                        "hash": hash,
                        "size": bytes.len(),
                        "contentType": "application/json"
                    }
                },
                { "kind": "RunCompleted", "output": { "ok": true } }
            ]),
        );

        assert!(
            workflow_engine::apply_step_result(&fx.state, "owner-blob-ref", result)
                .await
                .expect("apply blob step result")
        );

        let row = fx
            .pg
            .query_one(
                "SELECT output_kind, output_hash, output, output_size \
                   FROM zeroship.workflow_steps \
                  WHERE run_id = $1 AND ordinal = 0",
                &[&run_id],
            )
            .await
            .expect("load blob step");
        assert_eq!(row.get::<_, String>("output_kind"), "blob");
        assert_eq!(row.get::<_, Option<String>>("output_hash").as_deref(), Some(hash.as_str()));
        assert!(row.get::<_, Option<serde_json::Value>>("output").is_none());
        assert_eq!(row.get::<_, Option<i64>>("output_size"), Some(bytes.len() as i64));

        let blob = fx
            .pg
            .query_one(
                "SELECT refcount, size FROM zeroship.workflow_blobs WHERE hash = $1",
                &[&hash],
            )
            .await
            .expect("load workflow blob ref");
        assert_eq!(blob.get::<_, i32>("refcount"), 1);
        assert_eq!(blob.get::<_, i64>("size"), bytes.len() as i64);

        let run = fx
            .pg
            .query_one(
                "SELECT journal_bytes, blob_bytes FROM zeroship.workflow_runs WHERE id = $1",
                &[&run_id],
            )
            .await
            .expect("load run accounting");
        assert!(run.get::<_, i64>("journal_bytes") < bytes.len() as i64);
        assert!(run.get::<_, i64>("blob_bytes") >= bytes.len() as i64);
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn workflow_blob_ref_gc_reclaims_zero_refs_but_not_referenced_hashes() {
    let Some(fx) = isolated_fixture("blob-ref-gc").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "blob-ref-gc").await;
        let run_id = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "running",
            0,
            None,
            Some("owner-blob-gc"),
            Some(60_000),
            Some("nonce-blob-gc"),
        )
        .await;

        let referenced = b"referenced-output".to_vec();
        let referenced_hash = sha256_hex(&referenced);
        fx.state
            .workflow_blob_store
            .put_blob(&referenced_hash, &referenced)
            .await
            .expect("write referenced blob");
        let result = batch_step_result(
            &run_id,
            "nonce-blob-gc",
            serde_json::json!([
                {
                    "kind": "StepCompleted",
                    "ordinal": 0,
                    "name": "kept",
                    "stepKind": "run",
                    "outputRef": {
                        "kind": "ref",
                        "ref": format!("wfblob:sha256:{referenced_hash}"),
                        "hash": referenced_hash.clone(),
                        "size": referenced.len(),
                        "contentType": "application/json"
                    }
                }
            ]),
        );
        assert!(
            workflow_engine::apply_step_result(&fx.state, "owner-blob-gc", result)
                .await
                .expect("apply referenced blob")
        );
        let kept_step = fx
            .pg
            .query_one(
                "SELECT output_kind, output_hash \
                   FROM zeroship.workflow_steps \
                  WHERE run_id = $1 AND ordinal = 0",
                &[&run_id],
            )
            .await
            .expect("load kept step output ref");
        assert_eq!(kept_step.get::<_, String>("output_kind"), "blob");
        assert_eq!(
            kept_step.get::<_, Option<String>>("output_hash").as_deref(),
            Some(referenced_hash.as_str())
        );

        let orphan = b"orphan-output".to_vec();
        let orphan_hash = sha256_hex(&orphan);
        fx.state
            .workflow_blob_store
            .put_blob(&orphan_hash, &orphan)
            .await
            .expect("write orphan blob");
        let old = Utc::now()
            - ChronoDuration::seconds(workflow_blob_gc::REF_SWEEP_GRACE_SECS + 60);
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_blobs \
                    (hash, size, content_type, refcount, last_referenced_at) \
                 VALUES ($1, $2, 'application/json', 0, $3)",
                &[&orphan_hash, &(orphan.len() as i64), &old],
            )
            .await
            .expect("insert orphan ref row");
        fx.pg
            .execute(
                "UPDATE zeroship.workflow_blobs \
                    SET refcount = 0, last_referenced_at = $2 \
                  WHERE hash = $1",
                &[&referenced_hash, &old],
            )
            .await
            .expect("age referenced ref row");

        let deleted = workflow_blob_gc::tick_ref_sweep(&fx.state)
            .await
            .expect("run ref gc");
        assert_eq!(deleted, 1);
        assert!(fx
            .state
            .workflow_blob_store
            .get_blob(&referenced_hash)
            .await
            .is_ok());
        assert!(fx
            .state
            .workflow_blob_store
            .get_blob(&orphan_hash)
            .await
            .is_err());
        let kept_rows = fx
            .pg
            .query(
                "SELECT hash FROM zeroship.workflow_blobs WHERE hash = $1",
                &[&referenced_hash],
            )
            .await
            .expect("load kept ref");
        assert_eq!(kept_rows.len(), 1);
        let orphan_rows = fx
            .pg
            .query(
                "SELECT hash FROM zeroship.workflow_blobs WHERE hash = $1",
                &[&orphan_hash],
            )
            .await
            .expect("load deleted ref");
        assert!(orphan_rows.is_empty());
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn workflow_blob_orphan_gc_reclaims_only_unreferenced_old_files() {
    let Some(fx) = isolated_fixture("blob-orphan-gc").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (_app_id, _deploy_id) = seed_app_and_deploy(&fx, "blob-orphan-gc").await;
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(
                workflow_blob_gc::ORPHAN_SWEEP_GRACE_SECS as u64 + 60,
            ))
            .expect("old mtime");

        let orphan = b"old-orphan-output".to_vec();
        let orphan_hash = sha256_hex(&orphan);
        fx.state
            .workflow_blob_store
            .put_blob(&orphan_hash, &orphan)
            .await
            .expect("write orphan blob");
        set_local_workflow_blob_mtime(&fx.blob_root, &orphan_hash, old);

        let referenced = b"old-referenced-output".to_vec();
        let referenced_hash = sha256_hex(&referenced);
        fx.state
            .workflow_blob_store
            .put_blob(&referenced_hash, &referenced)
            .await
            .expect("write referenced blob");
        set_local_workflow_blob_mtime(&fx.blob_root, &referenced_hash, old);
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_blobs \
                    (hash, size, content_type, refcount, last_referenced_at) \
                 VALUES ($1, $2, 'application/json', 1, now())",
                &[&referenced_hash, &(referenced.len() as i64)],
            )
            .await
            .expect("insert referenced orphan guard");

        let young = b"young-orphan-output".to_vec();
        let young_hash = sha256_hex(&young);
        fx.state
            .workflow_blob_store
            .put_blob(&young_hash, &young)
            .await
            .expect("write young blob");

        let deleted = workflow_blob_gc::tick_orphan_sweep(&fx.state)
            .await
            .expect("run orphan gc");
        assert_eq!(deleted, 1);
        assert!(fx
            .state
            .workflow_blob_store
            .get_blob(&orphan_hash)
            .await
            .is_err());
        assert!(fx
            .state
            .workflow_blob_store
            .get_blob(&referenced_hash)
            .await
            .is_ok());
        assert!(fx
            .state
            .workflow_blob_store
            .get_blob(&young_hash)
            .await
            .is_ok());
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn signal_fanout_redrain_registers_delivered_pending_broadcast() {
    let Some(fx) = isolated_fixture("fanout-redrain-register").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "fanout-redrain-register").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "waiting",
        -1_000,
        Some("wait:0:topic:topic.event"),
        None,
        None,
        None,
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = now() WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("simulate committed fanout wake");
    fx.scheduler_store
        .ack_terminal(&run_id)
        .await
        .expect("simulate lost post-commit fanout register");

    let topic = format!("fanout-redrain-{}", Uuid::new_v4().simple());
    let broadcast_id = zeroship_core::typed_id::new_workflow_broadcast_id();
    let signal_id = zeroship_core::typed_id::new_workflow_signal_id();
    let subscription_id = zeroship_core::typed_id::new_workflow_subscription_id();
    let expires_at = Utc::now() + ChronoDuration::seconds(30);
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_subscriptions \
                (id, app_id, topic, run_id, signal_name, type_filter, ordinal, created_at, expires_at) \
             VALUES ($1, $2, $3, $4, 'topic', 'topic.event', 0, now(), $5)",
            &[&subscription_id, &app_id, &topic, &run_id, &expires_at],
        )
        .await
        .expect("insert topic subscription");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_broadcasts \
                (id, app_id, topic, type, payload, origin, idempotency_key, deploy_id, fanout_state, expires_at) \
             VALUES ($1, $2, $3, 'topic.event', $4, 'app', $5, $6, 'pending', $7)",
            &[
                &broadcast_id,
                &app_id,
                &topic,
                &serde_json::json!({"ok": true}),
                &format!("idem-{broadcast_id}"),
                &deploy_id,
                &expires_at,
            ],
        )
        .await
        .expect("insert pending broadcast");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_signals \
                (id, run_id, type, payload, origin, delivery, topic, broadcast_id, idempotency_key) \
             VALUES ($1, $2, 'topic.event', $3, 'app', 'topic', $4, $5, $6)",
            &[
                &signal_id,
                &run_id,
                &serde_json::json!({"ok": true}),
                &topic,
                &broadcast_id,
                &format!("sig-{signal_id}"),
            ],
        )
        .await
        .expect("insert delivered signal without scheduler registration");

    let stats = workflow_signal_fanout::tick_with_config(
        &fx.state,
        workflow_signal_fanout::FanoutSweepConfig {
            max_broadcasts_per_tick: 1,
            max_deliveries_per_broadcast: 100,
        },
    )
    .await
    .expect("redrain pending broadcast");
    assert_eq!(stats.broadcasts, 1);
    assert_eq!(
        stats.deliveries, 0,
        "redrain should not duplicate delivered signal"
    );

    let timer = fx
        .scheduler_store
        .timer(&run_id)
        .await
        .expect("load redrained timer")
        .expect("redrain should register delivered wake");
    assert!(
        timer.wake_at <= Utc::now() + ChronoDuration::milliseconds(100),
        "redrained timer should be due, got {:?}",
        timer.wake_at
    );
    let state: String = fx
        .pg
        .query_one(
            "SELECT fanout_state FROM zeroship.workflow_broadcasts WHERE id = $1",
            &[&broadcast_id],
        )
        .await
        .expect("load broadcast state")
        .get("fanout_state");
    assert_eq!(state, "completed");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn workflow_retention_reaps_only_expired_terminal_runs() {
    let Some(fx) = isolated_fixture("retention-gc").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "retention-gc").await;
        let window_ms = 2 * 24 * 60 * 60 * 1_000;
        let old_terminal_at = Utc::now() - ChronoDuration::milliseconds(window_ms + 60_000);
        let fresh_terminal_at = Utc::now() - ChronoDuration::milliseconds(60_000);

        let expired = seed_run(
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
        let fresh = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "failed",
            -1_000,
            None,
            None,
            None,
            None,
        )
        .await;
        let live = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "compensating",
            -1_000,
            None,
            None,
            None,
            None,
        )
        .await;

        fx.pg
            .execute(
                "UPDATE zeroship.workflow_runs \
                    SET wake_at = NULL, terminal_at = $2 \
                  WHERE id = $1",
                &[&expired, &old_terminal_at],
            )
            .await
            .expect("age expired terminal run");
        fx.pg
            .execute(
                "UPDATE zeroship.workflow_runs \
                    SET wake_at = NULL, terminal_at = $2 \
                  WHERE id = $1",
                &[&fresh, &fresh_terminal_at],
            )
            .await
            .expect("age fresh terminal run");
        fx.pg
            .execute(
                "UPDATE zeroship.workflow_runs \
                    SET wake_at = NULL, terminal_at = $2 \
                  WHERE id = $1",
                &[&live, &old_terminal_at],
            )
            .await
            .expect("stamp non-terminal run with old timestamp");

        let expired_bytes = b"expired-terminal-blob".to_vec();
        let expired_hash = sha256_hex(&expired_bytes);
        fx.state
            .workflow_blob_store
            .put_blob(&expired_hash, &expired_bytes)
            .await
            .expect("write expired workflow blob");
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_blobs \
                    (hash, size, content_type, refcount, last_referenced_at) \
                 VALUES ($1, $2, 'application/json', 1, $3)",
                &[&expired_hash, &(expired_bytes.len() as i64), &old_terminal_at],
            )
            .await
            .expect("insert expired workflow blob ref");
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_steps \
                    (run_id, ordinal, name, name_occurrence, kind, state, output_kind, \
                     output_hash, output_size, output_content_type, batch_id, batch_width, finished_at) \
                 VALUES ($1, 0, 'expired-blob', 0, 'run', 'completed', 'blob', \
                         $2, $3, 'application/json', 'wfd_retention', 1, $4)",
                &[&expired, &expired_hash, &(expired_bytes.len() as i64), &old_terminal_at],
            )
            .await
            .expect("insert expired blob step");

        for run_id in [&fresh, &live] {
            fx.pg
                .execute(
                    "INSERT INTO zeroship.workflow_steps \
                        (run_id, ordinal, name, name_occurrence, kind, state, output, output_kind, \
                         batch_id, batch_width, finished_at) \
                     VALUES ($1, 0, 'kept', 0, 'run', 'completed', $2, 'inline', \
                             'wfd_retention', 1, now())",
                    &[run_id, &serde_json::json!({"kept": true})],
                )
                .await
                .expect("insert kept step");
        }

        let broadcast_id = zeroship_core::typed_id::new_workflow_broadcast_id();
        let signal_id = zeroship_core::typed_id::new_workflow_signal_id();
        let subscription_id = zeroship_core::typed_id::new_workflow_subscription_id();
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_broadcasts \
                    (id, app_id, topic, type, payload, origin, idempotency_key, deploy_id, \
                     fanout_state, created_at, expires_at) \
                 VALUES ($1, $2, 'retention.topic', 'retention.event', $3, 'app', $4, $5, \
                         'completed', $6, $6)",
                &[
                    &broadcast_id,
                    &app_id,
                    &serde_json::json!({"expired": true}),
                    &format!("idem-{broadcast_id}"),
                    &deploy_id,
                    &old_terminal_at,
                ],
            )
            .await
            .expect("insert expired broadcast");
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_signals \
                    (id, run_id, type, payload, origin, delivery, topic, broadcast_id, \
                     idempotency_key, created_at) \
                 VALUES ($1, $2, 'retention.event', $3, 'app', 'topic', 'retention.topic', $4, \
                         $5, $6)",
                &[
                    &signal_id,
                    &expired,
                    &serde_json::json!({"expired": true}),
                    &broadcast_id,
                    &format!("sig-{signal_id}"),
                    &old_terminal_at,
                ],
            )
            .await
            .expect("insert expired signal");
        fx.pg
            .execute(
                "INSERT INTO zeroship.workflow_subscriptions \
                    (id, app_id, topic, run_id, signal_name, ordinal, created_at, expires_at) \
                 VALUES ($1, $2, 'retention.topic', $3, 'wait', 0, $4, $4)",
                &[&subscription_id, &app_id, &expired, &old_terminal_at],
            )
            .await
            .expect("insert expired subscription");

        let stats = workflow_retention::tick_with_config(
            &fx.state,
            workflow_retention::WorkflowRetentionConfig {
                retention_window_ms: window_ms,
                batch_size: 16,
            },
        )
        .await
        .expect("run retention sweep");
        assert_eq!(stats.runs, 1);
        assert_eq!(stats.steps, 1);
        assert_eq!(stats.signals, 1);
        assert_eq!(stats.subscriptions, 1);
        assert_eq!(stats.broadcasts, 1);
        assert_eq!(stats.blobs, 1);

        let expired_count = fx
            .pg
            .query_one(
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_runs WHERE id = $1",
                &[&expired],
            )
            .await
            .expect("expired run count");
        assert_eq!(expired_count.get::<_, i64>("n"), 0);
        for (run_id, expected_state) in [(&fresh, "failed"), (&live, "compensating")] {
            let row = fx
                .pg
                .query_one(
                    "SELECT state FROM zeroship.workflow_runs WHERE id = $1",
                    &[run_id],
                )
                .await
                .expect("kept run");
            assert_eq!(row.get::<_, String>("state"), expected_state);
            let step_count = fx
                .pg
                .query_one(
                    "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
                    &[run_id],
                )
                .await
                .expect("kept step count");
            assert_eq!(step_count.get::<_, i64>("n"), 1);
        }

        for (sql, value) in [
            (
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
                expired.as_str(),
            ),
            (
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_signals WHERE run_id = $1",
                expired.as_str(),
            ),
            (
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_subscriptions WHERE run_id = $1",
                expired.as_str(),
            ),
            (
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_broadcasts WHERE id = $1",
                broadcast_id.as_str(),
            ),
            (
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_blobs WHERE hash = $1",
                expired_hash.as_str(),
            ),
        ] {
            let row = fx
                .pg
                .query_one(sql, &[&value])
                .await
                .expect("deleted row count");
            assert_eq!(row.get::<_, i64>("n"), 0, "expected no rows for {sql}");
        }
        assert!(fx
            .state
            .workflow_blob_store
            .get_blob(&expired_hash)
            .await
            .is_err());

        let again = workflow_retention::tick_with_config(
            &fx.state,
            workflow_retention::WorkflowRetentionConfig {
                retention_window_ms: window_ms,
                batch_size: 16,
            },
        )
        .await
        .expect("run retention sweep again");
        assert_eq!(again, workflow_retention::RetentionStats::default());
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn deploy_retention_reclaims_superseded_manifest_after_pinned_run_terminal() {
    let Some(fx) = isolated_fixture("deploy-retention-drain").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, old_deploy) = seed_app_and_deploy(&fx, "deploy-retention-drain").await;
        age_deploy(
            &fx,
            &old_deploy,
            Utc::now() - ChronoDuration::minutes(10),
        )
        .await;
        let old_hash = put_manifest_for_deploy(&fx, app_id, &old_deploy).await;
        let live_run = seed_run(
            &fx,
            app_id,
            &old_deploy,
            "sleeping",
            60_000,
            None,
            None,
            None,
            None,
        )
        .await;
        let active_deploy = seed_additional_deploy(&fx, app_id, "deploy-retention-active").await;
        let active_hash = put_manifest_for_deploy(&fx, app_id, &active_deploy).await;

        let count = deploy_retention::deploy_pinned_run_count(
            fx.pg.inner.as_ref(),
            &app_id,
            &old_deploy,
        )
        .await
        .expect("count pinned runs");
        assert_eq!(count, 1);
        assert!(
            !deploy_retention::deploy_bundle_reclaimable(
                fx.pg.inner.as_ref(),
                &app_id,
                &old_deploy,
            )
            .await
            .expect("old deploy guard while live"),
            "superseded deploy with a live pinned run must be retained"
        );
        assert!(
            !deploy_retention::deploy_bundle_reclaimable(
                fx.pg.inner.as_ref(),
                &app_id,
                &active_deploy,
            )
            .await
            .expect("active deploy guard"),
            "active deploy must not be reclaimable even with no pinned runs"
        );

        let retained = deploy_retention::tick_with_config(
            &fx.state,
            deploy_retention::DeployRetentionConfig {
                grace_window_ms: 0,
                batch_size: 16,
            },
        )
        .await
        .expect("deploy retention tick with live pin");
        assert_eq!(retained.candidates, 1);
        assert_eq!(retained.retained_live_pins, 1);
        assert_eq!(retained.manifests_deleted, 0);
        assert!(fx
            .state
            .blob_store
            .get_manifest(&app_id, &old_hash)
            .await
            .is_ok());

        fx.pg
            .execute(
                "UPDATE zeroship.workflow_runs \
                    SET state = 'completed', wake_at = NULL, terminal_at = now() \
                  WHERE id = $1",
                &[&live_run],
            )
            .await
            .expect("complete pinned run");

        let drained_count = deploy_retention::deploy_pinned_run_count(
            fx.pg.inner.as_ref(),
            &app_id,
            &old_deploy,
        )
        .await
        .expect("count drained runs");
        assert_eq!(drained_count, 0);
        assert!(
            deploy_retention::deploy_bundle_reclaimable(
                fx.pg.inner.as_ref(),
                &app_id,
                &old_deploy,
            )
            .await
            .expect("old deploy guard after drain"),
            "superseded deploy with zero live pins must be reclaimable"
        );

        let reclaimed = deploy_retention::tick_with_config(
            &fx.state,
            deploy_retention::DeployRetentionConfig {
                grace_window_ms: 0,
                batch_size: 16,
            },
        )
        .await
        .expect("deploy retention tick after drain");
        assert_eq!(reclaimed.candidates, 1);
        assert_eq!(reclaimed.retained_live_pins, 0);
        assert_eq!(reclaimed.manifests_deleted, 1);
        assert!(matches!(
            fx.state.blob_store.get_manifest(&app_id, &old_hash).await,
            Err(zeroship_bundle::BlobError::NotFound(_))
        ));
        assert!(
            fx.state
                .blob_store
                .get_manifest(&app_id, &active_hash)
                .await
                .is_ok(),
            "active deploy manifest must survive deploy retention"
        );
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn deploy_retention_counts_are_per_app() {
    let Some(fx) = isolated_fixture("deploy-retention-per-app").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_a, old_a) = seed_app_and_deploy(&fx, "deploy-retention-app-a").await;
        let (app_b, old_b) = seed_app_and_deploy(&fx, "deploy-retention-app-b").await;
        let old_at = Utc::now() - ChronoDuration::minutes(10);
        age_deploy(&fx, &old_a, old_at).await;
        age_deploy(&fx, &old_b, old_at).await;
        let old_a_hash = put_manifest_for_deploy(&fx, app_a, &old_a).await;
        let old_b_hash = put_manifest_for_deploy(&fx, app_b, &old_b).await;

        let _run_a = seed_run(
            &fx, app_a, &old_a, "queued", 60_000, None, None, None, None,
        )
        .await;
        let _run_b = seed_run(
            &fx, app_b, &old_b, "completed", -1_000, None, None, None, None,
        )
        .await;
        let active_a = seed_additional_deploy(&fx, app_a, "deploy-retention-app-a-live").await;
        let active_b = seed_additional_deploy(&fx, app_b, "deploy-retention-app-b-live").await;
        let active_a_hash = put_manifest_for_deploy(&fx, app_a, &active_a).await;
        let active_b_hash = put_manifest_for_deploy(&fx, app_b, &active_b).await;

        let count_a = deploy_retention::deploy_pinned_run_count(
            fx.pg.inner.as_ref(),
            &app_a,
            &old_a,
        )
        .await
        .expect("count app a pins");
        let count_b = deploy_retention::deploy_pinned_run_count(
            fx.pg.inner.as_ref(),
            &app_b,
            &old_b,
        )
        .await
        .expect("count app b pins");
        assert_eq!(count_a, 1);
        assert_eq!(count_b, 0);
        assert!(
            !deploy_retention::deploy_bundle_reclaimable(
                fx.pg.inner.as_ref(),
                &app_a,
                &old_a,
            )
            .await
            .expect("app a guard")
        );
        assert!(
            deploy_retention::deploy_bundle_reclaimable(
                fx.pg.inner.as_ref(),
                &app_b,
                &old_b,
            )
            .await
            .expect("app b guard")
        );

        let stats = deploy_retention::tick_with_config(
            &fx.state,
            deploy_retention::DeployRetentionConfig {
                grace_window_ms: 0,
                batch_size: 16,
            },
        )
        .await
        .expect("deploy retention tick");
        assert_eq!(stats.candidates, 2);
        assert_eq!(stats.retained_live_pins, 1);
        assert_eq!(stats.manifests_deleted, 1);

        assert!(
            fx.state
                .blob_store
                .get_manifest(&app_a, &old_a_hash)
                .await
                .is_ok(),
            "app A old deploy stays because app A still has a live pin"
        );
        assert!(matches!(
            fx.state.blob_store.get_manifest(&app_b, &old_b_hash).await,
            Err(zeroship_bundle::BlobError::NotFound(_))
        ));
        assert!(fx
            .state
            .blob_store
            .get_manifest(&app_a, &active_a_hash)
            .await
            .is_ok());
        assert!(fx
            .state
            .blob_store
            .get_manifest(&app_b, &active_b_hash)
            .await
            .is_ok());
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn retention_and_schedule_sweeps_visit_multiple_app_journals() {
    let Some(fx) = isolated_fixture("multi-app-sweeps").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_a, deploy_a) = seed_app_and_deploy(&fx, "sweep-a").await;
        let (app_b, deploy_b) = seed_app_and_deploy(&fx, "sweep-b").await;
        let old_terminal_at = Utc::now() - ChronoDuration::minutes(10);

        let run_a = seed_run(
            &fx, app_a, &deploy_a, "completed", -1_000, None, None, None, None,
        )
        .await;
        let run_b = seed_run(
            &fx, app_b, &deploy_b, "completed", -1_000, None, None, None, None,
        )
        .await;
        for (app_id, run_id) in [(app_a, &run_a), (app_b, &run_b)] {
            fx.pg
                .execute(
                    &TestPg::workflow_sql_for_app(
                        app_id,
                        "UPDATE zeroship.workflow_runs \
                            SET wake_at = NULL, terminal_at = $2 \
                          WHERE id = $1",
                    ),
                    &[run_id, &old_terminal_at],
                )
                .await
                .expect("age terminal run");
        }

        let stats = workflow_retention::tick_with_config(
            &fx.state,
            workflow_retention::WorkflowRetentionConfig {
                retention_window_ms: 1,
                batch_size: 16,
            },
        )
        .await
        .expect("retention sweep");
        assert_eq!(stats.runs, 2, "retention must sweep both app journals");
        for (app_id, run_id) in [(app_a, &run_a), (app_b, &run_b)] {
            let row = fx
                .pg
                .query_one(
                    &TestPg::workflow_sql_for_app(
                        app_id,
                        "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_runs WHERE id = $1",
                    ),
                    &[run_id],
                )
                .await
                .expect("retention run count");
            assert_eq!(row.get::<_, i64>("n"), 0);
        }

        let planned = Utc::now() - ChronoDuration::milliseconds(1_000);
        for (app_id, deploy_id, name) in [
            (app_a, deploy_a.as_str(), "sweep-a"),
            (app_b, deploy_b.as_str(), "sweep-b"),
        ] {
            let schedule_id = zeroship_core::typed_id::new_workflow_schedule_id();
            fx.pg
                .execute(
                    "INSERT INTO zeroship.workflow_schedules \
                        (id, app_id, deploy_id, deploy_hash, name, workflow_name, kind, \
                         interval_ms, anchor, input_json, overlap, catch_up, catch_up_max, \
                         next_fire_at, enabled, updated_at) \
                     VALUES ($1, $2, $3, $4, $5, 'TestWorkflow', 'interval', \
                             1000, 'epoch', $6, 'allow', 'skip', 0, $7, true, now())",
                    &[
                        &schedule_id,
                        &app_id,
                        &deploy_id,
                        &format!("hash-{deploy_id}"),
                        &name,
                        &serde_json::json!({"sweep": name}),
                        &planned,
                    ],
                )
                .await
                .expect("insert due schedule");
        }

        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            workflow_schedules::ScheduleSweepConfig {
                batch_size: 4,
                claim_ttl_ms: 1_500,
                backfill_hard_max: 4,
                owner_id: "multi-app-sweeps".to_string(),
            },
        )
        .await
        .expect("schedule sweep");
        assert_eq!(fired, 2, "schedule sweep must fire both app journals");
        for app_id in [app_a, app_b] {
            let row = fx
                .pg
                .query_one(
                    &TestPg::workflow_sql_for_app(
                        app_id,
                        "SELECT COUNT(*)::bigint AS n \
                           FROM zeroship.workflow_runs \
                          WHERE dedup_key LIKE 'sched:%'",
                    ),
                    &[],
                )
                .await
                .expect("scheduled run count");
            assert_eq!(row.get::<_, i64>("n"), 1);
        }
    })
    .await
    .expect("test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn schedule_overlap_policy_skip_blocks_live_run_and_allow_fires_concurrent_run() {
    let Some(fx) = isolated_fixture("schedule-overlap-policy").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "schedule-overlap-policy").await;
        let interval_ms = 1_000;

        let skip_first = aligned_planned_instant(5, interval_ms);
        let skip_id = insert_interval_schedule(
            &fx,
            app_id,
            &deploy_id,
            "skip-live",
            "TestWorkflow",
            "skipIfRunning",
            "skip",
            0,
            interval_ms,
            skip_first,
        )
        .await;
        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            schedule_policy_config("schedule-overlap-skip-first"),
        )
        .await
        .expect("first skip schedule sweep");
        assert_eq!(fired, 1, "first skip schedule tick should create one run");
        assert_eq!(schedule_run_count(&fx, &skip_id).await, 1);
        assert_eq!(schedule_run_scheduler_timer_count(&fx, &skip_id).await, 1);

        let skip_second = skip_first + ChronoDuration::milliseconds(interval_ms);
        force_schedule_due(&fx, &skip_id, skip_second).await;
        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            schedule_policy_config("schedule-overlap-skip-second"),
        )
        .await
        .expect("second skip schedule sweep");
        assert_eq!(
            fired, 0,
            "skipIfRunning must not create a second run while the first is live"
        );
        assert_eq!(schedule_run_count(&fx, &skip_id).await, 1);
        assert_eq!(schedule_run_started_instants(&fx, &skip_id).await, vec![skip_first]);

        let allow_first = aligned_planned_instant(7, interval_ms);
        let allow_id = insert_interval_schedule(
            &fx,
            app_id,
            &deploy_id,
            "allow-live",
            "TestWorkflow",
            "allow",
            "skip",
            0,
            interval_ms,
            allow_first,
        )
        .await;
        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            schedule_policy_config("schedule-overlap-allow-first"),
        )
        .await
        .expect("first allow schedule sweep");
        assert_eq!(fired, 1, "first allow schedule tick should create one run");

        let allow_second = allow_first + ChronoDuration::milliseconds(interval_ms);
        force_schedule_due(&fx, &allow_id, allow_second).await;
        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            schedule_policy_config("schedule-overlap-allow-second"),
        )
        .await
        .expect("second allow schedule sweep");
        assert_eq!(
            fired, 1,
            "allow overlap should create a second live scheduled run"
        );
        assert_eq!(schedule_run_count(&fx, &allow_id).await, 2);
        assert_eq!(
            schedule_run_started_instants(&fx, &allow_id).await,
            vec![allow_first, allow_second]
        );
        assert_eq!(schedule_run_scheduler_timer_count(&fx, &allow_id).await, 2);
    })
    .await
    .expect("schedule overlap policy test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn schedule_catch_up_backfill_is_bounded_by_max_and_drops_excess() {
    let Some(fx) = isolated_fixture("schedule-catch-up-policy").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "schedule-catch-up-policy").await;
        let interval_ms = 1_000;
        let planned = aligned_planned_instant(9, interval_ms);
        let schedule_id = insert_interval_schedule(
            &fx,
            app_id,
            &deploy_id,
            "catch-up-bounded",
            "TestWorkflow",
            "allow",
            "backfill",
            3,
            interval_ms,
            planned,
        )
        .await;
        let tick_started = Utc::now();

        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            schedule_policy_config("schedule-catch-up-bounded"),
        )
        .await
        .expect("catch-up schedule sweep");
        assert_eq!(
            fired, 3,
            "catch-up backfill must create exactly catch_up_max runs"
        );
        assert_eq!(schedule_run_count(&fx, &schedule_id).await, 3);
        assert_eq!(schedule_run_scheduler_timer_count(&fx, &schedule_id).await, 3);

        let expected = vec![
            planned,
            planned + ChronoDuration::milliseconds(interval_ms),
            planned + ChronoDuration::milliseconds(interval_ms * 2),
        ];
        assert_eq!(
            schedule_run_started_instants(&fx, &schedule_id).await,
            expected
        );
        let (last_fire_at, last_fired_epoch, next_fire_at) =
            schedule_fire_row(&fx, &schedule_id).await;
        assert_eq!(last_fire_at, expected.last().copied());
        assert_eq!(
            last_fired_epoch,
            expected.last().map(DateTime::<Utc>::timestamp_millis)
        );
        assert!(
            next_fire_at > tick_started,
            "excess missed ticks should be dropped by rearming after the sweep clock"
        );
    })
    .await
    .expect("schedule catch-up policy test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn schedule_normal_cadence_fires_one_tick_and_rearms() {
    let Some(fx) = isolated_fixture("schedule-normal-cadence").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "schedule-normal-cadence").await;
        let interval_ms = 1_000;
        let planned = aligned_planned_instant(2, interval_ms);
        let schedule_id = insert_interval_schedule(
            &fx,
            app_id,
            &deploy_id,
            "normal-cadence",
            "TestWorkflow",
            "allow",
            "skip",
            0,
            interval_ms,
            planned,
        )
        .await;
        let tick_started = Utc::now();

        let fired = workflow_schedules::tick_with_config(
            &fx.state,
            schedule_policy_config("schedule-normal-cadence"),
        )
        .await
        .expect("normal cadence schedule sweep");
        assert_eq!(fired, 1, "one due tick should create one scheduled run");
        assert_eq!(schedule_run_count(&fx, &schedule_id).await, 1);
        assert_eq!(schedule_run_started_instants(&fx, &schedule_id).await, vec![planned]);
        assert_eq!(schedule_run_scheduler_timer_count(&fx, &schedule_id).await, 1);

        let (last_fire_at, last_fired_epoch, next_fire_at) =
            schedule_fire_row(&fx, &schedule_id).await;
        assert_eq!(last_fire_at, Some(planned));
        assert_eq!(last_fired_epoch, Some(planned.timestamp_millis()));
        assert!(
            next_fire_at > tick_started,
            "normal cadence schedule should rearm to the next future tick"
        );
    })
    .await
    .expect("schedule normal cadence test timeout");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn batch_step_result_applies_atomically_and_preserves_effn1() {
    let Some(fx) = isolated_fixture("batch-apply").await else {
        return;
    };
    compio::time::timeout(Duration::from_secs(10), async {
        let (app_id, deploy_id) = seed_app_and_deploy(&fx, "batch-apply").await;

        let run_id = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "running",
            -1_000,
            None,
            Some("owner-batch"),
            Some(60_000),
            Some("wfd_batch"),
        )
        .await;
        let result = batch_step_result(
            &run_id,
            "wfd_batch",
            serde_json::json!([
                {"kind": "StepCompleted", "ordinal": 0, "name": "a", "nameOccurrence": 0, "output": "A"},
                {"kind": "StepCompleted", "ordinal": 1, "name": "b", "nameOccurrence": 0, "output": "B"},
                {"kind": "StepCompleted", "ordinal": 2, "name": "c", "nameOccurrence": 0, "output": "C"}
            ]),
        );
        assert!(
            workflow_engine::apply_step_result(&fx.state, "owner-batch", result.clone())
                .await
                .expect("apply 3-wide batch")
        );
        let rows = fx
            .pg
            .query(
                "SELECT ordinal, name, state, batch_id, batch_width \
                   FROM zeroship.workflow_steps \
                  WHERE run_id = $1 \
                  ORDER BY ordinal",
                &[&run_id],
            )
            .await
            .expect("load 3-wide steps");
        assert_eq!(rows.len(), 3);
        for (idx, row) in rows.iter().enumerate() {
            assert_eq!(row.get::<_, i32>("ordinal"), i32::try_from(idx).unwrap());
            assert_eq!(row.get::<_, String>("state"), "completed");
            assert_eq!(row.get::<_, String>("batch_id"), "wfd_batch");
            assert_eq!(row.get::<_, i16>("batch_width"), 3);
        }
        assert_eq!(rows[0].get::<_, String>("name"), "a");
        assert_eq!(rows[1].get::<_, String>("name"), "b");
        assert_eq!(rows[2].get::<_, String>("name"), "c");
        let run = fx
            .pg
            .query_one(
                "SELECT state, next_ordinal FROM zeroship.workflow_runs WHERE id = $1",
                &[&run_id],
            )
            .await
            .expect("load 3-wide run");
        assert_eq!(run.get::<_, String>("state"), "queued");
        assert_eq!(run.get::<_, i32>("next_ordinal"), 3);

        fx.pg
            .execute(
                "UPDATE zeroship.workflow_runs \
                    SET state='running', claimed_by=$1, dispatch_nonce=$2, lease_expires=$3 \
                  WHERE id=$4",
                &[
                    &"owner-batch",
                    &"wfd_batch",
                    &(Utc::now() + ChronoDuration::seconds(60)),
                    &run_id,
                ],
            )
            .await
            .expect("reclaim 3-wide run");
        assert!(
            workflow_engine::apply_step_result(&fx.state, "owner-batch", result)
                .await
                .expect("reapply 3-wide batch")
        );
        let count = fx
            .pg
            .query_one(
                "SELECT COUNT(*)::bigint AS n FROM zeroship.workflow_steps WHERE run_id = $1",
                &[&run_id],
            )
            .await
            .expect("count 3-wide steps");
        assert_eq!(count.get::<_, i64>("n"), 3, "batch re-apply must be idempotent");

        let sleep_run = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "running",
            -1_000,
            None,
            Some("owner-batch-sleep"),
            Some(60_000),
            Some("wfd_batch_sleep"),
        )
        .await;
        let wake_at = Utc::now() + ChronoDuration::seconds(60);
        let sleep_result = batch_step_result(
            &sleep_run,
            "wfd_batch_sleep",
            serde_json::json!([
                {"kind": "StepCompleted", "ordinal": 0, "name": "a", "nameOccurrence": 0, "output": "A"},
                {"kind": "StepCompleted", "ordinal": 1, "name": "b", "nameOccurrence": 0, "output": "B"},
                {"kind": "Sleep", "ordinal": 2, "name": "cooldown", "nameOccurrence": 0, "wakeAt": wake_at.to_rfc3339()}
            ]),
        );
        assert!(
            workflow_engine::apply_step_result(&fx.state, "owner-batch-sleep", sleep_result)
                .await
                .expect("apply sleep batch")
        );
        let sleep_rows = fx
            .pg
            .query(
                "SELECT ordinal, name, kind, state, batch_width \
                   FROM zeroship.workflow_steps \
                  WHERE run_id = $1 \
                  ORDER BY ordinal",
                &[&sleep_run],
            )
            .await
            .expect("load sleep batch steps");
        assert_eq!(
            sleep_rows
                .iter()
                .map(|row| (
                    row.get::<_, i32>("ordinal"),
                    row.get::<_, String>("name"),
                    row.get::<_, String>("kind"),
                    row.get::<_, String>("state"),
                    row.get::<_, i16>("batch_width"),
                ))
                .collect::<Vec<_>>(),
            vec![
                (0, "a".to_string(), "run".to_string(), "completed".to_string(), 3),
                (1, "b".to_string(), "run".to_string(), "completed".to_string(), 3),
                (2, "cooldown".to_string(), "sleep".to_string(), "running".to_string(), 3),
            ]
        );
        let parked = fx
            .pg
            .query_one(
                "SELECT state, next_ordinal, waiting_step_key FROM zeroship.workflow_runs WHERE id = $1",
                &[&sleep_run],
            )
            .await
            .expect("load parked run");
        assert_eq!(parked.get::<_, String>("state"), "sleeping");
        assert_eq!(parked.get::<_, i32>("next_ordinal"), 3);
        assert_eq!(
            parked
                .get::<_, Option<String>>("waiting_step_key")
                .as_deref(),
            Some("sleep:2:cooldown")
        );

        let single_run = seed_run(
            &fx,
            app_id,
            &deploy_id,
            "running",
            -1_000,
            None,
            Some("owner-batch-one"),
            Some(60_000),
            Some("wfd_batch_one"),
        )
        .await;
        let single = batch_step_result(
            &single_run,
            "wfd_batch_one",
            serde_json::json!([
                {"kind": "StepCompleted", "ordinal": 0, "name": "only", "nameOccurrence": 0, "output": {"ok": true}}
            ]),
        );
        assert!(
            workflow_engine::apply_step_result(&fx.state, "owner-batch-one", single)
                .await
                .expect("apply effN=1 batch")
        );
        let single_row = fx
            .pg
            .query_one(
                "SELECT r.state, r.next_ordinal, s.name, s.batch_width \
                   FROM zeroship.workflow_runs r \
                   JOIN zeroship.workflow_steps s ON s.run_id = r.id AND s.ordinal = 0 \
                  WHERE r.id = $1",
                &[&single_run],
            )
            .await
            .expect("load effN=1 row");
        assert_eq!(single_row.get::<_, String>("state"), "queued");
        assert_eq!(single_row.get::<_, i32>("next_ordinal"), 1);
        assert_eq!(single_row.get::<_, String>("name"), "only");
        assert_eq!(single_row.get::<_, i16>("batch_width"), 1);
    })
    .await
    .expect("batch apply regression timed out");

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn caught_step_failure_continues_run_to_completion() {
    let Some(fx) = isolated_fixture("caught-step-failure").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "caught-step-failure").await;
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
    let dispatcher = Arc::new(CaughtStepFailureDispatcher::new(&fx.state));

    let first = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-caught-step-failure"),
    )
    .await
    .expect("first tick");
    assert_eq!(first, 1);
    let (wake_at, strikes, error) = wait_for_run_state(&fx, &run_id, "queued").await;
    assert!(wake_at.is_some(), "failed step should schedule immediate replay");
    assert_eq!(strikes, 0, "failed step row is durable progress");
    assert_eq!(error, None);
    assert_eq!(dispatcher.requests().len(), 1);
    assert_eq!(
        workflow_step_summaries(&fx, &run_id).await,
        vec![(0, "may-fail".to_string(), "run".to_string(), "failed".to_string())]
    );

    // The replay is scheduled at a wake_at slightly in the future, and `wait_for_run_state`
    // above watches the RUN ROW, not the scheduler store's timer. Firing immediately races
    // that deadline and claims nothing.
    //
    // Baseline on the tree before this line existed, 20 isolated runs: 12 passed, 8 failed,
    // every failure at the assertion below - 6 as `left: 0` (this race) and 2 as `left: 2`
    // (the count race the assertion change addresses). So one assertion carried both, which
    // is why both are fixed together here.
    wait_until_scheduler_timer_due(&fx, &run_id).await;

    let second = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-caught-step-failure-replay"),
    )
    .await
    .expect("second tick");
    // Not an exact count: `fire_once` returns TIMERS FIRED, and one run can contribute more
    // than one. Redundant as well as unstable - the three assertions after this one already
    // pin the real contract (the run completes, exactly 2 dispatches, both step rows).
    assert!(
        second >= 1,
        "second tick claimed nothing; the replay was due but was not picked up"
    );
    wait_for_completed(&fx, &[run_id.clone()]).await;
    assert_eq!(dispatcher.requests().len(), 2);
    assert_eq!(
        workflow_step_summaries(&fx, &run_id).await,
        vec![
            (0, "may-fail".to_string(), "run".to_string(), "failed".to_string()),
            (1, "after-catch".to_string(), "run".to_string(), "completed".to_string()),
        ]
    );

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn uncaught_step_failure_fails_after_one_extra_replay() {
    let Some(fx) = isolated_fixture("uncaught-step-failure").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "uncaught-step-failure").await;
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
    let dispatcher = Arc::new(UncaughtStepFailureDispatcher::new(&fx.state));

    let first = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-uncaught-step-failure"),
    )
    .await
    .expect("first tick");
    assert_tick_claimed(&fx, first, 1, "uncaught_step_failure first tick").await;
    let (wake_at, strikes, _) = wait_for_run_state(&fx, &run_id, "queued").await;
    assert!(wake_at.is_some(), "failed step should schedule exactly one replay");
    assert_eq!(strikes, 0);

    // MODE B fix: TICK UNTIL THE REPLAY IS PICKED UP, because one tick is not the
    // contract. A failed step schedules its replay at a wake_at slightly in the future,
    // and `fire_once` fires from the scheduler store's own TimerWheel - a different
    // clock from `workflow_runs.wake_at`, with no API to observe when it comes due. So a
    // single tick races an unobservable deadline and correctly claims zero when it wins:
    // measured at 3 of 14 isolated runs, and STILL 3 of 20 after I tried waiting on the
    // run row's wake_at instead, which proved I was watching the wrong clock.
    //
    // Retrying is not a workaround, it is what production does - `run()` ticks every
    // DEFAULT_TICK_SECS forever, so a test that ticks exactly once is asserting
    // something the system never promises.
    let mut second = 0usize;
    for _ in 0..100 {
        second = workflow_engine::fire_once(
            &fx.scheduler_store,
            &fx.state,
            Arc::clone(&dispatcher),
            config("owner-uncaught-step-failure-replay"),
        )
        .await
        .expect("second tick");
        if second >= 1 {
            break;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
    }
    // MODE A fix: do NOT assert an exact count. `fire_once` returns TIMERS FIRED, not
    // runs claimed - `claimed += 1` per fired timer inside `while claimed < fair_limit`
    // - so one run legitimately yields 2 on some interleavings. Measured: 4 of 14
    // isolated runs returned 2 while dispatching exactly twice in total, identical to
    // every passing run, so the extra increment carries no extra dispatch.
    //
    // Nothing is lost by dropping it. The real contract is asserted below and is both
    // stable and stronger: the run reaches `failed` with a PermanentError, the
    // dispatcher recorded exactly 2 requests, and exactly one failed step row exists.
    // Asserting the tick did SOME work is all this line can honestly claim.
    assert!(
        second >= 1,
        "second tick claimed nothing; the replay was due but was not picked up"
    );
    let (wake_at, strikes, error) = wait_for_run_state(&fx, &run_id, "failed").await;
    assert_eq!(wake_at, None);
    assert_eq!(strikes, 0);
    assert_eq!(
        error.as_ref().and_then(|e| e.get("type")).and_then(serde_json::Value::as_str),
        Some("PermanentError")
    );
    assert_eq!(dispatcher.requests().len(), 2);
    assert_eq!(
        workflow_step_summaries(&fx, &run_id).await,
        vec![(0, "uncaught".to_string(), "run".to_string(), "failed".to_string())],
        "terminal replay must not add a second failed step row"
    );
    let failed_rows = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND state = 'failed'",
            &[&run_id],
        )
        .await
        .expect("count failed rows");
    assert_eq!(failed_rows.get::<_, i64>("n"), 1);

    let terminal_tick = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-uncaught-step-failure-terminal"),
    )
    .await
    .expect("terminal tick");
    assert_eq!(terminal_tick, 0, "terminal failed run must not re-dispatch");
    assert_eq!(dispatcher.requests().len(), 2);

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn zero_progress_frontier_trips_stuck_strikes_to_stalled() {
    let Some(fx) = isolated_fixture("stuck-strikes").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "stuck-strikes").await;
    fx.pg
        .execute(
            "UPDATE zeroship.plans \
                SET runtime_limits_json = runtime_limits_json || $2::jsonb \
              WHERE id = (SELECT plan_id FROM zeroship.apps WHERE id = $1)",
            &[
                &app_id,
                &serde_json::json!({ STUCK_STRIKE_LIMIT_FIELD: 2 }),
            ],
        )
        .await
        .expect("set worker-visible stuck strike limit");
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
    let dispatcher = Arc::new(ZeroProgressDispatcher::new(&fx.state));
    let cfg = config("owner-stuck-strikes");

    let first = workflow_engine::fire_once(&fx.scheduler_store, &fx.state, Arc::clone(&dispatcher), cfg.clone())
        .await
        .expect("first zero-progress tick");
    assert_eq!(first, 1);
    let (wake_at, strikes, error) = wait_for_run_state(&fx, &run_id, "queued").await;
    assert!(wake_at.is_some(), "first strike requeues for another attempt");
    assert_eq!(strikes, 1);
    assert_eq!(error, None);
    // Was `let _ = wait_for_scheduler_timer(..)`, which waits only for the timer ROW to
    // exist and then discarded the `wake_at` it returns - so the tick below could fire
    // against a timer still in the future, claim nothing, and fail with `left: 0`.
    // Observed in the full-binary run of 2026-08-08.
    wait_until_scheduler_timer_due(&fx, &run_id).await;
    assert!(
        workflow_step_summaries(&fx, &run_id).await.is_empty(),
        "UNSETTLED frontier creates no workflow_steps row"
    );

    let second = workflow_engine::fire_once(&fx.scheduler_store, &fx.state, Arc::clone(&dispatcher), cfg)
        .await
        .expect("second zero-progress tick");
    assert_eq!(second, 1);
    let (wake_at, strikes, error) = wait_for_run_state(&fx, &run_id, "stalled").await;
    assert_eq!(wake_at, None);
    assert_eq!(strikes, 2);
    let error = error.expect("stalled error");
    assert_eq!(error["type"], "StalledError");
    assert_eq!(error["stuck_strikes"], 2);
    assert_eq!(error["stuck_strike_limit"], 2);
    assert_eq!(dispatcher.requests().len(), 2);
    assert!(
        workflow_step_summaries(&fx, &run_id).await.is_empty(),
        "wall-budget UNSETTLED outcomes remain no-row through stall"
    );

    let terminal_tick = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-stuck-strikes-terminal"),
    )
    .await
    .expect("terminal stalled tick");
    assert_eq!(terminal_tick, 0, "stalled is terminal and fail-closed");
    assert_eq!(dispatcher.requests().len(), 2);

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
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
    let (dispatcher, releases) = BlockingDispatcher::with_capacity(Arc::clone(&fx.state), 1);

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
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
    assert_eq!(claimed_by.as_deref(), Some(TEST_WORKER_OWNER));
    assert!(nonce.as_deref().is_some_and(|n| n.starts_with("wfd_")));
    assert_eq!(state, "running");

    for release in releases {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &[run_id]).await;

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
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
    let (d1, r1) = BlockingDispatcher::with_capacity(Arc::clone(&fx.state), 4);
    let (d2, r2) = BlockingDispatcher::with_capacity(Arc::clone(&fx.state), 4);
    let mut c1 = config("owner-concurrent-a");
    c1.per_app_fair_limit = 4;
    c1.max_inflight_per_app = 8;
    let mut c2 = config("owner-concurrent-b");
    c2.per_app_fair_limit = 4;
    c2.max_inflight_per_app = 8;

    let (a, b) = futures::join!(
        workflow_engine::fire_once(&fx.scheduler_store, &fx.state, Arc::new(d1.clone()), c1),
        workflow_engine::fire_once(&fx.scheduler_store, &fx.state, Arc::new(d2.clone()), c2),
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

    drop(d1);
    drop(d2);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn stale_lease_is_taken_over_after_ttl() {
    let _timing_guard = timing_test_guard();
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
    let (dispatcher, releases) = BlockingDispatcher::with_capacity(Arc::clone(&fx.state), 1);

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
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
    assert_eq!(claimed_by.as_deref(), Some(TEST_WORKER_OWNER));
    assert_ne!(nonce.as_deref(), Some("wfd_dead"));

    for release in releases {
        let _ = release.send(());
    }
    wait_for_completed(&fx, &[run_id]).await;

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn lease_handoff_rejects_stale_writer_after_second_owner_commits() {
    let Some(fx) = isolated_fixture("lease-handoff-guard").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "lease-handoff-guard").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "running",
        -1_000,
        None,
        Some("owner-lease-a"),
        Some(-10_000),
        Some("wfd_lease_a"),
    )
    .await;

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-lease-b"),
    )
    .await
    .expect("lease handoff claim");
    assert_eq!(claimed, 1, "second owner should reclaim the expired lease");
    wait_for_completed(&fx, &[run_id.clone()]).await;

    let stale = StepResult::from_checkpoints(
        run_id.clone(),
        "wfd_lease_a".to_string(),
        vec![StepCheckpoint::completed_run(
            0,
            "done",
            serde_json::json!({"writer": "stale"}),
        )],
        RunUpdate::Completed {
            output: Some(serde_json::json!({"writer": "stale"})),
            output_ref: None,
        },
    );
    assert!(
        !workflow_engine::apply_step_result(&fx.state, "owner-lease-a", stale)
            .await
            .expect("stale lease apply"),
        "old owner/nonce must not commit after a second owner completed the run"
    );

    let run = fx
        .pg
        .query_one(
            "SELECT state, output, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load lease handoff run");
    assert_eq!(run.get::<_, String>("state"), "completed");
    assert_eq!(
        run.get::<_, Option<serde_json::Value>>("output"),
        Some(serde_json::json!({"ok": true}))
    );
    assert_eq!(run.get::<_, Option<String>>("claimed_by"), None);
    assert_eq!(run.get::<_, Option<String>>("dispatch_nonce"), None);

    let steps = fx
        .pg
        .query(
            "SELECT ordinal, name, output \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("load lease handoff steps");
    assert_eq!(steps.len(), 1, "exactly one checkpoint should win");
    assert_eq!(steps[0].get::<_, i32>("ordinal"), 0);
    assert_eq!(steps[0].get::<_, String>("name"), "done");
    assert_eq!(
        steps[0].get::<_, Option<serde_json::Value>>("output"),
        Some(serde_json::json!({"ok": true}))
    );

    drop(fx);
    common::drain_pg().await;
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
    let (dispatcher, releases) = BlockingDispatcher::with_capacity(Arc::clone(&fx.state), 1);

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
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

    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
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
        Some(60_000),
        Some("wfd_apply"),
    )
    .await;
    let result = StepResult::from_checkpoints(
        run_id.clone(),
        "wfd_apply".to_string(),
        vec![StepCheckpoint::completed_run(
            0,
            "charge",
            serde_json::json!({"charged": true}),
        )],
        RunUpdate::Completed {
            output: Some(serde_json::json!({"charged": true})),
            output_ref: None,
        },
    );

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

    assert_journal_bytes_grows_and_state_cap_errors_without_oversized_row().await;

    drop(fx);
    common::drain_pg().await;
}

async fn assert_journal_bytes_grows_and_state_cap_errors_without_oversized_row() {
    let Some(fx) = isolated_fixture("journal-cap").await else {
        return;
    };
    let first_output = serde_json::json!({"small": "ok"});
    let second_output = serde_json::json!({"large": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"});
    let first_size = pg_json_size(&fx, &first_output).await;
    let second_size = pg_json_size(&fx, &second_output).await;
    let run_cap = first_size + second_size - 1;
    let plan_id = seed_workflow_cap_plan(&fx, "journal-cap", run_cap, 10_000_000).await;
    let (app_id, deploy_id) = seed_app_and_deploy_on_plan(&fx, "journal-cap", &plan_id).await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "running",
        -1_000,
        None,
        Some("owner-journal-cap"),
        Some(60_000),
        Some("wfd_journal_cap_1"),
    )
    .await;

    let first = StepResult::from_checkpoints(
        run_id.clone(),
        "wfd_journal_cap_1".to_string(),
        vec![StepCheckpoint::completed_run(0, "first", first_output.clone())],
        RunUpdate::Queued,
    );
    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-journal-cap", first)
            .await
            .expect("apply first checkpoint")
    );
    let row = fx
        .pg
        .query_one(
            "SELECT journal_bytes FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load journal_bytes after first");
    assert_eq!(row.get::<_, i64>("journal_bytes"), first_size);

    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET state='running', claimed_by=$1, dispatch_nonce=$2, lease_expires=$3 \
              WHERE id=$4",
            &[
                &"owner-journal-cap",
                &"wfd_journal_cap_2",
                &(Utc::now() + ChronoDuration::seconds(60)),
                &run_id,
            ],
        )
        .await
        .expect("reclaim for cap breach");
    let second = StepResult::from_checkpoints(
        run_id.clone(),
        "wfd_journal_cap_2".to_string(),
        vec![StepCheckpoint::completed_run(1, "second", second_output)],
        RunUpdate::Completed {
            output: Some(serde_json::json!({"done": true})),
            output_ref: None,
        },
    );
    assert!(
        workflow_engine::apply_step_result(&fx.state, "owner-journal-cap", second)
            .await
            .expect("apply second checkpoint")
    );

    let row = fx
        .pg
        .query_one(
            "SELECT state, error, journal_bytes \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load capped run");
    assert_eq!(row.get::<_, String>("state"), "failed");
    let error: serde_json::Value = row.get("error");
    assert_eq!(error["error_code"], "workflow_state_cap_exceeded");
    assert_eq!(
        row.get::<_, i64>("journal_bytes"),
        first_size,
        "oversized checkpoint must not advance journal_bytes"
    );
    let rows = fx
        .pg
        .query(
            "SELECT ordinal FROM zeroship.workflow_steps WHERE run_id = $1 ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("load capped steps");
    let ordinals: Vec<i32> = rows.into_iter().map(|row| row.get("ordinal")).collect();
    assert_eq!(ordinals, vec![0], "oversized checkpoint row was not written");
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
        if let Some(waiting_key) = waiting_key {
            let (kind, name, signal_type) = if waiting_key.starts_with("sleep:") {
                ("sleep", "cooldown", None)
            } else {
                ("wait_signal", "go", Some("go"))
            };
            fx.pg
                .execute(
                    "INSERT INTO zeroship.workflow_steps \
                        (run_id, ordinal, name, name_occurrence, kind, state, wake_at, signal_type, batch_id, batch_width) \
                     VALUES ($1, 0, $2, 0, $3, 'running', $4, $5, 'wfd_pause_restore', 1)",
                    &[&run_id, &name, &kind, &wake_at, &signal_type],
                )
                .await
                .expect("insert paused frontier step");
        }
        runs.push((run_id, state.to_string(), wake_at));
    }

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    for (run_id, original_state, _) in &runs {
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
        assert!(
            row.get::<_, Option<DateTime<Utc>>>("wake_at").is_none(),
            "pause should suppress wake_at for {original_state}"
        );
        let (timer, inflight) = scheduler_presence(&fx, run_id).await;
        assert!(
            timer.is_none() && inflight.is_none(),
            "pause should de-register scheduler rows for {original_state}"
        );
    }

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-paused-skip"),
    )
    .await
    .expect("tick");
    assert_eq!(
        claimed, 0,
        "paused rows should not leave stale scheduler work to claim-lose"
    );

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
        let resumed_wake: DateTime<Utc> = row
            .get::<_, Option<DateTime<Utc>>>("wake_at")
            .expect("resume should restore a wake for this test case");
        if matches!(original_state.as_str(), "queued" | "running") {
            assert!(
                resumed_wake <= Utc::now() + ChronoDuration::milliseconds(100),
                "resume should register due-now for {original_state}, got {resumed_wake:?}"
            );
        } else {
            assert!(
                resumed_wake
                    .signed_duration_since(*original_wake)
                    .num_milliseconds()
                    .abs()
                    <= 1,
                "resume should recompute frontier wake for {original_state}"
            );
        }
        assert_scheduler_presence(&fx, run_id, "resume target registration").await;
    }

    let due_runs: Vec<String> = runs
        .iter()
        .filter(|(_, state, _)| state == "queued" || state == "running")
        .map(|(run_id, _, _)| run_id.clone())
        .collect();
    workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-resumed-complete"),
    )
    .await
    .expect("tick resumed");
    wait_for_completed(&fx, &due_runs).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn pause_signal_resume_registers_no_timeout_waiting_run() {
    let Some(fx) = isolated_fixture("pause-signal-resume").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "pause-signal-resume").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "waiting",
        -1_000,
        Some("wait:0:go:go"),
        None,
        None,
        None,
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = NULL WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("park wait without timeout");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, signal_type, batch_id, batch_width) \
             VALUES ($1, 0, 'go', 0, 'wait_signal', 'running', 'go', 'wfd_pause_signal', 1)",
            &[&run_id],
        )
        .await
        .expect("insert no-timeout wait step");
    fx.scheduler_store
        .ack_terminal(&run_id)
        .await
        .expect("start with no scheduler row");

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;

    // Status only for these calls: no read_body follows, so a retained
    // `WebResponse` would keep the app state's Postgres client alive past
    // the teardown at the end of this test.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/pause")),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::OK);
    let paused = fx
        .pg
        .query_one(
            "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("paused row");
    assert_eq!(paused.get::<_, String>("state"), "paused");
    assert_eq!(paused.get::<_, Option<DateTime<Utc>>>("wake_at"), None);
    let (timer, inflight) = scheduler_presence(&fx, &run_id).await;
    assert!(timer.is_none() && inflight.is_none(), "pause should de-register");

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{run_id}/signal"))
                .set_json(&serde_json::json!({"type": "go", "payload": {"during": "pause"}})),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::ACCEPTED);
    let paused_after_signal = fx
        .pg
        .query_one(
            "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("paused signaled row");
    assert_eq!(paused_after_signal.get::<_, String>("state"), "paused");
    assert_eq!(
        paused_after_signal.get::<_, Option<DateTime<Utc>>>("wake_at"),
        None,
        "signal during pause must buffer without re-arming the paused row"
    );

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/resume")),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::OK);
    let resumed = fx
        .pg
        .query_one(
            "SELECT state, wake_at FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("resumed row");
    assert_eq!(resumed.get::<_, String>("state"), "waiting");
    let wake_at: DateTime<Utc> = resumed
        .get::<_, Option<DateTime<Utc>>>("wake_at")
        .expect("resume should re-arm pending signal");
    assert!(
        wake_at <= Utc::now() + ChronoDuration::milliseconds(100),
        "resume should register a due pending signal wake, got {wake_at:?}"
    );
    assert_scheduler_presence(&fx, &run_id, "resume after paused signal").await;

    workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-pause-signal-resume"),
    )
    .await
    .expect("drive resumed signal");
    wait_for_completed(&fx, &[run_id]).await;

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn preserve_ack_parks_no_timeout_wait_off_inflight_reaper() {
    let _timing_guard = timing_test_guard();
    workflow_engine::reset_inflight_dispatches_for_test();
    let Some(fx) = isolated_fixture("preserve-park").await else {
        return;
    };
    let (app_id, deploy_id) = seed_app_and_deploy(&fx, "preserve-park").await;
    let run_id = seed_run(
        &fx,
        app_id,
        &deploy_id,
        "waiting",
        -1_000,
        Some("wait:0:go:go"),
        None,
        None,
        None,
    )
    .await;
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = NULL WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("park waiting run in journal");
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, signal_type, batch_id, batch_width) \
             VALUES ($1, 0, 'go', 0, 'wait_signal', 'running', 'go', 'wfd_preserve_park', 1)",
            &[&run_id],
        )
        .await
        .expect("insert no-timeout wait step");

    let mut cfg = config("owner-preserve-park");
    cfg.claim_ttl_ms = 20;
    let dispatcher = Arc::new(PreserveAckDispatcher::default());
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        cfg.clone(),
    )
    .await
    .expect("dispatch no-timeout wait");
    assert_eq!(claimed, 1);
    wait_for_preserve_requests(&dispatcher, 1).await;

    for _ in 0..100 {
        let (timer, inflight) = scheduler_presence(&fx, &run_id).await;
        if timer.is_none() && inflight.is_none() {
            break;
        }
        compio::time::sleep(Duration::from_millis(10)).await;
    }
    let (timer, inflight) = scheduler_presence(&fx, &run_id).await;
    assert!(timer.is_none(), "parked no-timeout wait should not keep a timer");
    assert!(
        inflight.is_none(),
        "preserve ack must clear scheduler inflight for parked no-timeout wait"
    );

    compio::time::sleep(Duration::from_millis(30)).await;
    let before_reaper = dispatcher.requests().len();
    let reaped = workflow_engine::reap_lapsed_inflight_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        cfg,
        16,
    )
    .await
    .expect("reap parked no-timeout wait");
    assert_eq!(reaped, 0, "parked no-timeout wait should not be redispatched");
    compio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        dispatcher.requests().len(),
        before_reaper,
        "reaper should not dispatch a parked no-timeout wait"
    );

    let app = test::init_service(
        web::App::new()
            .state(Arc::clone(&fx.state))
            .configure(workflow_instance_api::configure),
    )
    .await;
    // Status only: no read_body follows, so a retained `WebResponse` would
    // keep the app state's Postgres client alive past the teardown below.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{run_id}/signal"))
                .set_json(&serde_json::json!({"type": "go", "payload": {"wake": true}})),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::ACCEPTED);
    let timer = fx
        .scheduler_store
        .timer(&run_id)
        .await
        .expect("load signaled timer")
        .expect("signal should re-register parked wait");
    assert!(
        timer.wake_at <= Utc::now() + ChronoDuration::milliseconds(100),
        "signal should register a due wake, got {:?}",
        timer.wake_at
    );
    let inflight = fx
        .scheduler_store
        .inflight(&run_id)
        .await
        .expect("load signaled inflight");
    assert!(inflight.is_none(), "signal registration should not recreate inflight");

    drop(app);
    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
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
    let (dispatcher, release) = GatedCheckpointDispatcher::new(Arc::clone(&fx.state),
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

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-pause-mid"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_gated_requests(&dispatcher, 1).await;

    // Status only for both calls below: no read_body follows either one, so a
    // retained `WebResponse` would keep the app state's Postgres client alive
    // past the teardown at the end of this test.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/pause")),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::OK);
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
    assert_eq!(
        row.get::<_, Option<String>>("claimed_by").as_deref(),
        Some(TEST_WORKER_OWNER)
    );
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

    let skipped = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteAfterA::new(&fx.state)),
        config("owner-paused-after-apply"),
    )
    .await
    .expect("paused tick");
    assert_eq!(
        skipped, 0,
        "paused run should already be retired from the scheduler store after checkpoint"
    );

    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/resume")),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::OK);
    assert_scheduler_presence(&fx, &run_id, "resume after paused checkpoint").await;
    workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteAfterA::new(&fx.state)),
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

    drop(app);
    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn cancel_mid_dispatch_discards_late_outcome() {
    let _timing_guard = timing_test_guard();
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
    let (dispatcher, release) = GatedCheckpointDispatcher::new(Arc::clone(&fx.state),
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

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("owner-cancel-mid"),
    )
    .await
    .expect("tick");
    assert_eq!(claimed, 1);
    wait_for_gated_requests(&dispatcher, 1).await;

    // Status only: no read_body follows, so a retained `WebResponse` would
    // keep the app state's Postgres client alive past the teardown below.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post().uri(&format!("/internal/workflows/runs/{run_id}/cancel")),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(status, ntex::http::StatusCode::OK);
    let _ = release.send(());
    wait_for_cancelled_without_steps(&fx, &run_id).await;
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteDispatcher::new(&fx.state)),
        config("owner-cancelled-skip"),
    )
    .await
    .expect("cancelled tick");
    assert_eq!(
        claimed, 0,
        "cancelled run should already be retired from the scheduler store"
    );

    drop(app);
    drop(dispatcher);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
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
    // Status only: no read_body follows, so a retained `WebResponse` would
    // keep the app state's Postgres client alive past the teardown at the
    // end of this test.
    let status = test::call_service(
        &app,
        authed(
            test::TestRequest::post()
                .uri(&format!("/internal/workflows/runs/{guarded}/restart"))
                .set_json(&serde_json::json!({})),
            app_id,
        )
        .to_request(),
    )
    .await
    .status();
    assert_eq!(
        status,
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
    register_existing_run_timer(&fx, &run_id).await;
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

    workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(CompleteAfterA::new(&fx.state)),
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

    drop(app);
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
async fn per_app_cap_does_not_livelock_queued_runs() {
    let _timing_guard = timing_test_guard();
    workflow_engine::reset_inflight_dispatches_for_test();
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
    let dispatcher = Arc::new(CompleteDispatcher::new(&fx.state));

    for _ in 0..200 {
        workflow_engine::fire_once(&fx.scheduler_store, &fx.state, Arc::clone(&dispatcher), cfg.clone())
            .await
            .expect("tick");
        let elapsed_deadline = Utc::now() - ChronoDuration::milliseconds(1);
        fx.pg
            .execute(
                "UPDATE workflow_scheduler.inflight \
                    SET deadline = $2 \
                  WHERE run_id = ANY($1)",
                &[&run_ids, &elapsed_deadline],
            )
            .await
            .expect("age cap test inflight deadlines");
        workflow_engine::reap_lapsed_inflight_once(
            &fx.scheduler_store,
            &fx.state,
            Arc::clone(&dispatcher),
            cfg.clone(),
            16,
        )
        .await
        .expect("reap cap test lapsed inflight");
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
            // Teardown: this is an early return out of the polling loop, and
            // locals are dropped only after the body returns - by which
            // point the runtime is gone and the sockets can no longer be
            // closed. Drop them explicitly, then wait for the close to land.
            drop(dispatcher);
            drop(fx);
            common::drain_pg().await;
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
        .expect("list runs");
    let states: Vec<(String, String, Option<String>)> = rows
        .into_iter()
        .map(|r| (r.get("id"), r.get("state"), r.get("claimed_by")))
        .collect();
    panic!("queued runs did not all complete under cap: {states:?}");
}

#[ntex::test]
#[serial]
async fn gateway_402_backpressure_parks_claim_without_step_attempt() {
    let _timing_guard = timing_test_guard();
    let gateway = test::server(|| async {
        web::App::new().service(
            web::resource("/__zeroship/internal/workflow-advance")
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

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(GatewayStepDispatcher::new(fx.state.gateway_url.clone())),
        WorkflowEngineConfig::default(),
    )
    .await
    .expect("fire once");
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
            // Teardown: this is an early return out of the polling loop, and
            // locals are dropped only after the body returns - by which
            // point the runtime is gone and the socket can no longer be
            // closed. Drop the fixture explicitly, then wait for the close
            // to land.
            drop(fx);
            common::drain_pg().await;
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
