//! DW-07 durable-workflows M1 keystone e2e.
//!
//! This test is driven by `tests/e2e_durable_workflows.sh`. It expects that
//! script to boot a migrated disposable Postgres on :5440, deploy a real
//! workflow .zship, and keep real gateway + worker processes running.

#![allow(clippy::await_holding_lock, clippy::future_not_send)]

use crate::common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use compio_postgres::{connect, NoTls};
use compio_postgres::types::ToSql;
use futures::channel::oneshot;
use futures::lock::Mutex as AsyncMutex;
use uuid::Uuid;
use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::workflow_blob_gc;
use zeroship_control::cron::workflow_engine::{
    self, DispatchOutcome, GatewayStepDispatcher, StepDispatcher, WorkflowEngineConfig,
};
use zeroship_control::cron::workflow_signal_fanout::{self, FanoutSweepConfig};
use zeroship_control::cron::workflow_schedules::{self, ScheduleSweepConfig};
use serial_test::serial;
use zeroship_control::registry::RegistryError;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_plugin_workflow::advance::{
    WorkflowAdvanceNackKind, WorkflowAdvanceResponse, WorkflowRunDispatchRequest,
};
use zeroship_plugin_workflow::engine::MAX_LIVE_DESCENDANTS_FIELD;
use zeroship_plugin_workflow::store::pg::{PgStore, WorkflowTables};
use zeroship_workflow_scheduler::WorkflowSchedulerStore;

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";
const WORKFLOW_NAME: &str = "KeystoneWorkflow";
const SIGNAL_WORKFLOW_NAME: &str = "SignalWorkflow";
const TOPIC_SIGNAL_WORKFLOW_NAME: &str = "TopicSignalWorkflow";
const CONCURRENT_WORKFLOW_NAME: &str = "ConcurrentWorkflow";
const CONCURRENT_COMMIT_WORKFLOW_NAME: &str = "ConcurrentCommitWorkflow";
const SINGLE_COMMIT_WORKFLOW_NAME: &str = "SingleCommitWorkflow";
const SIDE_EFFECT_WORKFLOW_NAME: &str = "SideEffectWorkflow";
const BARE_AWAIT_WORKFLOW_NAME: &str = "BareAwaitWorkflow";
const NAME_DIVERGENCE_WORKFLOW_NAME: &str = "NameDivergenceWorkflow";
const COMPENSATION_WORKFLOW_NAME: &str = "CompensationWorkflow";
const COMPENSATION_DIVERGENCE_WORKFLOW_NAME: &str = "CompensationNameDivergenceWorkflow";
const SCHEDULED_WORKFLOW_NAME: &str = "ScheduledWorkflow";
const BENCH_WORKFLOW_NAME: &str = "BenchWorkflow";
const BLOB_OUTPUT_WORKFLOW_NAME: &str = "BlobOutputWorkflow";
const STREAM_LIMIT_WORKFLOW_NAME: &str = "StreamLimitWorkflow";
const PARENT_CALL_WORKFLOW_NAME: &str = "ParentCallWorkflow";
const PARENT_START_MANY_WORKFLOW_NAME: &str = "ParentStartManyWorkflow";
const PARENT_CATCH_CHILD_FAILURE_WORKFLOW_NAME: &str = "ParentCatchChildFailureWorkflow";
const PARENT_CASCADE_WORKFLOW_NAME: &str = "ParentCascadeWorkflow";
const PARENT_MANY_CASCADE_WORKFLOW_NAME: &str = "ParentManyCascadeWorkflow";
const CONTINUE_AS_NEW_WORKFLOW_NAME: &str = "ContinueAsNewWorkflow";
const COMPENSABLE_CARRY_WORKFLOW_NAME: &str = "CompensableCarryWorkflow";

static SCHEDULER_PROVISIONED_DB: OnceLock<AsyncMutex<Option<String>>> = OnceLock::new();

fn enabled() -> bool {
    zeroship_core::test_env!("ZEROSHIP_DW_E2E").as_deref() == Some("1")
}

/// Refuse unless `tests/e2e_durable_workflows.sh` is driving this binary.
///
/// The fleet IS the backend here: these tests drive the real control workflow
/// engine against a real gateway, a real worker and a really deployed `.zship`,
/// and none of that exists unless the harness stood it up. Every caller used to
/// announce a skip, which cargo counts as a pass, so a plain
/// `cargo test -p zeroship-control` reported the durable-workflows keystone
/// green without ever starting a process.
///
/// `enabled()` stays the predicate rather than being folded in here: the
/// `ZEROSHIP_DW_E2E` read is what `tests/test_only_env_gate.sh` enumerates, and
/// this function only formats the refusal.
fn require_dw_e2e_fleet() {
    if enabled() {
        return;
    }
    common::refuse_missing_backend(
        "the durable-workflows e2e fleet",
        "ZEROSHIP_DW_E2E is not 1, so no migrated database, deployed workflow \
         bundle, gateway or worker has been stood up for this run",
        "Drive this target through the harness that provisions all of it:\n\
         \x20     tests/e2e_durable_workflows.sh\n\
         \n\
         \x20   It boots a disposable Postgres, applies the platform\n\
         \x20   migrations, starts real control, gateway and worker processes,\n\
         \x20   deploys a real workflow .zship, and then runs these tests with\n\
         \x20   ZEROSHIP_DW_E2E=1 and the ZEROSHIP_DW_E2E_* coordinates this\n\
         \x20   file reads.",
    );
}

/// `value` is the caller's already-read result for `name` (a `test_env!` read
/// at the call site) - the read stays at each literal call site so it remains
/// enumerable; this helper only formats the panic.
fn required_env(name: &str, value: Option<String>) -> String {
    value.unwrap_or_else(|| panic!("{name} must be set by e2e harness"))
}

fn tmpdir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "zs-dw07-{label}-{}",
        Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).expect("mkdir tmp");
    path
}

struct Fixture {
    state: Arc<AppState>,
    pg: TestPg,
    blob_root: PathBuf,
    cleanup_blob_root: bool,
    deploy_tmp_dir: PathBuf,
    app_id: Uuid,
    deploy_id: String,
    scheduler_store: WorkflowSchedulerStore,
}

#[derive(Clone)]
struct TestPg {
    inner: Arc<compio_postgres::Client>,
    app_id: Uuid,
}

impl TestPg {
    fn new(inner: Arc<compio_postgres::Client>, app_id: Uuid) -> Self {
        Self { inner, app_id }
    }

    fn rewrite(&self, sql: &str) -> String {
        let tables = WorkflowTables::for_app_id(&self.app_id);
        // The three `workflow_e2e_*` tables are the harness's own out-of-band
        // side-effect record, and they live in the app's DATA schema (the bare
        // `<app_id>`, quoted) rather than in `zeroship`, because that is the only
        // schema `env.db` can address - see `prepare_side_effect_table`. Mapping
        // them here rather than at ~30 call sites keeps every accessor's SQL
        // reading as the plain table name.
        let app_data_schema = quote_ident(&self.app_id.to_string());
        sql.replace("zeroship.workflow_runs", &tables.runs)
            .replace("zeroship.workflow_steps", &tables.steps)
            .replace("zeroship.workflow_signals", &tables.signals)
            .replace(
                "zeroship.workflow_subscriptions",
                &tables.subscriptions,
            )
            .replace("zeroship.workflow_blobs", &tables.blobs)
            .replace(
                "zeroship.workflow_e2e_",
                &format!("{app_data_schema}.workflow_e2e_"),
            )
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
        if self.cleanup_blob_root {
            let _ = std::fs::remove_dir_all(&self.blob_root);
        }
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

async fn provision_scheduler_store_once(db_url: &str, store: &WorkflowSchedulerStore) {
    let lock = SCHEDULER_PROVISIONED_DB.get_or_init(|| AsyncMutex::new(None));
    let mut provisioned = lock.lock().await;
    if provisioned.as_deref() == Some(db_url) {
        return;
    }
    store
        .provision()
        .await
        .expect("provision workflow scheduler store");
    *provisioned = Some(db_url.to_string());
}

async fn build_fixture(db_url: &str, gateway_url: &str, app_id: Uuid, deploy_id: String) -> Fixture {
    let (blob_root, cleanup_blob_root) = match zeroship_core::test_env!("ZEROSHIP_DW_E2E_BLOB_ROOT")
    {
        Some(root) if !root.trim().is_empty() => (PathBuf::from(root), false),
        _ => (tmpdir("control-blob"), true),
    };
    let deploy_tmp_dir = tmpdir("deploy");
    let registry = Registry::new(db_url).await.expect("registry");
    common::ensure_builtin_plans(&registry).await;
    let scheduler_store = WorkflowSchedulerStore::new(db_url.to_string());
    provision_scheduler_store_once(db_url, &scheduler_store).await;
    let env_store = EnvStore::new(registry.clone(), TEST_MASTER_KEY).expect("env store");
    let stripe_store = StripeStore::new(registry.clone());
    let blob_store: Arc<dyn BlobStore> =
        Arc::new(LocalDiskBlobStore::new(blob_root.clone()).expect("blob store"));
    let workflow_blob_store: Arc<dyn zeroship_bundle::WorkflowBlobStore> = Arc::new(
        zeroship_bundle::LocalWorkflowBlobStore::new(blob_root.clone())
            .expect("workflow blob store"),
    );
    let control_pg = Arc::new(pg(db_url).await);
    common::provision_app_workflow_schema(control_pg.as_ref(), &app_id).await;
    PgStore::provision(control_pg.as_ref(), &app_id)
        .await
        .expect("provision workflow journal");
    let test_pg = TestPg::new(Arc::clone(&control_pg), app_id);

    Fixture {
        state: Arc::new(AppState {
            service_auth: std::sync::Arc::new(zeroship_core::service_peers::ServiceAuth::unconfigured()),
            registry,
            env_store,
            stripe_store,
            blob_store,
            workflow_blob_store,
            control_key: SecretString::new("test-control-key".to_string()),
            master_key: SecretString::new(TEST_MASTER_KEY.to_string()),
            stripe_webhook_secret: SecretString::new(String::new()),
            stripe_secret_key: SecretString::new(String::new()),
            stripe_base_url: "http://127.0.0.1:9".to_string(),
            gateway_url: gateway_url.trim_end_matches('/').to_string(),
            worker_urls: Vec::new(),
            admin_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            webhook_limiter: Arc::new(RateLimiter::new(Quota::per_minute(10_000, 100))),
            origin_scheme: zeroship_core::config::OriginScheme::Http,
            trust_proxy: false,
            worker_enrolment: zeroship_control::worker_enrolment::EnrolmentEnvelope::closed(),
            deploy_tmp_dir: deploy_tmp_dir.clone(),
            control_pg: Arc::clone(&control_pg),
            app_base_domain: "zeroship.localhost".to_string(),
            trusted_oauth_clients: zeroship_control::default_trusted_oauth_clients(),
            expected_oauth_audience: "control.zeroship.ai".to_string(),
            static_policies: zeroship_authz::load_platform_policies()
                .expect("bundled authz policies parse"),
            auth_provider: zeroship_control::platform_auth_provider(
                "https://auth.zeroship.test/oauth2",
                Some(common::platform_jwks_url()),
            ),
            // No platform deploy-token mint here: that is control's OUTBOUND
            // destination for the device flow, and no fixture below drives one.
            provider_registry: zeroship_control::metering::provider::builtin_registry(),
            billing_stack: zeroship_control::metering::provider::BillingStack::for_tests(),
            billing_stream: None,
            tax_provider: zeroship_control::tax::build_tax_provider(
                &zeroship_control::tax::TaxProviderConfig::native(),
            )
            .expect("native tax provider builds"),
            notifier: Arc::new(zeroship_control::notify::RecordingNotifier::new()),
            mailer: std::sync::Arc::new(zeroship_mailer::RecordingMailer::new()),
            pairwise_salt: [0u8; 32],
            projected_charge_cache: Arc::new(
                zeroship_control::billing_read::ProjectedChargeCache::default(),
            ),
        }),
        pg: test_pg,
        blob_root,
        cleanup_blob_root,
        deploy_tmp_dir,
        app_id,
        deploy_id,
        scheduler_store,
    }
}

#[derive(Clone)]
struct CrashOnceDispatcher {
    inner: GatewayStepDispatcher,
    release: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    dropped: Arc<Mutex<Option<oneshot::Sender<DispatchOutcome>>>>,
}

impl CrashOnceDispatcher {
    fn new(
        gateway_url: String,
    ) -> (
        Self,
        oneshot::Receiver<DispatchOutcome>,
        oneshot::Sender<()>,
    ) {
        let (drop_tx, drop_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        (
            Self {
                inner: GatewayStepDispatcher::new(gateway_url, test_control_service_auth()),
                release: Arc::new(Mutex::new(Some(release_rx))),
                dropped: Arc::new(Mutex::new(Some(drop_tx))),
            },
            drop_rx,
            release_tx,
        )
    }
}

#[async_trait(?Send)]
impl StepDispatcher for CrashOnceDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let release = self
            .release
            .lock()
            .expect("release lock")
            .take();
        let Some(release) = release else {
            return self.inner.dispatch(request).await;
        };

        let outcome = self.inner.dispatch(request).await;
        if let Some(tx) = self.dropped.lock().expect("dropped lock").take() {
            let _ = tx.send(outcome.clone());
        }
        let _ = release.await;
        outcome
    }
}

#[derive(Clone)]
struct DropOnceDispatcher {
    inner: GatewayStepDispatcher,
    dropped: Arc<Mutex<Option<oneshot::Sender<DispatchOutcome>>>>,
}

impl DropOnceDispatcher {
    fn new(gateway_url: String) -> (Self, oneshot::Receiver<DispatchOutcome>) {
        let (drop_tx, drop_rx) = oneshot::channel();
        (
            Self {
                inner: GatewayStepDispatcher::new(gateway_url, test_control_service_auth()),
                dropped: Arc::new(Mutex::new(Some(drop_tx))),
            },
            drop_rx,
        )
    }
}

#[async_trait(?Send)]
impl StepDispatcher for DropOnceDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let drop = self.dropped.lock().expect("dropped lock").take();
        let Some(drop) = drop else {
            return self.inner.dispatch(request).await;
        };
        let outcome = self.inner.dispatch(request.clone()).await;
        let _ = drop.send(outcome.clone());
        DispatchOutcome::Backpressure {
            run_id: request.run_id,
            reason: "simulated crash after compensation effect".to_string(),
        }
    }
}

#[derive(Clone)]
struct CountingDispatcher {
    inner: GatewayStepDispatcher,
    count: Arc<AtomicUsize>,
}

impl CountingDispatcher {
    fn new(gateway_url: String) -> Self {
        Self {
            inner: GatewayStepDispatcher::new(gateway_url, test_control_service_auth()),
            count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

#[async_trait(?Send)]
impl StepDispatcher for CountingDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.dispatch(request).await
    }
}

#[derive(Clone)]
struct CapturingDispatcher {
    inner: GatewayStepDispatcher,
    outcomes: Arc<Mutex<Vec<DispatchOutcome>>>,
}

impl CapturingDispatcher {
    fn new(gateway_url: String) -> Self {
        Self {
            inner: GatewayStepDispatcher::new(gateway_url, test_control_service_auth()),
            outcomes: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn outcomes(&self) -> Vec<DispatchOutcome> {
        self.outcomes.lock().expect("outcomes lock").clone()
    }

    fn count(&self) -> usize {
        self.outcomes.lock().expect("outcomes lock").len()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for CapturingDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let outcome = self.inner.dispatch(request).await;
        self.outcomes
            .lock()
            .expect("outcomes lock")
            .push(outcome.clone());
        outcome
    }
}

#[derive(Clone)]
struct TimingGatewayDispatcher {
    inner: GatewayStepDispatcher,
    latencies: Arc<Mutex<Vec<Duration>>>,
}

impl TimingGatewayDispatcher {
    fn new(gateway_url: String) -> Self {
        Self {
            inner: GatewayStepDispatcher::new(gateway_url, test_control_service_auth()),
            latencies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn latencies(&self) -> Vec<Duration> {
        self.latencies.lock().expect("latencies lock").clone()
    }
}

#[async_trait(?Send)]
impl StepDispatcher for TimingGatewayDispatcher {
    async fn dispatch(&self, request: WorkflowRunDispatchRequest) -> DispatchOutcome {
        let started = Instant::now();
        let outcome = self.inner.dispatch(request).await;
        self.latencies
            .lock()
            .expect("latencies lock")
            .push(started.elapsed());
        outcome
    }
}

fn expect_completed_ack(outcome: DispatchOutcome, label: &str) -> WorkflowAdvanceResponse {
    let DispatchOutcome::Completed(response) = outcome else {
        panic!("{label} did not produce a workflow advance ack");
    };
    assert!(response.is_ack(), "{label} returned nack: {response:?}");
    response
}

fn assert_ack_run(response: &WorkflowAdvanceResponse, run_id: &str, label: &str) {
    assert_eq!(
        response.run_id.as_deref(),
        Some(run_id),
        "{label} ack run_id"
    );
    assert!(
        response
            .registrations
            .iter()
            .any(|registration| registration.run_id == run_id),
        "{label} ack registrations did not include dispatched run"
    );
}

// ---------------------------------------------------------------------------
// The out-of-band side-effect record
// ---------------------------------------------------------------------------
//
// WHAT USED TO BE HERE, and why it is gone. Until 2026-08-27 this file carried
// a small HTTP server (`start_side_effect_server` + `handle_side_effect_request`
// + a hand-rolled percent-decoder and `write_http`) bound to 127.0.0.1 on a port
// the shell harness passed in as `ZEROSHIP_DW_E2E_SIDE_PORT`. The deployed
// workflow's step bodies `fetch`ed it, and it shelled out to
// `docker exec … psql -c "<interpolated SQL>"` to append a row.
//
// It connected for exactly one reason: `tests/e2e_durable_workflows.sh` launched
// `zeroship-worker` with `ZEROSHIP_DEV=1`, and the runtime's SSRF gate read that
// variable straight out of the process environment and skipped ALL host/IP
// validation when it was set. That was a production hole - a worker inheriting
// the variable from a unit file had SSRF off for every `fetch` - and it is
// closed: dev-ness is a stated input, written only by `set_dev_mode`, whose one
// caller is `zeroship serve`, and the relaxation it grants is loopback only
// (crates/zeroship-runtime/src/transport/ssrf.rs:45-72, :212-224). A worker
// states nothing, so it refuses 127.0.0.1, and the fixture cannot reach a
// harness server at any address a test machine can bind.
//
// WHAT REPLACED IT: the step bodies call `env.db` (see the fixture in
// tests/e2e_durable_workflows.sh). Same three tables, same columns, same
// assertions below - the WRITER changed from "HTTP -> psql subprocess" to "a
// native platform primitive", and the transport disappeared rather than moving.
// The tables moved out of the `zeroship` schema into the app's own schema,
// because that is the only schema `env.db` addresses; `TestPg::rewrite` maps the
// names, so the SQL in every accessor below is unchanged.

/// The three side-effect tables, in the app's OWN data schema, plus the runtime
/// role `env.db` speaks as.
///
/// WHY THE APP SCHEMA AND NOT `zeroship`. `env.db.collection("foo")` addresses
/// exactly one place: `"<app_id>"."foo"`
/// (`build_find_with_schema_and_unmask_and_soft_delete_with_dialect_and_limit_ceiling`
/// in crates/zeroship-schema/src/query.rs). There is no
/// cross-schema qualifier - an op-level `schema:` naming another schema is
/// refused - so the tables the step bodies write have to live there. Every
/// accessor below still spells them `zeroship.workflow_e2e_*`;
/// [`TestPg::rewrite`] maps that to the app schema, the same trick the workflow
/// journal tables already use.
///
/// WHY THE ROLE AND THE GRANTS. Every autocommit `env.db` statement opens a
/// transaction and runs `SET LOCAL ROLE "app_<app_id>_role"` first
/// (crates/zeroship-data-engine/src/auth/bootstrap.rs:200-207,
/// crates/zeroship-data-engine/src/exec.rs:522-542), so the effective privileges
/// are that role's, not `zeroship_worker`'s. In production the role, the
/// membership edge and the grants are all created by `migrated`'s apply
/// (crates/zeroship-migrate-server/src/apply.rs:1669-1712, :1638-1649). THIS HARNESS
/// DOES NOT RUN `migrated` - it deploys a `.zship` with `dev-provision` and
/// nothing else - so the statements below are that apply's runtime half,
/// reproduced. They are a deliberate mirror, not an invention: keep them in step
/// with `provision_runtime_app_role` if it changes. Without them the first
/// `env.db` call fails with `role "app_..._role" does not exist`, which
/// plugin-db reports as `schema_not_provisioned`
/// (crates/zeroship-data-core/src/error.rs:216-243).
///
/// The explicit column grants are re-issued on every call because this function
/// drops and recreates the tables.
async fn prepare_side_effect_table(pg: &TestPg) {
    let app_schema = quote_ident(&pg.app_id.to_string());
    let app_role_name = zeroship_core::database_role::per_app_role_name(&pg.app_id.to_string())
        .expect("workflow fixture app id must produce a valid PostgreSQL role name");
    let app_role = quote_ident(&app_role_name);
    // The seven platform system columns, verbatim from the DDL the platform
    // itself emits for a creator table
    // (crates/zeroship-schema/src/query.rs:208-219). `id` is TEXT because
    // plugin-db mints a typed_id string into it when the document omits one
    // (crates/zeroship-data-engine/src/crud/system_fields_pass.rs:244-249); a uuid
    // column would reject that. `deleted_at` is not optional either - `find` and
    // `count` add `AND deleted_at IS NULL` unless asked not to.
    //
    // `seq bigserial` is OURS, not the platform's. The accessors order by it.
    // `id` would very probably work (typed_id is UUIDv7 base62-encoded to a
    // fixed 22 chars, so lexical order is time order), but that is a property of
    // the id codec and this test has no business depending on it.
    const SYSTEM_COLUMNS: &str = "id text PRIMARY KEY, \
         created_at timestamptz NOT NULL DEFAULT now(), \
         updated_at timestamptz NOT NULL DEFAULT now(), \
         created_by text NULL, \
         updated_by text NULL, \
         version integer NOT NULL DEFAULT 1, \
         deleted_at timestamptz NULL, \
         seq bigserial NOT NULL";
    pg.batch_execute(&format!(
        "DO $dw07_role$ BEGIN \
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '__zeroship_app_role_template') THEN \
                EXECUTE 'CREATE ROLE \"__zeroship_app_role_template\" NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT'; \
            END IF; \
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{app_role_name}') THEN \
                EXECUTE 'CREATE ROLE {app_role} NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE \"__zeroship_app_role_template\"'; \
            END IF; \
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'zeroship_worker') THEN \
                GRANT {app_role} TO \"zeroship_worker\"; \
            END IF; \
         END $dw07_role$; \
         CREATE SCHEMA IF NOT EXISTS {app_schema}; \
         GRANT USAGE ON SCHEMA {app_schema} TO {app_role}; \
         DROP TABLE IF EXISTS zeroship.workflow_e2e_effect_attempts; \
         DROP TABLE IF EXISTS zeroship.workflow_e2e_effect_commits; \
         DROP TABLE IF EXISTS zeroship.workflow_e2e_side_effects; \
         CREATE TABLE zeroship.workflow_e2e_side_effects ( \
            {SYSTEM_COLUMNS}, \
            run_id text NOT NULL, \
            step_name text NOT NULL \
         ); \
         CREATE TABLE zeroship.workflow_e2e_effect_attempts ( \
            {SYSTEM_COLUMNS}, \
            run_id text NOT NULL, \
            step_name text NOT NULL, \
            idempotency_key text NOT NULL \
         ); \
         CREATE TABLE zeroship.workflow_e2e_effect_commits ( \
            {SYSTEM_COLUMNS}, \
            run_id text NOT NULL, \
            step_name text NOT NULL, \
            idempotency_key text NOT NULL UNIQUE \
         ); \
         GRANT \
            SELECT (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name), \
            INSERT (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name), \
            UPDATE (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name), \
            DELETE \
           ON TABLE {app_schema}.workflow_e2e_side_effects TO {app_role}; \
         GRANT \
            SELECT (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name, idempotency_key), \
            INSERT (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name, idempotency_key), \
            UPDATE (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name, idempotency_key), \
            DELETE \
           ON TABLE {app_schema}.workflow_e2e_effect_attempts TO {app_role}; \
         GRANT \
            SELECT (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name, idempotency_key), \
            INSERT (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name, idempotency_key), \
            UPDATE (id, created_at, updated_at, created_by, updated_by, version, deleted_at, seq, run_id, step_name, idempotency_key), \
            DELETE \
           ON TABLE {app_schema}.workflow_e2e_effect_commits TO {app_role}; \
         GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {app_schema} TO {app_role};",
    ))
    .await
    .expect("prepare side-effect table");
}

/// Double-quote a Postgres identifier. The two values this is used on are a
/// `Uuid` rendering and a name derived from it, so neither can carry a quote;
/// the escape is here so a future caller does not have to notice that.
fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn config(owner: &str) -> WorkflowEngineConfig {
    WorkflowEngineConfig {
        batch_apps: 4,
        per_app_fair_limit: 2,
        max_inflight_per_app: 8,
        max_inflight_dispatch: 8,
        claim_ttl_ms: 1_500,
        heartbeat_ms: 60_000,
        stuck_strike_limit: 3,
        max_child_depth: workflow_engine::DEFAULT_MAX_CHILD_DEPTH,
        max_live_descendants: workflow_engine::DEFAULT_MAX_LIVE_DESCENDANTS,
        max_start_many_batch: workflow_engine::DEFAULT_MAX_START_MANY_BATCH,
        journal_limits: Default::default(),
        owner_id: owner.to_string(),
    }
}

fn single_dispatch_config(owner: &str) -> WorkflowEngineConfig {
    let mut cfg = config(owner);
    cfg.batch_apps = 1;
    cfg.per_app_fair_limit = 1;
    cfg.max_inflight_per_app = 1;
    cfg.max_inflight_dispatch = 1;
    cfg
}

fn duplicate_dispatch_config(owner: &str) -> WorkflowEngineConfig {
    let mut cfg = single_dispatch_config(owner);
    cfg.max_inflight_dispatch = 2;
    cfg
}

struct PlanRuntimeLimitRestore {
    plan_id: String,
    runtime_limits: serde_json::Value,
}

async fn set_app_plan_runtime_limit(
    fx: &Fixture,
    key: &str,
    value: serde_json::Value,
) -> PlanRuntimeLimitRestore {
    let row = fx
        .pg
        .query_one(
            "SELECT p.id, p.runtime_limits_json \
               FROM zeroship.apps a \
               JOIN zeroship.plans p ON p.id = a.plan_id \
              WHERE a.id = $1",
            &[&fx.app_id],
        )
        .await
        .expect("load app plan runtime limits");
    let plan_id: String = row.get("id");
    let original: serde_json::Value = row.get("runtime_limits_json");
    let mut updated = original.clone();
    if !updated.is_object() {
        updated = serde_json::json!({});
    }
    updated[key] = value;
    fx.pg
        .execute(
            "UPDATE zeroship.plans SET runtime_limits_json = $1 WHERE id = $2",
            &[&updated, &plan_id],
        )
        .await
        .expect("set app plan runtime limit");
    PlanRuntimeLimitRestore {
        plan_id,
        runtime_limits: original,
    }
}

async fn restore_app_plan_runtime_limits(fx: &Fixture, restore: PlanRuntimeLimitRestore) {
    fx.pg
        .execute(
            "UPDATE zeroship.plans SET runtime_limits_json = $1 WHERE id = $2",
            &[&restore.runtime_limits, &restore.plan_id],
        )
        .await
        .expect("restore app plan runtime limits");
}

fn schedule_config(owner: &str) -> ScheduleSweepConfig {
    ScheduleSweepConfig {
        batch_size: 4,
        claim_ttl_ms: 1_500,
        backfill_hard_max: 8,
        owner_id: owner.to_string(),
    }
}

async fn seed_workflow_run(
    fx: &Fixture,
    workflow_name: &str,
    input: serde_json::Value,
) -> String {
    let run_id = zeroship_core::typed_id::new_workflow_run_id();
    let wake_at = Utc::now();
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_runs \
                (id, workflow_name, app_id, deploy_id, state, input, wake_at, started_at) \
             VALUES ($1, $2, $3, $4, 'queued', $5, $6, now())",
            &[
                &run_id,
                &workflow_name,
                &fx.app_id,
                &fx.deploy_id,
                &input,
                &wake_at,
            ],
        )
        .await
        .expect("seed workflow run");
    fx.scheduler_store
        .register_timer(&run_id, fx.app_id, wake_at)
        .await
        .expect("register seeded workflow timer");
    run_id
}

async fn seed_run(fx: &Fixture, label: &str) -> String {
    seed_workflow_run(
        fx,
        WORKFLOW_NAME,
        serde_json::json!({ "case": label }),
    )
    .await
}

async fn register_existing_run_timer(fx: &Fixture, run_id: &str) {
    let row = fx
        .pg
        .query_one(
            "SELECT app_id, state, wake_at, cancel_requested \
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
        if row.get::<_, bool>("cancel_requested") {
            fx.scheduler_store
                .ack_register_next(run_id, app_id, Utc::now())
                .await
                .expect("register cancelled workflow timer");
        } else {
            let wake_at = wake_at.expect("active workflow run should have wake_at for test register");
            fx.scheduler_store
                .register_timer(run_id, app_id, wake_at)
                .await
                .expect("register existing workflow timer");
        }
    }
}

fn bench_percentile_ms(values: &[Duration], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let idx = ((percentile * (sorted.len().saturating_sub(1) as f64)).round() as usize)
        .min(sorted.len() - 1);
    sorted[idx].as_secs_f64() * 1_000.0
}

fn bench_max_ms(values: &[Duration]) -> f64 {
    values
        .iter()
        .copied()
        .max()
        .map_or(0.0, |value| value.as_secs_f64() * 1_000.0)
}

fn machine_summary() -> String {
    let cpus = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(0);
    let mem_gib = fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let rest = line.strip_prefix("MemTotal:")?;
                let kib = rest.split_whitespace().next()?.parse::<f64>().ok()?;
                Some(kib / 1024.0 / 1024.0)
            })
        });
    match mem_gib {
        Some(mem_gib) => format!(
            "{} {} logical_cpus={} mem_gib={:.1}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            cpus,
            mem_gib
        ),
        None => format!(
            "{} {} logical_cpus={}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            cpus
        ),
    }
}

async fn bench_counts(fx: &Fixture, run_ids: &[String]) -> (i64, i64) {
    let row = fx
        .pg
        .query_one(
            "SELECT \
                (SELECT COUNT(*)::bigint \
                   FROM zeroship.workflow_runs \
                  WHERE id = ANY($1) AND state = 'completed') AS completed, \
                (SELECT COUNT(*)::bigint \
                   FROM zeroship.workflow_steps \
                  WHERE run_id = ANY($1)) AS checkpoints",
            &[&run_ids],
        )
        .await
        .expect("load DW-23 bench counts");
    (row.get("completed"), row.get("checkpoints"))
}

#[test]
#[ignore = "DW-23 load bench: requires tests/e2e_durable_workflows.sh --bench"]
fn dw23_workflow_engine_load_bench() {
    let rt = compio::runtime::Runtime::new().expect("compio runtime");
    rt.block_on(async {
        require_dw_e2e_fleet();

        let db_url = required_env("a test database", zeroship_core::config::test_database_url_opt());
        let gateway_url = required_env("ZEROSHIP_DW_E2E_GATEWAY_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_GATEWAY_URL"));
        let app_id: Uuid = required_env("ZEROSHIP_DW_E2E_APP_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_APP_ID"))
            .parse()
            .expect("ZEROSHIP_DW_E2E_APP_ID uuid");
        let deploy_id = required_env("ZEROSHIP_DW_E2E_DEPLOY_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_DEPLOY_ID"));
        let fx = build_fixture(&db_url, &gateway_url, app_id, deploy_id).await;
        set_dispatch_paused(&fx, false).await;

        // The parameters of the ONE recorded run, as literals:
        // docs/archive/benchmarks/2026-07-07-durable-workflows-load.md:9.
        // They used to be ZEROSHIP_DW23_BENCH_{RUNS,CONCURRENCY,MAX_SECS}, which
        // nothing set, and whose max_secs DEFAULT was 60 while the archived run
        // passed 90 - so the archived numbers were not reproducible from the
        // defaults. Change a parameter by editing this line and re-archiving.
        let run_count = 128usize;
        let concurrency = 32usize;
        let max_secs = 90usize;
        let mut run_ids = Vec::with_capacity(run_count);
        for idx in 0..run_count {
            run_ids
                .push(seed_workflow_run(
                    &fx,
                    BENCH_WORKFLOW_NAME,
                    serde_json::json!({ "marker": idx }),
                )
                .await);
        }

        let dispatcher = Arc::new(TimingGatewayDispatcher::new(gateway_url));
        let cfg = WorkflowEngineConfig {
            batch_apps: 4,
            per_app_fair_limit: i64::try_from(concurrency).unwrap_or(i64::MAX),
            max_inflight_per_app: i64::try_from(concurrency).unwrap_or(i64::MAX),
            max_inflight_dispatch: concurrency,
            claim_ttl_ms: 60_000,
            heartbeat_ms: 60_000,
            stuck_strike_limit: 3,
            max_child_depth: workflow_engine::DEFAULT_MAX_CHILD_DEPTH,
            max_live_descendants: workflow_engine::DEFAULT_MAX_LIVE_DESCENDANTS,
            max_start_many_batch: workflow_engine::DEFAULT_MAX_START_MANY_BATCH,
            journal_limits: Default::default(),
            owner_id: format!("dw23-bench-{}", Uuid::new_v4().simple()),
        };

        let started = Instant::now();
        let max_duration = Duration::from_secs(u64::try_from(max_secs).unwrap_or(u64::MAX));
        let mut total_claimed = 0usize;
        let mut checkpoint_elapsed = None;
        let final_counts = loop {
            let claimed = workflow_engine::fire_once(
                &fx.scheduler_store,
                &fx.state,
                Arc::clone(&dispatcher),
                cfg.clone(),
            )
            .await
            .expect("DW-23 bench workflow tick");
            total_claimed += claimed;

            let counts = bench_counts(&fx, &run_ids).await;
            if checkpoint_elapsed.is_none()
                && counts.1 >= i64::try_from(run_count).unwrap_or(i64::MAX)
            {
                checkpoint_elapsed = Some(started.elapsed());
            }
            if counts.0 >= i64::try_from(run_count).unwrap_or(i64::MAX) {
                break counts;
            }
            if started.elapsed() > max_duration {
                panic!(
                    "DW-23 bench exceeded {:?}: completed={} checkpoints={} total_claimed={}",
                    max_duration, counts.0, counts.1, total_claimed
                );
            }
            let sleep_ms = if claimed == 0 { 10 } else { 1 };
            compio::time::sleep(Duration::from_millis(sleep_ms)).await;
        };

        let elapsed = started.elapsed();
        let checkpoint_elapsed = checkpoint_elapsed.unwrap_or(elapsed);
        let latencies = dispatcher.latencies();
        assert_eq!(
            final_counts.0,
            i64::try_from(run_count).unwrap(),
            "all bench runs must complete"
        );
        assert_eq!(
            final_counts.1,
            i64::try_from(run_count).unwrap(),
            "BenchWorkflow should write one checkpoint per run"
        );
        assert!(
            !latencies.is_empty(),
            "real gateway/worker dispatch path should record latencies"
        );

        let elapsed_secs = elapsed.as_secs_f64();
        let checkpoint_secs = checkpoint_elapsed.as_secs_f64();
        println!("DW23_BENCH_RESULT machine=\"{}\"", machine_summary());
        println!(
            "DW23_BENCH_RESULT runs={} concurrency={} elapsed_ms={:.3} total_claims={} claims_per_sec={:.3} completed_runs_per_sec={:.3}",
            run_count,
            concurrency,
            elapsed_secs * 1_000.0,
            total_claimed,
            total_claimed as f64 / elapsed_secs,
            run_count as f64 / elapsed_secs
        );
        println!(
            "DW23_BENCH_RESULT checkpoints={} checkpoint_elapsed_ms={:.3} checkpoints_per_sec={:.3}",
            final_counts.1,
            checkpoint_secs * 1_000.0,
            final_counts.1 as f64 / checkpoint_secs
        );
        println!(
            "DW23_BENCH_RESULT replay_latency_ms count={} p50={:.3} p95={:.3} p99={:.3} max={:.3}",
            latencies.len(),
            bench_percentile_ms(&latencies, 0.50),
            bench_percentile_ms(&latencies, 0.95),
            bench_percentile_ms(&latencies, 0.99),
            bench_max_ms(&latencies)
        );

        // Teardown: this test hand-builds its own compio runtime (`rt`) rather
        // than using `#[compio::test]`, but the same rule applies - the
        // fixture holds the sole Postgres connection, and `rt` is torn down
        // the instant this block returns, so the drop and drain must happen
        // while `rt` is still alive to drive them.
        drop(fx);
        common::drain_pg().await;
    });
}

async fn seed_signal_run(
    fx: &Fixture,
    label: &str,
    timeout: &str,
    max_signal_age: Option<&str>,
) -> String {
    let mut input = serde_json::json!({
        "case": label,
        "timeout": timeout,
    });
    if let Some(max_signal_age) = max_signal_age {
        input["maxSignalAge"] = serde_json::Value::String(max_signal_age.to_string());
    }
    seed_workflow_run(fx, SIGNAL_WORKFLOW_NAME, input).await
}

async fn seed_topic_signal_run(fx: &Fixture, label: &str, topic: &str) -> String {
    seed_workflow_run(
        fx,
        TOPIC_SIGNAL_WORKFLOW_NAME,
        serde_json::json!({ "case": label, "topic": topic }),
    )
    .await
}

async fn set_dispatch_paused(fx: &Fixture, paused: bool) {
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_rollout_config \
                (id, dispatch_paused, ingress_disabled, updated_by) \
             VALUES ('global', $1, false, 'dw24-e2e') \
             ON CONFLICT (id) DO UPDATE SET \
                dispatch_paused = EXCLUDED.dispatch_paused, \
                updated_at = now(), \
                updated_by = EXCLUDED.updated_by",
            &[&paused],
        )
        .await
        .expect("set workflow dispatch pause switch");
}

async fn set_ingress_disabled(fx: &Fixture, disabled: bool) {
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_rollout_config \
                (id, dispatch_paused, ingress_disabled, updated_by) \
             VALUES ('global', false, $1, 'dw24-e2e') \
             ON CONFLICT (id) DO UPDATE SET \
                ingress_disabled = EXCLUDED.ingress_disabled, \
                updated_at = now(), \
                updated_by = EXCLUDED.updated_by",
            &[&disabled],
        )
        .await
        .expect("set workflow ingress disable switch");
}

async fn seed_completed_step(
    fx: &Fixture,
    run_id: &str,
    name: &str,
    kind: &str,
    output: serde_json::Value,
) {
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, output, output_kind, \
                 batch_id, batch_width, finished_at) \
             VALUES ($1, 0, $2, 0, $3, 'completed', $4, 'inline', 'wfd_seed', 1, now())",
            &[&run_id, &name, &kind, &output],
        )
        .await
        .expect("seed completed workflow step");
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET next_ordinal = GREATEST(next_ordinal, 1) \
              WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("advance seeded run next_ordinal");
}

async fn seed_compensable_completed_step(
    fx: &Fixture,
    run_id: &str,
    name: &str,
    output: serde_json::Value,
) {
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_steps \
                (run_id, ordinal, name, name_occurrence, kind, state, output, output_kind, \
                 batch_id, batch_width, finished_at, compensation_state, compensation_max_attempts) \
             VALUES ($1, 0, $2, 0, 'run', 'completed', $3, 'inline', 'wfd_seed', 1, now(), 'pending', 1)",
            &[&run_id, &name, &output],
        )
        .await
        .expect("seed compensable completed workflow step");
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_runs \
                SET next_ordinal = GREATEST(next_ordinal, 1) \
              WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("advance compensable seeded run next_ordinal");
}

async fn run_state(
    pg: &TestPg,
    run_id: &str,
) -> (String, Option<DateTime<Utc>>, Option<String>, Option<String>) {
    let rows = pg
        .query(
            "SELECT state, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load run state");
    (
        rows[0].get("state"),
        rows[0].get("wake_at"),
        rows[0].get("claimed_by"),
        rows[0].get("dispatch_nonce"),
    )
}

async fn workflow_step_count(pg: &TestPg, run_id: &str) -> i64 {
    pg.query_one(
        "SELECT COUNT(*)::bigint AS n \
           FROM zeroship.workflow_steps \
          WHERE run_id = $1",
        &[&run_id],
    )
    .await
    .expect("count workflow steps")
    .get("n")
}

async fn wait_for_any_state(
    fx: &Fixture,
    run_id: &str,
    expected: &[&str],
) -> (String, Option<DateTime<Utc>>) {
    for _ in 0..200 {
        let (state, wake_at, _, _) = run_state(&fx.pg, run_id).await;
        if expected.iter().any(|candidate| state == *candidate) {
            return (state, wake_at);
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} did not reach any of {expected:?}");
}

async fn wait_for_step_count(fx: &Fixture, run_id: &str, expected: usize) {
    for _ in 0..400 {
        if step_rows(fx, run_id).await.len() == expected {
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "run {run_id} did not reach {expected} step rows: {}",
        run_debug(fx, run_id).await
    );
}

async fn wait_for_dispatch_count(dispatcher: &CountingDispatcher, expected: usize) {
    for _ in 0..120 {
        if dispatcher.count() == expected {
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "dispatcher reached {} calls, expected {expected}",
        dispatcher.count()
    );
}

async fn wait_for_captured_dispatch_count(dispatcher: &CapturingDispatcher, expected: usize) {
    for _ in 0..400 {
        if dispatcher.count() == expected {
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "capturing dispatcher reached {} calls, expected {expected}",
        dispatcher.count()
    );
}

async fn scheduler_counts(fx: &Fixture, run_id: &str) -> (i64, i64) {
    let row = fx
        .pg
        .query_one(
            "SELECT \
                (SELECT COUNT(*)::bigint FROM zeroship.workflow_scheduler_timers WHERE run_id = $1) AS timers, \
                (SELECT COUNT(*)::bigint FROM zeroship.workflow_scheduler_inflight WHERE run_id = $1) AS inflight",
            &[&run_id],
        )
        .await
        .expect("load scheduler counts");
    (row.get("timers"), row.get("inflight"))
}

/// Poll the scheduler store until a run's (timers, inflight) counts converge to
/// `expected`. The scheduler store is a derived, eventually-consistent copy: a
/// terminal run's de-register (`ack_terminal`) runs in the detached dispatch
/// task after the journal commit is visible, so tests must observe the settled
/// state rather than assume synchronous de-registration.
async fn wait_for_scheduler_counts(fx: &Fixture, run_id: &str, expected: (i64, i64)) {
    for _ in 0..200 {
        if scheduler_counts(fx, run_id).await == expected {
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "scheduler counts for {run_id} did not converge to {expected:?}; last = {:?}",
        scheduler_counts(fx, run_id).await
    );
}

async fn scheduler_timer_wake_at(fx: &Fixture, run_id: &str) -> Option<DateTime<Utc>> {
    fx.pg
        .query(
            "SELECT wake_at FROM zeroship.workflow_scheduler_timers WHERE run_id = $1",
            &[&run_id],
        )
        .await
        .expect("load scheduler timer")
        .first()
        .map(|row| row.get("wake_at"))
}

async fn wait_for_scheduler_timer(fx: &Fixture, run_id: &str) -> DateTime<Utc> {
    for _ in 0..120 {
        if let Some(wake_at) = scheduler_timer_wake_at(fx, run_id).await {
            return wake_at;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "scheduler timer for {run_id} did not appear: {}",
        run_debug(fx, run_id).await
    );
}

async fn force_inflight_deadline_elapsed(fx: &Fixture, run_id: &str) -> bool {
    let deadline = Utc::now() - ChronoDuration::milliseconds(1);
    let updated = fx
        .pg
        .execute(
            "UPDATE zeroship.workflow_scheduler_inflight SET deadline = $2 WHERE run_id = $1",
            &[&run_id, &deadline],
        )
        .await
        .expect("force inflight deadline elapsed");
    updated == 1
}

async fn force_run_due(fx: &Fixture, run_id: &str) -> DateTime<Utc> {
    let wake_at = Utc::now() - ChronoDuration::milliseconds(10);
    let updated = fx
        .pg
        .execute(
            "UPDATE zeroship.workflow_runs SET wake_at = $2 WHERE id = $1",
            &[&run_id, &wake_at],
        )
        .await
        .expect("force workflow run due");
    assert_eq!(updated, 1, "expected one workflow run for {run_id}");
    fx.scheduler_store
        .register_timer(run_id, fx.app_id, wake_at)
        .await
        .expect("register due workflow timer");
    wake_at
}

async fn force_claim_lease_elapsed(fx: &Fixture, run_id: &str) {
    let lease_expires = Utc::now() - ChronoDuration::milliseconds(1);
    let updated = fx
        .pg
        .execute(
            "UPDATE zeroship.workflow_runs SET lease_expires = $2 WHERE id = $1",
            &[&run_id, &lease_expires],
    )
    .await
    .expect("force workflow claim lease elapsed");
    assert_eq!(updated, 1, "expected one workflow run for {run_id}");
}

async fn reap_unclaimed_inflight_retry<D>(
    fx: &Fixture,
    dispatcher: &Arc<D>,
    cfg: &WorkflowEngineConfig,
    run_id: &str,
    attempt: usize,
    claimed_by: &Option<String>,
) where
    D: StepDispatcher + 'static,
{
    if claimed_by.is_some() || attempt < 8 || !attempt.is_multiple_of(8) {
        return;
    }
    if scheduler_counts(fx, run_id).await != (0, 1) {
        return;
    }
    if !force_inflight_deadline_elapsed(fx, run_id).await {
        return;
    }
    let _ = workflow_engine::reap_lapsed_inflight_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(dispatcher),
        cfg.clone(),
        1,
    )
    .await
    .expect("reap unclaimed inflight retry");
}

fn retryable_scheduler_tick_error(err: &RegistryError) -> bool {
    matches!(err, RegistryError::Database(message) if message.contains("workflow scheduler: db error"))
}

async fn workflow_tick<D>(
    fx: &Fixture,
    dispatcher: &Arc<D>,
    cfg: &WorkflowEngineConfig,
    label: &str,
) -> usize
where
    D: StepDispatcher + 'static,
{
    let mut last_error = None;
    for retry in 0..20 {
        match workflow_engine::fire_once(
            &fx.scheduler_store,
            &fx.state,
            Arc::clone(dispatcher),
            cfg.clone(),
        )
        .await
        {
            Ok(claimed) => return claimed,
            Err(err) if retryable_scheduler_tick_error(&err) => {
                last_error = Some(err);
                compio::time::sleep(Duration::from_millis(25 + retry * 5)).await;
            }
            Err(err) => panic!("{label} tick failed: {err}"),
        }
    }
    panic!(
        "{label} tick kept failing with retryable scheduler DB errors; last={last_error:?}"
    );
}

async fn drive_until_completed<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    run_id: &str,
) where
    D: StepDispatcher + 'static,
{
    for attempt in 0..260 {
        workflow_tick(fx, &dispatcher, &cfg, "workflow").await;
        let (state, _, claimed_by, _) = run_state(&fx.pg, run_id).await;
        if state == "completed" {
            return;
        }
        reap_unclaimed_inflight_retry(fx, &dispatcher, &cfg, run_id, attempt, &claimed_by).await;
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    let rows = fx
        .pg
        .query(
            "SELECT state, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load stuck run");
    panic!(
        "run {run_id} did not complete at {:?}: state={:?} wake_at={:?} claimed_by={:?} nonce={:?}; {}",
        Utc::now(),
        rows[0].get::<_, String>("state"),
        rows[0].get::<_, Option<DateTime<Utc>>>("wake_at"),
        rows[0].get::<_, Option<String>>("claimed_by"),
        rows[0].get::<_, Option<String>>("dispatch_nonce"),
        run_debug(fx, run_id).await
    );
}

async fn rollout_switch_drill(
    fx: &Fixture,
    control_url: &str,
    gateway_url: &str,
    dispatcher: Arc<GatewayStepDispatcher>,
) {
    let enabled_run = seed_run(fx, "dw24-enabled").await;
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("dw24-enabled-claim"),
    )
    .await
    .expect("DW24 enabled claim tick");
    assert_eq!(claimed, 1, "enabled workflow run should dispatch once");
    drive_until_completed(
        fx,
        Arc::clone(&dispatcher),
        config("dw24-enabled-complete"),
        &enabled_run,
    )
    .await;

    let parked_run = seed_run(fx, "dw24-dispatch-paused").await;
    set_dispatch_paused(fx, true).await;
    let paused_claims = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&dispatcher),
        config("dw24-dispatch-paused"),
    )
    .await
    .expect("DW24 dispatch-pause tick");
    set_dispatch_paused(fx, false).await;
    assert_eq!(paused_claims, 0, "dispatch pause must stop claiming");
    let (state, wake_at, claimed_by, dispatch_nonce) = run_state(&fx.pg, &parked_run).await;
    assert_eq!(state, "queued");
    assert!(
        wake_at.is_some(),
        "paused due run should stay durably parked with its wake_at"
    );
    assert_eq!(claimed_by, None);
    assert_eq!(dispatch_nonce, None);
    drive_until_completed(
        fx,
        Arc::clone(&dispatcher),
        config("dw24-dispatch-resume"),
        &parked_run,
    )
    .await;

    let signal_run = seed_signal_run(fx, "dw24-ingress-disabled", "PT30S", None).await;
    drive_until_waiting(
        fx,
        Arc::clone(&dispatcher),
        config("dw24-ingress-park"),
        &signal_run,
    )
    .await;
    let token = create_run_signal_token(control_url, fx.app_id, &signal_run, "PT30S").await;
    set_ingress_disabled(fx, true).await;
    let (public_status, public_body) = post_public_signal(
        gateway_url,
        &token,
        serde_json::json!({"ok": true, "source": "dw24-public"}),
    )
    .await;
    let point_signal = post_signal(
        control_url,
        fx.app_id,
        &signal_run,
        serde_json::json!({"ok": true, "source": "dw24-point-to-point"}),
    )
    .await;
    set_ingress_disabled(fx, false).await;
    assert_eq!(
        public_status, 503,
        "public signal ingress should be disabled: {public_body}"
    );
    assert!(
        point_signal["id"].as_str().is_some_and(|id| id.starts_with("sig_")),
        "app-scoped run.signal should still work while public ingress is disabled"
    );
    register_existing_run_timer(fx, &signal_run).await;
    drive_until_completed(
        fx,
        Arc::clone(&dispatcher),
        config("dw24-ingress-resume"),
        &signal_run,
    )
    .await;
    assert_eq!(
        run_output(fx, &signal_run).await["signal"]["payload"],
        serde_json::json!({"ok": true, "source": "dw24-point-to-point"})
    );
}

async fn drive_until_failed<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    run_id: &str,
    label: &str,
) -> serde_json::Value
where
    D: StepDispatcher + 'static,
{
    for attempt in 0..120 {
        workflow_tick(fx, &dispatcher, &cfg, label).await;
        let (state, _, claimed_by, _) = run_state(&fx.pg, run_id).await;
        if state == "failed" {
            let row = fx
                .pg
                .query_one(
                    "SELECT error FROM zeroship.workflow_runs WHERE id = $1",
                    &[&run_id],
                )
                .await
                .unwrap_or_else(|e| panic!("load {label} failure: {e}"));
            return row.get("error");
        }
        if state == "completed" {
            panic!(
                "{label} workflow completed instead of failing: {}",
                run_debug(fx, run_id).await
            );
        }
        reap_unclaimed_inflight_retry(fx, &dispatcher, &cfg, run_id, attempt, &claimed_by).await;
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{label} workflow did not fail: {}",
        run_debug(fx, run_id).await
    );
}

async fn drive_until_cancelled<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    run_id: &str,
    label: &str,
) where
    D: StepDispatcher + 'static,
{
    for attempt in 0..160 {
        workflow_tick(fx, &dispatcher, &cfg, label).await;
        let (state, _, claimed_by, _) = run_state(&fx.pg, run_id).await;
        if state == "cancelled" {
            return;
        }
        if state == "failed" || state == "completed" {
            panic!(
                "{label} workflow reached {state} instead of cancelled: {}",
                run_debug(fx, run_id).await
            );
        }
        reap_unclaimed_inflight_retry(fx, &dispatcher, &cfg, run_id, attempt, &claimed_by).await;
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{label} workflow did not cancel: {}",
        run_debug(fx, run_id).await
    );
}

async fn drive_until_sleeping<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    run_id: &str,
    label: &str,
) where
    D: StepDispatcher + 'static,
{
    for attempt in 0..160 {
        workflow_tick(fx, &dispatcher, &cfg, label).await;
        let (state, _, claimed_by, _) = run_state(&fx.pg, run_id).await;
        if state == "sleeping" {
            return;
        }
        if state == "failed" || state == "completed" || state == "cancelled" {
            panic!(
                "{label} reached terminal {state} before sleeping: {}",
                run_debug(fx, run_id).await
            );
        }
        reap_unclaimed_inflight_retry(fx, &dispatcher, &cfg, run_id, attempt, &claimed_by).await;
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{label} workflow did not sleep: {}",
        run_debug(fx, run_id).await
    );
}

async fn drive_until_compensating<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    run_id: &str,
    label: &str,
) where
    D: StepDispatcher + 'static,
{
    for attempt in 0..320 {
        let (state, _, claimed_by, _) = run_state(&fx.pg, run_id).await;
        // Only act on a SETTLED run (no in-flight dispatch). Ticking while a
        // dispatch is in flight, or immediately after the run enters compensating,
        // would claim the compensating run and dispatch its FIRST compensator here
        // — over-driving past the point the caller wants (its crash-drop tick must
        // be the first compensator dispatch). Waiting for claimed_by to clear after
        // each tick makes us observe the stable post-apply state and return the
        // moment c's failure lands the run in compensating, before any compensator.
        if claimed_by.is_none() {
            if state == "compensating" {
                return;
            }
            if state == "failed" || state == "completed" || state == "cancelled" {
                panic!(
                    "{label} reached terminal {state} before compensating: {}",
                    run_debug(fx, run_id).await
                );
            }
            workflow_tick(fx, &dispatcher, &cfg, label).await;
            reap_unclaimed_inflight_retry(fx, &dispatcher, &cfg, run_id, attempt, &claimed_by).await;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "{label} workflow did not enter compensating: {}",
        run_debug(fx, run_id).await
    );
}

async fn drive_until_waiting<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    run_id: &str,
) where
    D: StepDispatcher + 'static,
{
    for attempt in 0..180 {
        workflow_tick(fx, &dispatcher, &cfg, "workflow").await;
        let (state, _, claimed_by, _) = run_state(&fx.pg, run_id).await;
        if state == "waiting" {
            return;
        }
        if state == "failed" || state == "completed" {
            panic!(
                "run {run_id} reached {state} before waiting: {}",
                run_debug(fx, run_id).await
            );
        }
        reap_unclaimed_inflight_retry(fx, &dispatcher, &cfg, run_id, attempt, &claimed_by).await;
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "run {run_id} did not park waiting: {}",
        run_debug(fx, run_id).await
    );
}

async fn step_rows(fx: &Fixture, run_id: &str) -> Vec<(i32, String, String, String)> {
    fx.pg
        .query(
            "SELECT ordinal, name, kind, state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("load step rows")
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

async fn assert_expected_steps(fx: &Fixture, run_id: &str) {
    let rows = step_rows(fx, run_id).await;
    assert_eq!(
        rows,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (1, "sleep".to_string(), "sleep".to_string(), "completed".to_string()),
            (2, "b".to_string(), "run".to_string(), "completed".to_string()),
        ],
        "workflow_steps must contain exactly a, sleep, b once"
    );
    let dupes = fx
        .pg
        .query(
            "SELECT ordinal, COUNT(*)::bigint AS n \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 \
              GROUP BY ordinal \
             HAVING COUNT(*) > 1",
            &[&run_id],
        )
        .await
        .expect("check duplicate step rows");
    assert!(dupes.is_empty(), "duplicate workflow step rows: {dupes:?}");
}

async fn assert_signal_success_steps(fx: &Fixture, run_id: &str) {
    let rows = step_rows(fx, run_id).await;
    assert_eq!(
        rows,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (1, "go".to_string(), "wait_signal".to_string(), "completed".to_string()),
            (2, "b".to_string(), "run".to_string(), "completed".to_string()),
        ],
        "workflow_steps must contain exactly a, go wait, b once"
    );
}

async fn assert_signal_timeout_steps(fx: &Fixture, run_id: &str) {
    let rows = step_rows(fx, run_id).await;
    assert_eq!(
        rows,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (1, "go".to_string(), "wait_signal".to_string(), "failed".to_string()),
            (2, "timeout".to_string(), "run".to_string(), "completed".to_string()),
        ],
        "workflow_steps must contain exactly a, failed go wait, timeout once"
    );
}

async fn assert_topic_success_steps(fx: &Fixture, run_id: &str) {
    let rows = step_rows(fx, run_id).await;
    assert_eq!(
        rows,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (
                1,
                "topic-go".to_string(),
                "wait_signal".to_string(),
                "completed".to_string(),
            ),
            (2, "b".to_string(), "run".to_string(), "completed".to_string()),
        ],
        "topic workflow must contain exactly a, topic wait, b once"
    );
}

async fn assert_concurrent_steps(fx: &Fixture, run_id: &str) {
    let rows = step_rows(fx, run_id).await;
    assert_eq!(
        rows,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (1, "b".to_string(), "run".to_string(), "completed".to_string()),
            (2, "c".to_string(), "run".to_string(), "completed".to_string()),
            (3, "final".to_string(), "run".to_string(), "completed".to_string()),
        ],
        "concurrent workflow must checkpoint a, b, c together, then final"
    );
}

async fn assert_side_effect_steps(fx: &Fixture, run_id: &str) {
    let rows = step_rows(fx, run_id).await;
    assert_eq!(
        rows,
        vec![
            (
                0,
                "v".to_string(),
                "sideEffect".to_string(),
                "completed".to_string(),
            ),
            (
                1,
                "after".to_string(),
                "run".to_string(),
                "completed".to_string(),
            ),
        ],
        "sideEffect workflow must checkpoint the frozen value once, then the real run step"
    );
    let row = fx
        .pg
        .query_one(
            "SELECT output \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = 0",
            &[&run_id],
        )
        .await
        .expect("load sideEffect output");
    let output: serde_json::Value = row.get("output");
    assert_eq!(output["step"], "v");
    // `bump` returns a PRE-insert count - it counts, then inserts - so the first
    // "v" bump answers 0. (It read 0 for the same reason under the old
    // side-effect server, whose data-modifying CTE could not see its own INSERT.)
    // This asserts the sideEffect journaled bump's real return value faithfully;
    // that the fn ran exactly ONCE (memoized on replay) is proven by
    // side_counts["v"] == 1 below.
    assert_eq!(output["count"], 0);
}

async fn assert_signal_wait_parked(
    fx: &Fixture,
    run_id: &str,
    max_signal_age_ms: Option<i64>,
) -> DateTime<Utc> {
    let rows = fx
        .pg
        .query(
            "SELECT r.state AS run_state, r.wake_at AS run_wake_at, r.waiting_step_key, \
                    s.state AS step_state, s.wake_at AS step_wake_at, \
                    s.signal_type, s.max_signal_age_ms \
               FROM zeroship.workflow_runs r \
               JOIN zeroship.workflow_steps s ON s.run_id = r.id AND s.ordinal = 1 \
              WHERE r.id = $1",
            &[&run_id],
        )
        .await
        .expect("load parked wait");
    assert_eq!(rows.len(), 1, "missing wait step for run {run_id}");
    assert_eq!(rows[0].get::<_, String>("run_state"), "waiting");
    assert_eq!(rows[0].get::<_, String>("step_state"), "running");
    assert_eq!(rows[0].get::<_, Option<String>>("signal_type").as_deref(), Some("go"));
    assert_eq!(
        rows[0].get::<_, Option<i64>>("max_signal_age_ms"),
        max_signal_age_ms
    );
    let key: Option<String> = rows[0].get("waiting_step_key");
    assert!(
        key.as_deref()
            .is_some_and(|key| key == "wait:1:go:go" || key.starts_with("wait:1:go:go:")),
        "waiting_step_key should encode the go wait, got {key:?}"
    );
    let run_wake_at: DateTime<Utc> = rows[0]
        .get::<_, Option<DateTime<Utc>>>("run_wake_at")
        .expect("run wake_at");
    let step_wake_at: DateTime<Utc> = rows[0]
        .get::<_, Option<DateTime<Utc>>>("step_wake_at")
        .expect("step wake_at");
    assert!(
        run_wake_at
            .signed_duration_since(step_wake_at)
            .num_milliseconds()
            .abs()
            <= 1,
        "run wake_at should mirror wait deadline: run={run_wake_at:?} step={step_wake_at:?}"
    );
    assert!(
        step_wake_at > Utc::now(),
        "wait deadline should be absolute and in the future when parked"
    );
    step_wake_at
}

async fn side_counts(fx: &Fixture, run_id: &str) -> BTreeMap<String, i64> {
    fx.pg
        .query(
            "SELECT step_name, COUNT(*)::bigint AS n \
               FROM zeroship.workflow_e2e_side_effects \
              WHERE run_id = $1 \
              GROUP BY step_name",
            &[&run_id],
        )
        .await
        .expect("load side counts")
        .into_iter()
        .map(|row| (row.get("step_name"), row.get("n")))
        .collect()
}

async fn ordered_side_effect_steps(fx: &Fixture, run_id: &str) -> Vec<String> {
    fx.pg
        .query(
            "SELECT step_name \
               FROM zeroship.workflow_e2e_side_effects \
              WHERE run_id = $1 \
              ORDER BY seq",
            &[&run_id],
        )
        .await
        .expect("load side effect order")
        .into_iter()
        .map(|row| row.get("step_name"))
        .collect()
}

async fn effect_attempt_counts(fx: &Fixture, run_id: &str) -> BTreeMap<String, i64> {
    fx.pg
        .query(
            "SELECT step_name, COUNT(*)::bigint AS n \
               FROM zeroship.workflow_e2e_effect_attempts \
              WHERE run_id = $1 \
              GROUP BY step_name",
            &[&run_id],
        )
        .await
        .expect("load compensation attempt counts")
        .into_iter()
        .map(|row| (row.get("step_name"), row.get("n")))
        .collect()
}

async fn effect_commit_counts(fx: &Fixture, run_id: &str) -> BTreeMap<String, i64> {
    fx.pg
        .query(
            "SELECT step_name, COUNT(*)::bigint AS n \
               FROM zeroship.workflow_e2e_effect_commits \
              WHERE run_id = $1 \
              GROUP BY step_name",
            &[&run_id],
        )
        .await
        .expect("load compensation commit counts")
        .into_iter()
        .map(|row| (row.get("step_name"), row.get("n")))
        .collect()
}

async fn ordered_commit_steps(fx: &Fixture, run_id: &str) -> Vec<String> {
    fx.pg
        .query(
            "SELECT step_name \
               FROM zeroship.workflow_e2e_effect_commits \
              WHERE run_id = $1 \
              ORDER BY seq",
            &[&run_id],
        )
        .await
        .expect("load compensation commit order")
        .into_iter()
        .map(|row| row.get("step_name"))
        .collect()
}

async fn compensation_step_rows(
    fx: &Fixture,
    run_id: &str,
) -> Vec<(i32, String, Option<String>, i32)> {
    fx.pg
        .query(
            "SELECT ordinal, name, compensation_state, compensation_attempt \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND compensation_state IS NOT NULL \
              ORDER BY ordinal",
            &[&run_id],
        )
        .await
        .expect("load compensation step rows")
        .into_iter()
        .map(|row| {
            (
                row.get("ordinal"),
                row.get("name"),
                row.get("compensation_state"),
                row.get("compensation_attempt"),
            )
        })
        .collect()
}

async fn run_compensation_status(
    fx: &Fixture,
    run_id: &str,
) -> (String, Option<String>, Option<String>, Option<serde_json::Value>) {
    let row = fx
        .pg
        .query_one(
            "SELECT state, compensation_target, compensation_outcome, error \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load compensation run status");
    (
        row.get("state"),
        row.get("compensation_target"),
        row.get("compensation_outcome"),
        row.get("error"),
    )
}

async fn run_output(fx: &Fixture, run_id: &str) -> serde_json::Value {
    let rows = fx
        .pg
        .query(
            "SELECT output FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("load run output");
    rows[0]
        .get::<_, Option<serde_json::Value>>("output")
        .unwrap_or(serde_json::Value::Null)
}

async fn activate_redeploy_with_current_manifest(fx: &Fixture) -> (String, String) {
    let manifest_raw = fx
        .pg
        .query_one(
            "SELECT manifest_json FROM zeroship.apps WHERE id = $1",
            &[&fx.app_id],
        )
        .await
        .expect("load current app manifest")
        .get::<_, Option<String>>("manifest_json")
        .expect("current manifest json");
    let redeploy_hash = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let mut manifest: serde_json::Value =
        serde_json::from_str(&manifest_raw).expect("manifest json parses");
    manifest["deploy_hash"] = serde_json::Value::String(redeploy_hash.clone());
    let redeploy_manifest = serde_json::to_string(&manifest).expect("serialize redeploy manifest");
    fx.state
        .blob_store
        .put_manifest(&fx.app_id, &redeploy_hash, redeploy_manifest.as_bytes())
        .await
        .expect("write redeploy manifest blob");
    let updated = fx
        .state
        .registry
        .set_deploy_with_manifest(&fx.app_id, &redeploy_hash, &redeploy_manifest, None)
        .await
        .expect("activate redeploy manifest");
    assert!(updated, "redeploy should update app");
    let deploy_id: String = fx
        .pg
        .query_one(
            "SELECT id \
               FROM zeroship.app_deploys \
              WHERE app_id = $1 AND deploy_hash = $2",
            &[&fx.app_id, &redeploy_hash],
        )
        .await
        .expect("load redeploy id")
        .get("id");
    (redeploy_hash, deploy_id)
}

async fn wait_for_active_deploy(fx: &Fixture, expected_hash: &str, expected_deploy_id: &str) {
    for _ in 0..400 {
        let app_hash: Option<String> = fx
            .pg
            .query_one(
                "SELECT deploy_hash FROM zeroship.apps WHERE id = $1",
                &[&fx.app_id],
            )
            .await
            .expect("load active app deploy hash")
            .get("deploy_hash");
        let rows = fx
            .pg
            .query(
                "SELECT id, deploy_hash, activated_at \
                   FROM zeroship.app_deploys \
                  WHERE app_id = $1 AND activated_at IS NOT NULL \
                  ORDER BY activated_at DESC, created_at DESC, id DESC \
                  LIMIT 1",
                &[&fx.app_id],
            )
            .await
            .expect("load resolved active deploy");
        if app_hash.as_deref() == Some(expected_hash) {
            if let Some(row) = rows.first() {
                let id: String = row.get("id");
                let hash: String = row.get("deploy_hash");
                let activated_at: Option<DateTime<Utc>> = row.get("activated_at");
                if id == expected_deploy_id && hash == expected_hash && activated_at.is_some() {
                    return;
                }
            }
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    let app_hash: Option<String> = fx
        .pg
        .query_one(
            "SELECT deploy_hash FROM zeroship.apps WHERE id = $1",
            &[&fx.app_id],
        )
        .await
        .expect("load final active app deploy hash")
        .get("deploy_hash");
    let rows = fx
        .pg
        .query(
            "SELECT id, deploy_hash, activated_at \
               FROM zeroship.app_deploys \
              WHERE app_id = $1 AND activated_at IS NOT NULL \
              ORDER BY activated_at DESC, created_at DESC, id DESC \
              LIMIT 1",
            &[&fx.app_id],
        )
        .await
        .expect("load final resolved active deploy");
    panic!(
        "deploy {expected_deploy_id}/{expected_hash} did not become resolved active; app_hash={app_hash:?}, resolved={:?}",
        rows.first().map(|row| {
            (
                row.get::<_, String>("id"),
                row.get::<_, String>("deploy_hash"),
                row.get::<_, Option<DateTime<Utc>>>("activated_at"),
            )
        })
    );
}

async fn child_run_ids(fx: &Fixture, parent_run_id: &str) -> Vec<String> {
    fx.pg
        .query(
            "SELECT id \
               FROM zeroship.workflow_runs \
              WHERE parent_run_id = $1 \
              ORDER BY created_at, id",
            &[&parent_run_id],
        )
        .await
        .expect("load child runs")
        .into_iter()
        .map(|row| row.get("id"))
        .collect()
}

async fn wait_for_child_count(fx: &Fixture, parent_run_id: &str, expected: usize) -> Vec<String> {
    for _ in 0..120 {
        let ids = child_run_ids(fx, parent_run_id).await;
        if ids.len() == expected {
            return ids;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "parent {parent_run_id} did not reach {expected} children; got {:?}",
        child_run_ids(fx, parent_run_id).await
    );
}

async fn wait_for_child_cancel_requested(fx: &Fixture, child_run_id: &str) {
    // Wait until the child is BOTH cancel_requested AND parked (queued/sleeping/
    // waiting). The parked-cancel reap only reaps a run in a parked state; if the
    // child's spawn dispatch is still in flight (state='running') when the reap
    // tick fires, the reap can't see it and it parks to 'sleeping' just after the
    // tick — a race. Waiting for the reachable parked state makes the single reap
    // tick deterministic (the cooperative-cancel behavior itself is eventual: a
    // running child self-cancels at its next step boundary, a parked one is reaped).
    for _ in 0..400 {
        let row = fx
            .pg
            .query_one(
                "SELECT cancel_requested, state FROM zeroship.workflow_runs WHERE id = $1",
                &[&child_run_id],
            )
            .await
            .expect("load child cancel flag");
        let parked = matches!(
            row.get::<_, String>("state").as_str(),
            "queued" | "sleeping" | "waiting"
        );
        if row.get::<_, bool>("cancel_requested") && parked {
            register_existing_run_timer(fx, child_run_id).await;
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("child {child_run_id} was not cancel_requested + parked");
}

async fn child_states(
    fx: &Fixture,
    child_run_ids: &[String],
) -> Vec<(String, String, Option<String>, String)> {
    fx.pg
        .query(
            "SELECT id, state, claimed_by, deploy_id \
               FROM zeroship.workflow_runs \
              WHERE id = ANY($1) \
              ORDER BY id",
            &[&child_run_ids],
        )
        .await
        .expect("load child states")
        .into_iter()
        .map(|row| {
            (
                row.get("id"),
                row.get("state"),
                row.get("claimed_by"),
                row.get("deploy_id"),
            )
        })
        .collect()
}

async fn drive_children_until_sleeping<D>(
    fx: &Fixture,
    dispatcher: Arc<D>,
    cfg: WorkflowEngineConfig,
    child_run_ids: &[String],
) where
    D: StepDispatcher + 'static,
{
    for _ in 0..600 {
        workflow_tick(fx, &dispatcher, &cfg, "child sleep").await;
        let states = child_states(fx, child_run_ids).await;
        assert_eq!(states.len(), child_run_ids.len(), "missing child rows");
        if states
            .iter()
            .all(|(_, state, claimed_by, _)| state == "sleeping" && claimed_by.is_none())
        {
            return;
        }
        if states.iter().any(|(_, state, _, _)| {
            matches!(state.as_str(), "completed" | "failed" | "cancelled" | "stalled")
        }) {
            panic!("child reached terminal before sleeping: {states:?}");
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "children did not park sleeping: {:?}",
        child_states(fx, child_run_ids).await
    );
}

async fn drive_children_until_cancelled(
    fx: &Fixture,
    dispatcher: Arc<CountingDispatcher>,
    cfg: WorkflowEngineConfig,
    child_run_ids: &[String],
) {
    let mut initial_step_counts = Vec::new();
    for child_run_id in child_run_ids {
        initial_step_counts.push((
            child_run_id.clone(),
            workflow_step_count(&fx.pg, child_run_id).await,
        ));
    }
    for _ in 0..600 {
        let _claimed = workflow_tick(fx, &dispatcher, &cfg, "child cancel").await;
        let _redispatched = workflow_engine::reap_lapsed_inflight_once(
            &fx.scheduler_store,
            &fx.state,
            Arc::clone(&dispatcher),
            cfg.clone(),
            child_run_ids.len() as i64,
        )
        .await
        .expect("child cancel inflight reaper");
        let states = child_states(fx, child_run_ids).await;
        if states
            .iter()
            .all(|(_, state, claimed_by, _)| state == "cancelled" && claimed_by.is_none())
        {
            for (child_run_id, initial_count) in initial_step_counts {
                assert_eq!(
                    workflow_step_count(&fx.pg, &child_run_id).await,
                    initial_count,
                    "cancel pickup must not replay child workflow code"
                );
            }
            return;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "children did not cancel after {} dispatch attempts: {:?}",
        dispatcher.count(),
        child_states(fx, child_run_ids).await
    );
}

async fn active_child_count_for_deploy(
    fx: &Fixture,
    parent_run_id: &str,
    deploy_id: &str,
) -> i64 {
    fx.pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE parent_run_id = $1 \
                AND deploy_id = $2 \
                AND state IN ('queued','running','sleeping','waiting','compensating')",
            &[&parent_run_id, &deploy_id],
        )
        .await
        .expect("count active child runs for deploy")
        .get("n")
}

async fn assert_child_dedup_keys(fx: &Fixture, parent_run_id: &str, expected: usize) {
    let rows = fx
        .pg
        .query(
            "SELECT dedup_key, parent_wait_step_key, tree_depth \
               FROM zeroship.workflow_runs \
              WHERE parent_run_id = $1",
            &[&parent_run_id],
        )
        .await
        .expect("load child dedup keys");
    assert_eq!(rows.len(), expected, "child count for parent {parent_run_id}");
    // Children spawned in one startMany frontier can land in the runs table in
    // any order relative to their ordinal (created_at can tie within a dispatch,
    // and the UUIDv7 id is not ordinal-ordered), so compare the SET of
    // (dedup_key, wait_step_key) rather than assume row position == ordinal.
    // This still catches any wrong / missing / duplicate ordinal.
    let mut actual: Vec<(Option<String>, Option<String>)> = rows
        .iter()
        .map(|row| {
            assert_eq!(row.get::<_, i16>("tree_depth"), 1);
            (
                row.get::<_, Option<String>>("dedup_key"),
                row.get::<_, Option<String>>("parent_wait_step_key"),
            )
        })
        .collect();
    actual.sort();
    let mut want: Vec<(Option<String>, Option<String>)> = (0..expected as i32)
        .map(|i| {
            (
                Some(workflow_engine::child_dedup_key(parent_run_id, i)),
                Some(workflow_engine::child_signal_type(i)),
            )
        })
        .collect();
    want.sort();
    assert_eq!(actual, want, "child dedup/wait keys for parent {parent_run_id}");
}

async fn scheduled_run_for(
    fx: &Fixture,
    schedule_id: &str,
    planned: DateTime<Utc>,
) -> Option<(String, DateTime<Utc>, serde_json::Value)> {
    let key = format!("sched:{schedule_id}:{}", planned.timestamp_millis());
    let rows = fx
        .pg
        .query(
            "SELECT id, started_at, input \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 \
                AND workflow_name = $2 \
                AND dedup_key = $3 \
              ORDER BY created_at, id",
            &[&fx.app_id, &SCHEDULED_WORKFLOW_NAME, &key],
        )
        .await
        .expect("load scheduled run");
    rows.first().map(|row| {
        (
            row.get("id"),
            row.get("started_at"),
            row.get::<_, Option<serde_json::Value>>("input")
                .unwrap_or(serde_json::Value::Null),
        )
    })
}

async fn post_signal(
    control_url: &str,
    app_id: Uuid,
    run_id: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    let url = format!(
        "{}/internal/workflows/runs/{}/signal",
        control_url.trim_end_matches('/'),
        run_id
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "type": "go",
        "payload": payload,
    }))
    .expect("signal body json");
    let client = cyper::Client::new();
    let builder = client.post(&url).expect("signal request URL");
    let builder = builder
        .header("content-type", "application/json")
        .expect("content-type header")
        .header("x-zeroship-app-id", app_id.to_string())
        .expect("app id header");
    let response = builder.body(body).send().await.expect("post signal");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("read signal response");
    assert_eq!(
        status,
        202,
        "signal endpoint returned HTTP {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).expect("signal response json")
}

async fn create_run_signal_token(
    control_url: &str,
    app_id: Uuid,
    run_id: &str,
    ttl: &str,
) -> String {
    let url = format!(
        "{}/internal/workflows/runs/{}/signal-token",
        control_url.trim_end_matches('/'),
        run_id
    );
    create_signal_token_at(&url, app_id, ttl).await
}

async fn create_topic_signal_token(
    control_url: &str,
    app_id: Uuid,
    topic: &str,
    ttl: &str,
) -> String {
    let url = format!(
        "{}/internal/workflows/topics/{}/signal-token",
        control_url.trim_end_matches('/'),
        topic
    );
    create_signal_token_at(&url, app_id, ttl).await
}

async fn create_signal_token_at(url: &str, app_id: Uuid, ttl: &str) -> String {
    let body = serde_json::to_vec(&serde_json::json!({
        "types": ["go"],
        "ttl": ttl,
    }))
    .expect("token body json");
    let client = cyper::Client::new();
    let response = client
        .post(url)
        .expect("token request URL")
        .header("content-type", "application/json")
        .expect("content-type header")
        .header("x-zeroship-app-id", app_id.to_string())
        .expect("app id header")
        .body(body)
        .send()
        .await
        .expect("post signal token");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("read token response");
    assert_eq!(
        status,
        200,
        "signal token endpoint returned HTTP {status}: {}",
        String::from_utf8_lossy(&bytes)
    );
    let value: serde_json::Value = serde_json::from_slice(&bytes).expect("token response json");
    let token = value["token"].as_str().expect("token string").to_string();
    assert!(token.starts_with("wst_"), "unexpected token {token}");
    token
}

async fn post_public_signal(
    gateway_url: &str,
    token: &str,
    payload: serde_json::Value,
) -> (u16, serde_json::Value) {
    let url = format!("{}/__zeroship/v1/signal", gateway_url.trim_end_matches('/'));
    let body = serde_json::to_vec(&serde_json::json!({
        "token": token,
        "payload": payload,
    }))
    .expect("public signal body json");
    let client = cyper::Client::new();
    let response = client
        .post(&url)
        .expect("public signal request URL")
        .header("content-type", "application/json")
        .expect("content-type header")
        .body(body)
        .send()
        .await
        .expect("post public signal");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("read public signal response");
    let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        serde_json::json!({ "raw": String::from_utf8_lossy(&bytes).to_string() })
    });
    (status, value)
}

async fn post_control(
    control_url: &str,
    app_id: Uuid,
    run_id: &str,
    op: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let url = format!(
        "{}/internal/workflows/runs/{}/{}",
        control_url.trim_end_matches('/'),
        run_id,
        op
    );
    let bytes = serde_json::to_vec(&body).expect("control body json");
    let client = cyper::Client::new();
    let builder = client.post(&url).expect("control request URL");
    let builder = builder
        .header("content-type", "application/json")
        .expect("content-type header")
        .header("x-zeroship-app-id", app_id.to_string())
        .expect("app id header");
    let response = builder.body(bytes).send().await.expect("post control");
    let status = response.status().as_u16();
    let body_bytes = response.bytes().await.expect("read control response");
    assert_eq!(
        status,
        200,
        "{op} endpoint returned HTTP {status}: {}",
        String::from_utf8_lossy(&body_bytes)
    );
    serde_json::from_slice(&body_bytes).expect("control response json")
}

async fn post_control_raw(
    control_url: &str,
    app_id: Uuid,
    run_id: &str,
    op: &str,
    body: serde_json::Value,
) -> (u16, serde_json::Value) {
    let url = format!(
        "{}/internal/workflows/runs/{}/{}",
        control_url.trim_end_matches('/'),
        run_id,
        op
    );
    let bytes = serde_json::to_vec(&body).expect("control body json");
    let client = cyper::Client::new();
    let response = client
        .post(&url)
        .expect("control request URL")
        .header("content-type", "application/json")
        .expect("content-type header")
        .header("x-zeroship-app-id", app_id.to_string())
        .expect("app id header")
        .body(bytes)
        .send()
        .await
        .expect("post control");
    let status = response.status().as_u16();
    let body_bytes = response.bytes().await.expect("read control response");
    let value = serde_json::from_slice(&body_bytes).unwrap_or_else(|_| {
        serde_json::json!({ "raw": String::from_utf8_lossy(&body_bytes).to_string() })
    });
    (status, value)
}

async fn get_output_bytes(
    control_url: &str,
    app_id: Uuid,
    run_id: &str,
    step_name: Option<&str>,
    range: Option<&str>,
) -> (u16, Vec<u8>) {
    let url = match step_name {
        Some(step_name) => format!(
            "{}/internal/workflows/runs/{}/steps/{}/output?occurrence=0",
            control_url.trim_end_matches('/'),
            run_id,
            step_name
        ),
        None => format!(
            "{}/internal/workflows/runs/{}/output",
            control_url.trim_end_matches('/'),
            run_id
        ),
    };
    let client = cyper::Client::new();
    let mut builder = client
        .get(&url)
        .expect("output request URL")
        .header("x-zeroship-app-id", app_id.to_string())
        .expect("app id header");
    if let Some(range) = range {
        builder = builder.header("range", range).expect("range header");
    }
    let response = builder.send().await.expect("get output");
    let status = response.status().as_u16();
    let bytes = response.bytes().await.expect("read output response");
    (status, bytes.to_vec())
}

fn count_regular_files(root: &Path) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_file() => 1,
                Ok(kind) if kind.is_dir() => count_regular_files(&path),
                _ => 0,
            }
        })
        .sum()
}

async fn run_debug(fx: &Fixture, run_id: &str) -> String {
    let run_rows = fx
        .pg
        .query(
            "SELECT state, error, output, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs WHERE id = $1",
            &[&run_id],
        )
        .await
        .expect("debug run");
    let mut run_summary = "missing run".to_string();
    if let Some(row) = run_rows.first() {
        let state: String = row.get("state");
        let error: Option<serde_json::Value> = row.get("error");
        let output: Option<serde_json::Value> = row.get("output");
        let wake_at: Option<DateTime<Utc>> = row.get("wake_at");
        let claimed_by: Option<String> = row.get("claimed_by");
        let dispatch_nonce: Option<String> = row.get("dispatch_nonce");
        run_summary = format!(
            "state={state} error={error:?} output={output:?} wake_at={wake_at:?} \
             claimed_by={claimed_by:?} dispatch_nonce={dispatch_nonce:?}"
        );
    }
    // (id, state, wake_at, claimed_by, dispatch_nonce)
    type ChildRow = (String, String, Option<DateTime<Utc>>, Option<String>, Option<String>);
    let child_rows: Vec<ChildRow> = fx
        .pg
        .query(
            "SELECT id, state, wake_at, claimed_by, dispatch_nonce \
               FROM zeroship.workflow_runs \
              WHERE parent_run_id = $1 \
              ORDER BY created_at, id",
            &[&run_id],
        )
        .await
        .expect("debug child runs")
        .into_iter()
        .map(|row| {
            (
                row.get("id"),
                row.get("state"),
                row.get("wake_at"),
                row.get("claimed_by"),
                row.get("dispatch_nonce"),
            )
        })
        .collect();
    // (run_id, slot, wake_at, deadline, generation)
    type SchedulerRow = (
        String,
        String,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<i64>,
    );
    let scheduler_rows: Vec<SchedulerRow> = fx
        .pg
        .query(
            "SELECT run_id, slot, wake_at, deadline, generation \
               FROM ( \
                 SELECT run_id, 'timer' AS slot, wake_at, NULL::timestamptz AS deadline, generation \
                   FROM zeroship.workflow_scheduler_timers \
                  WHERE run_id = $1 OR run_id IN ( \
                    SELECT id FROM zeroship.workflow_runs WHERE parent_run_id = $1 \
                  ) \
                 UNION ALL \
                 SELECT run_id, 'inflight' AS slot, NULL::timestamptz AS wake_at, deadline, dispatch_generation AS generation \
                   FROM zeroship.workflow_scheduler_inflight \
                  WHERE run_id = $1 OR run_id IN ( \
                    SELECT id FROM zeroship.workflow_runs WHERE parent_run_id = $1 \
                  ) \
               ) rows \
              ORDER BY run_id, slot",
            &[&run_id],
        )
        .await
        .expect("debug scheduler rows")
        .into_iter()
        .map(|row| {
            (
                row.get("run_id"),
                row.get("slot"),
                row.get("wake_at"),
                row.get("deadline"),
                row.get("generation"),
            )
        })
        .collect();
    format!(
        "{run_summary}; steps={:?}; children={child_rows:?}; scheduler={scheduler_rows:?}; side_counts={:?}",
        step_rows(fx, run_id).await,
        side_counts(fx, run_id).await
    )
}

/// This test SIZES the stack it runs on, and the body is a separate `async fn`
/// reached through that thread, because its future does not fit the stack
/// libtest hands a test.
///
/// MEASURED 2026-08-20 against this file, one variable changed - `RUST_MIN_STACK`,
/// which is the size libtest's per-test thread gets:
///
///     2304 KiB   SIGABRT, "has overflowed its stack"
///     2560 KiB   test result: ok
///
/// Rust's default thread stack is 2 MiB, so the future sat about 400 KiB over it.
/// The consequence is not a failing test: the process ABORTS and takes every
/// later test in the binary with it. `cargo test -p zeroship-control --test main`
/// exited 101 on SIGABRT having reported 14 of 26, and `tests/run_billing_suite.sh`
/// exited 1 for the same reason. That target is ungated, so `cargo test
/// --workspace` hit it with no database at all.
///
/// BOTH HALVES OF THE SHAPE BELOW MATTER. The overflow happened while `block_on`
/// placed the future on the stack, which is BEFORE the body's first line - so it
/// fired even on the run that had no fleet and did nothing. The fleet check is
/// now made outside any future, so a run without `ZEROSHIP_DW_E2E` refuses
/// without constructing one; the enabled run gets 16 MiB, six times the
/// measured need.
///
/// ONE BEHAVIOUR CHANGES, and it is worth knowing: libtest's output capture is
/// thread-local, so the body's `println!`s now reach the terminal instead of
/// being buffered and replayed only on failure. `tests/e2e_durable_workflows.sh`
/// is the only caller that enables this test and it already passes `--nocapture`,
/// so nothing it prints was being captured anyway.
///
/// WHAT THIS DOES NOT FIX: the body is one 2000-line `async fn`, and a named
/// 16 MiB budget only makes the next 13 MiB of growth affordable rather than
/// impossible. Splitting it is the real answer and needs a run of
/// `tests/e2e_durable_workflows.sh` to verify, which this change did not have.
#[test]
#[serial]
fn durable_workflows_m1_keystone_real_spine() {
    require_dw_e2e_fleet();

    let handle = thread::Builder::new()
        .name("dw-keystone".to_owned())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let rt = compio::runtime::Runtime::new().expect("compio runtime");
            rt.block_on(keystone_real_spine());
        })
        .expect("spawn the keystone thread");
    // `resume_unwind`, not `expect`: an assertion anywhere in the body has to
    // reach libtest as its own payload, or every failure in 2000 lines is
    // reported as one message naming this join.
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

async fn keystone_real_spine() {
    let db_url = required_env("a test database", zeroship_core::config::test_database_url_opt());
    let control_url = required_env("ZEROSHIP_DW_E2E_CONTROL_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_CONTROL_URL"));
    let gateway_url = required_env("ZEROSHIP_DW_E2E_GATEWAY_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_GATEWAY_URL"));
    let app_id: Uuid = required_env("ZEROSHIP_DW_E2E_APP_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_APP_ID"))
        .parse()
        .expect("app id uuid");
    let deploy_id = required_env("ZEROSHIP_DW_E2E_DEPLOY_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_DEPLOY_ID"));

    let fx = build_fixture(&db_url, &gateway_url, app_id, deploy_id).await;
    prepare_side_effect_table(&fx.pg).await;

    let real_dispatcher = Arc::new(GatewayStepDispatcher::new(gateway_url.clone(), test_control_service_auth()));
    rollout_switch_drill(
        &fx,
        &control_url,
        &gateway_url,
        Arc::clone(&real_dispatcher),
    )
    .await;

    let schedule_row = fx
        .pg
        .query_one(
            "SELECT id, deploy_hash, workflow_name, kind, cron_expr, tz, next_fire_at \
               FROM zeroship.workflow_schedules \
              WHERE app_id = $1 AND name = 'dw14-scheduled'",
            &[&fx.app_id],
        )
        .await
        .expect("load reconciled schedule row");
    let schedule_id: String = schedule_row.get("id");
    assert!(schedule_id.starts_with("sch_"));
    let schedule_deploy_hash: String = schedule_row.get("deploy_hash");
    assert!(!schedule_deploy_hash.is_empty(), "schedule row carries deploy hash");
    assert_eq!(
        schedule_row.get::<_, String>("workflow_name"),
        SCHEDULED_WORKFLOW_NAME
    );
    assert_eq!(schedule_row.get::<_, String>("kind"), "cron");
    assert_eq!(
        schedule_row.get::<_, Option<String>>("cron_expr").as_deref(),
        Some("* * * * *")
    );
    assert_eq!(
        schedule_row.get::<_, Option<String>>("tz").as_deref(),
        Some("UTC")
    );
    let initial_next: DateTime<Utc> = schedule_row.get("next_fire_at");
    assert!(
        initial_next > Utc::now() - ChronoDuration::minutes(1),
        "initial next fire should be reconciled from deploy time, got {initial_next:?}"
    );

    let planned = DateTime::<Utc>::from_timestamp_millis(
        (Utc::now() - ChronoDuration::minutes(3)).timestamp_millis(),
    )
    .expect("planned instant");
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_schedules \
                SET next_fire_at = $2, claimed_by = NULL, claimed_at = NULL, lease_expires = NULL \
              WHERE id = $1",
            &[&schedule_id, &planned],
        )
        .await
        .expect("advance schedule clock");
    let fired = workflow_schedules::tick_with_config(&fx.state, schedule_config("dw14-schedule-a"))
        .await
        .expect("schedule sweep tick");
    let scheduled = scheduled_run_for(&fx, &schedule_id, planned).await;
    if fired == 0 {
        assert!(
            scheduled.is_some(),
            "schedule sweep fired zero runs and no background cron-created run exists"
        );
    } else {
        assert_eq!(fired, 1, "schedule sweep should fire one queued run");
    }
    let (scheduled_run, scheduled_started_at, scheduled_input) =
        scheduled.expect("scheduled run created");
    register_existing_run_timer(&fx, &scheduled_run).await;
    assert_eq!(scheduled_started_at, planned);
    assert_eq!(scheduled_input, serde_json::json!({"case": "schedule"}));
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw14-schedule-drive"),
        &scheduled_run,
    )
    .await;
    let scheduled_output = run_output(&fx, &scheduled_run).await;
    assert_eq!(scheduled_output["input"], serde_json::json!({"case": "schedule"}));
    assert_eq!(scheduled_output["workflowName"], SCHEDULED_WORKFLOW_NAME);
    let output_started = DateTime::parse_from_rfc3339(
        scheduled_output["startedAt"]
            .as_str()
            .expect("scheduled output startedAt"),
    )
    .expect("parse scheduled startedAt")
    .with_timezone(&Utc);
    assert_eq!(output_started, planned);

    let can_run = seed_workflow_run(
        &fx,
        CONTINUE_AS_NEW_WORKFLOW_NAME,
        serde_json::json!({"case": "can", "generation": 0}),
    )
    .await;
    let can_dispatcher = Arc::new(CapturingDispatcher::new(gateway_url.clone()));
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&can_dispatcher),
        config("p4-can-first"),
    )
    .await
    .expect("continue-as-new first tick");
    assert_eq!(claimed, 1, "continue-as-new first step should dispatch once");
    wait_for_step_count(&fx, &can_run, 1).await;
    assert_eq!(
        step_rows(&fx, &can_run).await,
        vec![(
            0,
            "before-can".to_string(),
            "run".to_string(),
            "completed".to_string(),
        )],
        "continue-as-new first generation should stop after the seed step"
    );

    let (can_redeploy_hash, can_redeploy_id) = activate_redeploy_with_current_manifest(&fx).await;
    wait_for_active_deploy(&fx, &can_redeploy_hash, &can_redeploy_id).await;
    force_run_due(&fx, &can_run).await;
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&can_dispatcher),
        config("p4-can-terminal"),
    )
    .await
    .expect("continue-as-new terminal tick");
    assert_eq!(claimed, 1, "continue-as-new terminal should dispatch once");
    wait_for_captured_dispatch_count(&can_dispatcher, 2).await;
    let outcomes = can_dispatcher.outcomes();
    match outcomes.last().expect("continue-as-new terminal dispatch") {
        DispatchOutcome::Completed(response) => {
            assert!(response.is_ack(), "continue-as-new terminal returned nack: {response:?}");
        }
        other => panic!("continue-as-new terminal returned backpressure: {other:?}"),
    }
    let can_row = fx
        .pg
        .query_one(
            "SELECT state, continued_as_new_run_id \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&can_run],
        )
        .await
        .expect("load continued generation");
    assert_eq!(can_row.get::<_, String>("state"), "completed");
    let fresh_run: String = can_row
        .get::<_, Option<String>>("continued_as_new_run_id")
        .expect("continued generation should stamp successor");
    let fresh_row = fx
        .pg
        .query_one(
            "SELECT workflow_name, deploy_id, state, input, parent_run_id, dedup_key, next_ordinal \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&fresh_run],
        )
        .await
        .expect("load fresh continued run");
    assert_eq!(
        fresh_row.get::<_, String>("workflow_name"),
        CONTINUE_AS_NEW_WORKFLOW_NAME
    );
    assert_eq!(fresh_row.get::<_, String>("deploy_id"), can_redeploy_id);
    assert_eq!(fresh_row.get::<_, String>("state"), "queued");
    assert_eq!(fresh_row.get::<_, i32>("next_ordinal"), 0);
    assert!(fresh_row.get::<_, Option<String>>("parent_run_id").is_none());
    assert!(fresh_row.get::<_, Option<String>>("dedup_key").is_none());
    let fresh_input: serde_json::Value = fresh_row.get("input");
    assert_eq!(fresh_input["generation"], serde_json::json!(1));
    assert_eq!(fresh_input["case"], serde_json::json!("can"));
    assert_eq!(fresh_input["previousRunId"], serde_json::json!(can_run));
    assert_eq!(fresh_input["marker"]["step"], serde_json::json!("before-can"));
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("p4-can-fresh"),
        &fresh_run,
    )
    .await;
    let fresh_output = run_output(&fx, &fresh_run).await;
    assert_eq!(fresh_output["generation"], serde_json::json!(1));
    assert_eq!(fresh_output["previousRunId"], serde_json::json!(can_run));
    assert_eq!(fresh_output["marker"]["step"], serde_json::json!("before-can"));
    assert_eq!(fresh_output["after"]["step"], serde_json::json!("after-can"));

    let carry_run = seed_workflow_run(
        &fx,
        COMPENSABLE_CARRY_WORKFLOW_NAME,
        serde_json::json!({"case": "carry"}),
    )
    .await;
    let carry_dispatcher = Arc::new(CapturingDispatcher::new(gateway_url.clone()));
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&carry_dispatcher),
        config("p4-carry-first"),
    )
    .await
    .expect("compensable carry first tick");
    assert_eq!(claimed, 1);
    wait_for_step_count(&fx, &carry_run, 1).await;
    let compensation_state: Option<String> = fx
        .pg
        .query_one(
            "SELECT compensation_state \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = 0",
            &[&carry_run],
        )
        .await
        .expect("load compensable step")
        .get("compensation_state");
    assert_eq!(compensation_state.as_deref(), Some("pending"));

    force_run_due(&fx, &carry_run).await;
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&carry_dispatcher),
        config("p4-carry-reject"),
    )
    .await
    .expect("compensable carry reject tick");
    assert_eq!(claimed, 1);
    let outcomes = carry_dispatcher.outcomes();
    let response = match outcomes.last().expect("captured carry response") {
        DispatchOutcome::Completed(response) => response,
        other => panic!("expected carry apply response, got {other:?}"),
    };
    assert!(response.is_nack(), "carry response should nack: {response:?}");
    assert_eq!(response.nack_kind, Some(WorkflowAdvanceNackKind::Invalid));
    assert!(
        response
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("CompensableCarryError")),
        "carry response should be typed: {response:?}"
    );
    let carry_row = fx
        .pg
        .query_one(
            "SELECT continued_as_new_run_id \
               FROM zeroship.workflow_runs \
              WHERE id = $1",
            &[&carry_run],
        )
        .await
        .expect("load carry run");
    assert!(
        carry_row
            .get::<_, Option<String>>("continued_as_new_run_id")
            .is_none(),
        "compensable carry must not stamp a successor"
    );
    let carry_successors: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE workflow_name = $1 AND id <> $2",
            &[&COMPENSABLE_CARRY_WORKFLOW_NAME, &carry_run],
        )
        .await
        .expect("count carry successors")
        .get("n");
    assert_eq!(carry_successors, 0, "compensable carry must not create a fresh run");
    fx.scheduler_store
        .ack_terminal(&carry_run)
        .await
        .expect("retire rejected compensable-carry scheduler row");

    let concurrent_planned = DateTime::<Utc>::from_timestamp_millis(
        (Utc::now() - ChronoDuration::minutes(2)).timestamp_millis(),
    )
    .expect("concurrent planned instant");
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_schedules \
                SET next_fire_at = $2, claimed_by = NULL, claimed_at = NULL, lease_expires = NULL \
              WHERE id = $1",
            &[&schedule_id, &concurrent_planned],
        )
        .await
        .expect("advance schedule clock for concurrent sweep");
    let (tick_a, tick_b) = futures::future::join(
        workflow_schedules::tick_with_config(
            &fx.state,
            schedule_config("dw14-schedule-concurrent-a"),
        ),
        workflow_schedules::tick_with_config(
            &fx.state,
            schedule_config("dw14-schedule-concurrent-b"),
        ),
    )
    .await;
    let tick_a = tick_a.expect("schedule concurrent tick a");
    let tick_b = tick_b.expect("schedule concurrent tick b");
    let key = format!(
        "sched:{schedule_id}:{}",
        concurrent_planned.timestamp_millis()
    );
    let mut run_count: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3",
            &[&fx.app_id, &SCHEDULED_WORKFLOW_NAME, &key],
        )
        .await
        .expect("count concurrent scheduled runs")
        .get("n");
    if tick_a + tick_b == 0 {
        for _ in 0..80 {
            if run_count == 1 {
                break;
            }
            compio::time::sleep(Duration::from_millis(25)).await;
            run_count = fx
                .pg
                .query_one(
                    "SELECT COUNT(*)::bigint AS n \
                       FROM zeroship.workflow_runs \
                      WHERE app_id = $1 AND workflow_name = $2 AND dedup_key = $3",
                    &[&fx.app_id, &SCHEDULED_WORKFLOW_NAME, &key],
                )
                .await
                .expect("count concurrent scheduled runs after background tick")
                .get("n");
        }
        assert_eq!(
            run_count, 1,
            "background schedule cron should have created the planned run if explicit ticks fired none"
        );
    } else {
        assert_eq!(
            tick_a + tick_b,
            1,
            "two concurrent schedule ticks should fire the planned instant once"
        );
    }
    assert_eq!(run_count, 1, "dedup key must leave exactly one run");
    let (concurrent_scheduled_run, concurrent_started_at, _) =
        scheduled_run_for(&fx, &schedule_id, concurrent_planned)
            .await
            .expect("concurrent scheduled run created");
    register_existing_run_timer(&fx, &concurrent_scheduled_run).await;
    assert_eq!(concurrent_started_at, concurrent_planned);
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw14-schedule-concurrent-drive"),
        &concurrent_scheduled_run,
    )
    .await;

    let happy_run = seed_run(&fx, "happy").await;
    for _ in 0..120 {
        workflow_engine::fire_once(
            &fx.scheduler_store,
            &fx.state,
            Arc::clone(&real_dispatcher),
            config("dw07-happy"),
        )
        .await
        .expect("happy tick");
        let (state, wake_at, _, _) = run_state(&fx.pg, &happy_run).await;
        if state == "failed" {
            panic!(
                "happy run failed before sleeping: {}",
                run_debug(&fx, &happy_run).await
            );
        }
        if state == "sleeping" {
            let Some(wake_at) = wake_at else {
                panic!(
                    "sleeping run has no wake_at: {}",
                    run_debug(&fx, &happy_run).await
                );
            };
            assert!(
                wake_at > Utc::now() - ChronoDuration::milliseconds(25),
                "sleep wake_at should not be in the past when first parked"
            );
            break;
        }
        compio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        run_state(&fx.pg, &happy_run).await.0,
        "sleeping",
        "{}",
        run_debug(&fx, &happy_run).await
    );
    drive_until_completed(&fx, Arc::clone(&real_dispatcher), config("dw07-happy"), &happy_run)
        .await;
    assert_expected_steps(&fx, &happy_run).await;
    let happy_counts = side_counts(&fx, &happy_run).await;
    assert_eq!(happy_counts.get("a").copied(), Some(1));
    assert_eq!(happy_counts.get("b").copied(), Some(1));

    let restart = post_control(
        &control_url,
        fx.app_id,
        &happy_run,
        "restart",
        serde_json::json!({"from": {"name": "b"}}),
    )
    .await;
    assert_eq!(restart["runId"], happy_run);
    assert_eq!(restart["state"], "queued");
    assert_eq!(restart["restartedFromOrdinal"], 2);
    assert_eq!(
        step_rows(&fx, &happy_run).await,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (1, "sleep".to_string(), "sleep".to_string(), "completed".to_string()),
        ],
        "restart from b should retain only the prefix before b"
    );
    register_existing_run_timer(&fx, &happy_run).await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-restart-drive"),
        &happy_run,
    )
    .await;
    assert_expected_steps(&fx, &happy_run).await;
    let restarted_counts = side_counts(&fx, &happy_run).await;
    assert_eq!(
        restarted_counts.get("a").copied(),
        Some(1),
        "restart from b must not re-run retained step a"
    );
    assert_eq!(
        restarted_counts.get("b").copied(),
        Some(2),
        "restart from b must re-run b"
    );

    let concurrent_run = seed_workflow_run(
        &fx,
        CONCURRENT_WORKFLOW_NAME,
        serde_json::json!({"case": "concurrent"}),
    )
    .await;
    let concurrent_dispatcher = Arc::new(CountingDispatcher::new(gateway_url.clone()));
    drive_until_completed(
        &fx,
        Arc::clone(&concurrent_dispatcher),
        config("dw07-concurrent"),
        &concurrent_run,
    )
    .await;
    assert_concurrent_steps(&fx, &concurrent_run).await;
    let concurrent_counts = side_counts(&fx, &concurrent_run).await;
    assert_eq!(concurrent_counts.get("a").copied(), Some(1));
    assert_eq!(concurrent_counts.get("b").copied(), Some(1));
    assert_eq!(concurrent_counts.get("c").copied(), Some(1));
    assert_eq!(concurrent_counts.get("final").copied(), Some(1));
    assert!(
        concurrent_dispatcher.count() < 4,
        "3-wide frontier plus final should complete in fewer dispatches than serial a,b,c,final; got {}",
        concurrent_dispatcher.count()
    );

    let frontier_run = seed_workflow_run(
        &fx,
        CONCURRENT_COMMIT_WORKFLOW_NAME,
        serde_json::json!({"case": "dw19-frontier"}),
    )
    .await;
    let (frontier_dispatcher, frontier_dropped_rx, frontier_release_tx) =
        CrashOnceDispatcher::new(gateway_url.clone());
    let frontier_dispatcher = Arc::new(frontier_dispatcher);
    let frontier_owner = "dw19-frontier-first";
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&frontier_dispatcher),
        config(frontier_owner),
    )
    .await
    .expect("DW19 frontier first tick");
    assert_eq!(claimed, 1);
    let frontier_ack = expect_completed_ack(
        frontier_dropped_rx
        .await
            .expect("DW19 frontier held ack"),
        "DW19 frontier held dispatch",
    );
    assert_ack_run(&frontier_ack, &frontier_run, "DW19 frontier held dispatch");
    assert_eq!(
        effect_commit_counts(&fx, &frontier_run).await,
        BTreeMap::from([
            ("frontier:a".to_string(), 1),
            ("frontier:b".to_string(), 1),
            ("frontier:c".to_string(), 1),
        ]),
        "frontier side effects commit in the worker before the held ack is released"
    );
    assert_eq!(
        step_rows(&fx, &frontier_run).await,
        vec![
            (0, "a".to_string(), "run".to_string(), "completed".to_string()),
            (1, "b".to_string(), "run".to_string(), "completed".to_string()),
            (2, "c".to_string(), "run".to_string(), "completed".to_string()),
        ],
        "worker apply lands the full concurrent frontier before the ack"
    );
    let _ = frontier_release_tx.send(());
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw19-frontier-redrive"),
        &frontier_run,
    )
    .await;
    assert_concurrent_steps(&fx, &frontier_run).await;
    let frontier_commits = effect_commit_counts(&fx, &frontier_run).await;
    assert_eq!(frontier_commits.get("frontier:a").copied(), Some(1));
    assert_eq!(frontier_commits.get("frontier:b").copied(), Some(1));
    assert_eq!(frontier_commits.get("frontier:c").copied(), Some(1));
    assert_eq!(frontier_commits.get("frontier:final").copied(), Some(1));
    let frontier_attempts = effect_attempt_counts(&fx, &frontier_run).await;
    assert_eq!(frontier_attempts.get("frontier:a").copied(), Some(1));
    assert_eq!(frontier_attempts.get("frontier:b").copied(), Some(1));
    assert_eq!(frontier_attempts.get("frontier:c").copied(), Some(1));

    // CW1/CW2/CW3: step.call parks the parent, spawns one deterministic child,
    // and resumes with the child's output. Holding the ack now simulates the
    // worker-applied/control-ack-lost window.
    let cw1_parent = seed_workflow_run(
        &fx,
        PARENT_CALL_WORKFLOW_NAME,
        serde_json::json!({"case": "cw1", "value": "alpha", "cascade": true}),
    )
    .await;
    let (cw1_dispatcher, cw1_dropped_rx, cw1_release_tx) =
        CrashOnceDispatcher::new(gateway_url.clone());
    let cw1_dispatcher = Arc::new(cw1_dispatcher);
    let cw1_owner = "dw17-cw1-parent";
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&cw1_dispatcher),
        config(cw1_owner),
    )
    .await
    .expect("CW1 first tick");
    assert_eq!(claimed, 1);
    let cw1_ack = expect_completed_ack(
        cw1_dropped_rx.await.expect("CW1 dropped parent ack"),
        "CW1 parent",
    );
    assert_ack_run(&cw1_ack, &cw1_parent, "CW1 parent");
    let cw1_children = wait_for_child_count(&fx, &cw1_parent, 1).await;
    assert_child_dedup_keys(&fx, &cw1_parent, 1).await;
    let cw1_parent_row = fx
        .pg
        .query_one(
            "SELECT state, waiting_step_key FROM zeroship.workflow_runs WHERE id = $1",
            &[&cw1_parent],
        )
        .await
        .expect("CW1 parent parked");
    assert_eq!(cw1_parent_row.get::<_, String>("state"), "waiting");
    assert_eq!(
        cw1_parent_row.get::<_, Option<String>>("waiting_step_key"),
        Some("child:0:ChildEchoWorkflow".to_string())
    );
    let _ = cw1_release_tx.send(());
    compio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        child_run_ids(&fx, &cw1_parent).await,
        cw1_children,
        "CW1 duplicate apply must not spawn a second child"
    );
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw17-cw1-drive"),
        &cw1_parent,
    )
    .await;
    let cw1_output = run_output(&fx, &cw1_parent).await;
    assert_eq!(cw1_output["child"]["value"], "alpha");
    assert_eq!(cw1_output["after"]["value"], "alpha");
    assert_eq!(
        cw1_output["after"]["childRunId"].as_str(),
        Some(cw1_children[0].as_str())
    );
    assert_eq!(
        step_rows(&fx, &cw1_parent).await,
        vec![
            (
                0,
                "ChildEchoWorkflow".to_string(),
                "child".to_string(),
                "completed".to_string(),
            ),
            (
                1,
                "after-child".to_string(),
                "run".to_string(),
                "completed".to_string(),
            ),
        ],
        "CW2/CW3 parent should contain the child join and follow-up step"
    );

    // CW4: startMany is a bounded child batch; all children are spawned once,
    // run as real child workflows, and join in issue order.
    let cw4_parent = seed_workflow_run(
        &fx,
        PARENT_START_MANY_WORKFLOW_NAME,
        serde_json::json!({"case": "cw4", "values": ["a", "b", "c"]}),
    )
    .await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw17-cw4-start-many"),
        &cw4_parent,
    )
    .await;
    assert_eq!(child_run_ids(&fx, &cw4_parent).await.len(), 3);
    assert_child_dedup_keys(&fx, &cw4_parent, 3).await;
    let cw4_output = run_output(&fx, &cw4_parent).await;
    assert_eq!(cw4_output["outputs"][0]["value"], "a");
    assert_eq!(cw4_output["outputs"][1]["value"], "b");
    assert_eq!(cw4_output["outputs"][2]["value"], "c");

    // CW5: child failure is catchable at the parent's step.call site.
    let cw5_parent = seed_workflow_run(
        &fx,
        PARENT_CATCH_CHILD_FAILURE_WORKFLOW_NAME,
        serde_json::json!({"case": "cw5", "value": "boom"}),
    )
    .await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw17-cw5-catch-child-failure"),
        &cw5_parent,
    )
    .await;
    let cw5_output = run_output(&fx, &cw5_parent).await;
    assert_eq!(cw5_output["caught"], true);
    assert_eq!(cw5_output["marker"]["name"], "Error");
    assert_eq!(
        step_rows(&fx, &cw5_parent).await,
        vec![
            (
                0,
                "ChildFailWorkflow".to_string(),
                "child".to_string(),
                "failed".to_string(),
            ),
            (
                1,
                "caught-child-failure".to_string(),
                "run".to_string(),
                "completed".to_string(),
            ),
        ]
    );

    // CW6: parent cancel cascades cooperatively by setting cancel_requested on
    // descendants; the child self-cancels on its next claim without dispatching
    // user code under a cross-run lease.
    let cw6_parent = seed_workflow_run(
        &fx,
        PARENT_CASCADE_WORKFLOW_NAME,
        serde_json::json!({"case": "cw6", "sleep": "PT30S"}),
    )
    .await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw17-cw6-park-parent"),
        &cw6_parent,
    )
    .await;
    let mut cw6_children = wait_for_child_count(&fx, &cw6_parent, 1).await;
    let cw6_child = cw6_children.remove(0);
    let cancel = post_control(
        &control_url,
        fx.app_id,
        &cw6_parent,
        "cancel",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(cancel["state"], "cancelled");
    fx.scheduler_store
        .ack_terminal(&cw6_parent)
        .await
        .expect("retire cancelled CW6 parent scheduler row");
    wait_for_child_cancel_requested(&fx, &cw6_child).await;
    let cw6_cfg = config("dw17-cw6-child-cooperative-cancel");
    let claimed = workflow_tick(&fx, &real_dispatcher, &cw6_cfg, "CW6 child cancel").await;
    assert_eq!(claimed, 1, "CW6 cancel pickup should send one light dispatch");
    assert_eq!(run_state(&fx.pg, &cw6_child).await.0, "cancelled");
    assert_eq!(
        workflow_step_count(&fx.pg, &cw6_child).await,
        0,
        "CW6 cancel pickup must not replay child workflow code"
    );

    // CW7: maxLiveDescendants rejects the child frontier as a catchable step
    // failure; this parent does not catch it, so the replay fails the run.
    let cw7_parent = seed_workflow_run(
        &fx,
        PARENT_CALL_WORKFLOW_NAME,
        serde_json::json!({"case": "cw7", "value": "over-cap"}),
    )
    .await;
    let cw7_restore = set_app_plan_runtime_limit(
        &fx,
        MAX_LIVE_DESCENDANTS_FIELD,
        serde_json::json!(0),
    )
    .await;
    let mut cw7_cfg = config("dw17-cw7-live-cap");
    cw7_cfg.max_live_descendants = 0;
    let cw7_error = drive_until_failed(
        &fx,
        Arc::clone(&real_dispatcher),
        cw7_cfg,
        &cw7_parent,
        "CW7 maxLiveDescendants",
    )
    .await;
    restore_app_plan_runtime_limits(&fx, cw7_restore).await;
    assert_eq!(cw7_error["type"], "LimitExceededError");
    assert!(child_run_ids(&fx, &cw7_parent).await.is_empty());
    assert_eq!(
        step_rows(&fx, &cw7_parent).await,
        vec![(
            0,
            "ChildEchoWorkflow".to_string(),
            "child".to_string(),
            "failed".to_string(),
        )]
    );

    let side_effect_run = seed_workflow_run(
        &fx,
        SIDE_EFFECT_WORKFLOW_NAME,
        serde_json::json!({"case": "side-effect"}),
    )
    .await;
    let side_effect_dispatcher = Arc::new(CountingDispatcher::new(gateway_url.clone()));
    drive_until_completed(
        &fx,
        Arc::clone(&side_effect_dispatcher),
        config("dw13-side-effect"),
        &side_effect_run,
    )
    .await;
    assert_side_effect_steps(&fx, &side_effect_run).await;
    let side_effect_counts = side_counts(&fx, &side_effect_run).await;
    assert_eq!(side_effect_counts.get("v").copied(), Some(1));
    assert_eq!(side_effect_counts.get("after").copied(), Some(1));
    assert!(
        side_effect_dispatcher.count() >= 2,
        "sideEffect replay proof should span multiple dispatches; got {}",
        side_effect_dispatcher.count()
    );
    let side_effect_output = run_output(&fx, &side_effect_run).await;
    assert_eq!(side_effect_output["v"]["step"], "v");
    // `bump`'s pre-insert count (see assert_side_effect_steps); the frozen
    // sideEffect value is replayed into the run output verbatim. Exactly-once
    // execution is proven by side_counts above.
    assert_eq!(side_effect_output["v"]["count"], 0);
    assert_eq!(side_effect_output["after"]["step"], "after");

    let name_divergence_run = seed_workflow_run(
        &fx,
        NAME_DIVERGENCE_WORKFLOW_NAME,
        serde_json::json!({"case": "name-divergence"}),
    )
    .await;
    seed_completed_step(
        &fx,
        &name_divergence_run,
        "expected",
        "run",
        serde_json::json!({"seeded": true}),
    )
    .await;
    let name_divergence_dispatcher = Arc::new(CountingDispatcher::new(gateway_url.clone()));
    let error = drive_until_failed(
        &fx,
        Arc::clone(&name_divergence_dispatcher),
        config("dw13-name-divergence"),
        &name_divergence_run,
        "name-divergence",
    )
    .await;
    assert_eq!(error["type"], "NondeterministicError");
    assert!(
        name_divergence_dispatcher.count() >= 1,
        "name-divergence workflow should have dispatched at least once"
    );
    assert_eq!(
        step_rows(&fx, &name_divergence_run).await,
        vec![(0, "expected".to_string(), "run".to_string(), "completed".to_string())],
        "name-divergence proof should preserve the seeded journal row"
    );
    assert_eq!(
        side_counts(&fx, &name_divergence_run)
            .await
            .get("actual")
            .copied(),
        None,
        "mismatched step callback must not execute"
    );

    let pause_run = seed_run(&fx, "pause-mid").await;
    let (pause_dispatcher, pause_outcome_rx, pause_release_tx) =
        CrashOnceDispatcher::new(gateway_url.clone());
    let pause_dispatcher = Arc::new(pause_dispatcher);
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&pause_dispatcher),
        config("dw07-pause-first"),
    )
    .await
    .expect("pause first tick");
    assert_eq!(claimed, 1);
    let pause_ack = expect_completed_ack(
        pause_outcome_rx.await.expect("pause real ack"),
        "pause first dispatch",
    );
    assert_ack_run(&pause_ack, &pause_run, "pause first dispatch");
    let before_pause = run_state(&fx.pg, &pause_run).await;
    assert_eq!(before_pause.0, "queued");
    assert_eq!(before_pause.2, None, "worker apply clears the claim before ack");
    assert_eq!(
        step_rows(&fx, &pause_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "worker apply lands the checkpoint before the held ack"
    );
    let pause_body = post_control(
        &control_url,
        fx.app_id,
        &pause_run,
        "pause",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(pause_body["state"], "paused");
    assert_eq!(side_counts(&fx, &pause_run).await.get("a").copied(), Some(1));
    assert_eq!(
        step_rows(&fx, &pause_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "pause sees the worker-applied checkpoint"
    );
    let _ = pause_release_tx.send(());
    for _ in 0..100 {
        let (state, _, claimed_by, _) = run_state(&fx.pg, &pause_run).await;
        if state == "paused" && claimed_by.is_none() && step_rows(&fx, &pause_run).await.len() == 1
        {
            break;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(run_state(&fx.pg, &pause_run).await.0, "paused");
    assert_eq!(
        step_rows(&fx, &pause_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "pause-mid-dispatch should land a checkpoint exactly once"
    );
    wait_for_scheduler_counts(&fx, &pause_run, (0, 0)).await;
    let skipped = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&real_dispatcher),
        config("dw07-paused-skip"),
    )
    .await
    .expect("paused skip tick");
    assert_eq!(
        skipped, 0,
        "paused checkpointed run should not leave stale scheduler work"
    );
    assert_eq!(run_state(&fx.pg, &pause_run).await.0, "paused");
    assert_eq!(
        step_rows(&fx, &pause_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "paused skip must not replay or apply"
    );
    let resume_body = post_control(
        &control_url,
        fx.app_id,
        &pause_run,
        "resume",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(resume_body["state"], "queued");
    let _ = wait_for_scheduler_timer(&fx, &pause_run).await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-pause-resume-drive"),
        &pause_run,
    )
    .await;
    assert_expected_steps(&fx, &pause_run).await;
    let pause_counts = side_counts(&fx, &pause_run).await;
    assert_eq!(pause_counts.get("a").copied(), Some(1));
    assert_eq!(pause_counts.get("b").copied(), Some(1));

    let cancel_run = seed_run(&fx, "cancel-mid").await;
    let (cancel_dispatcher, cancel_outcome_rx, cancel_release_tx) =
        CrashOnceDispatcher::new(gateway_url.clone());
    let cancel_dispatcher = Arc::new(cancel_dispatcher);
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&cancel_dispatcher),
        config("dw07-cancel-first"),
    )
    .await
    .expect("cancel first tick");
    assert_eq!(claimed, 1);
    let cancel_ack = expect_completed_ack(
        cancel_outcome_rx.await.expect("cancel real ack"),
        "cancel first dispatch",
    );
    assert_ack_run(&cancel_ack, &cancel_run, "cancel first dispatch");
    let cancel_body = post_control(
        &control_url,
        fx.app_id,
        &cancel_run,
        "cancel",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(cancel_body["state"], "cancelled");
    let _ = cancel_release_tx.send(());
    compio::time::sleep(Duration::from_millis(150)).await;
    let (state, wake_at, claimed_by, nonce) = run_state(&fx.pg, &cancel_run).await;
    assert_eq!(state, "cancelled");
    assert_eq!(wake_at, None);
    assert_eq!(claimed_by, None);
    assert_eq!(nonce, None);
    assert_eq!(
        step_rows(&fx, &cancel_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "cancel happens after the worker-applied checkpoint in the held-ack window"
    );
    let cancel_counts = side_counts(&fx, &cancel_run).await;
    assert_eq!(
        cancel_counts.get("a").copied(),
        Some(1),
        "the external side effect may have happened before cancel"
    );
    assert_eq!(cancel_counts.get("b").copied(), None);
    let skipped = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&real_dispatcher),
        config("dw07-cancelled-skip"),
    )
    .await
    .expect("cancelled skip tick");
    assert_eq!(
        skipped, 0,
        "cancelled run should already be retired from the scheduler store"
    );
    let (state, wake_at, claimed_by, nonce) = run_state(&fx.pg, &cancel_run).await;
    assert_eq!(state, "cancelled");
    assert_eq!(wake_at, None);
    assert_eq!(claimed_by, None);
    assert_eq!(nonce, None);
    assert_eq!(
        step_rows(&fx, &cancel_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "cancelled terminal-ack dispatch must not replay or apply"
    );
    wait_for_scheduler_counts(&fx, &cancel_run, (0, 0)).await;

    let crash_run = seed_run(&fx, "crash").await;
    let (crash_dispatcher, dropped_rx, release_tx) =
        CrashOnceDispatcher::new(gateway_url.clone());
    let crash_dispatcher = Arc::new(crash_dispatcher);

    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&crash_dispatcher),
        config("dw07-crash-first"),
    )
    .await
    .expect("crash first tick");
    assert_eq!(claimed, 1);

    let dropped_ack = expect_completed_ack(
        dropped_rx.await.expect("dropped real ack"),
        "crash first dispatch",
    );
    assert_ack_run(&dropped_ack, &crash_run, "crash first dispatch");
    assert_eq!(
        step_rows(&fx, &crash_run).await,
        vec![(0, "a".to_string(), "run".to_string(), "completed".to_string())],
        "worker applies step a before the held ack is released"
    );
    assert_eq!(side_counts(&fx, &crash_run).await.get("a").copied(), Some(1));

    let pre_takeover = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&real_dispatcher),
        config("dw07-pre-ttl"),
    )
    .await
    .expect("pre-ttl tick");
    assert_eq!(
        pre_takeover, 0,
        "a second tick during the live lease must not dispatch/checkpoint"
    );
    assert_eq!(
        step_rows(&fx, &crash_run).await.len(),
        1,
        "live-lease tick must not add another checkpoint"
    );

    compio::time::sleep(Duration::from_millis(1_650)).await;
    register_existing_run_timer(&fx, &crash_run).await;
    let takeover = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&real_dispatcher),
        config("dw07-takeover"),
    )
    .await
    .expect("takeover tick");
    assert_eq!(takeover, 1, "expired lease should be reclaimed");
    let (takeover_state, takeover_wake_at) =
        wait_for_any_state(&fx, &crash_run, &["queued", "sleeping"]).await;
    if takeover_state == "sleeping" {
        assert!(
            takeover_wake_at.is_some(),
            "redrive sleep parking must carry wake_at"
        );
    }
    assert_eq!(side_counts(&fx, &crash_run).await.get("a").copied(), Some(1));
    let redrive_rows = step_rows(&fx, &crash_run).await;
    assert_eq!(
        redrive_rows
            .iter()
            .filter(|(_, name, _, _)| name == "a")
            .count(),
        1,
        "redrive should keep exactly one a row: {redrive_rows:?}"
    );
    assert!(
        redrive_rows.len() <= 2
            && redrive_rows.first()
                == Some(&(0, "a".to_string(), "run".to_string(), "completed".to_string()))
            && redrive_rows
                .get(1)
                .is_none_or(|row| row.1 == "sleep" && row.2 == "sleep"),
        "redrive should only replay into the next sleep checkpoint: {redrive_rows:?}"
    );

    let _ = release_tx.send(());
    compio::time::sleep(Duration::from_millis(100)).await;
    let late_rows = step_rows(&fx, &crash_run).await;
    assert_eq!(
        late_rows
            .iter()
            .filter(|(_, name, _, _)| name == "a")
            .count(),
        1,
        "late held ack must not double-checkpoint a: {late_rows:?}"
    );

    drive_until_completed(&fx, Arc::clone(&real_dispatcher), config("dw07-crash-drive"), &crash_run)
        .await;
    assert_expected_steps(&fx, &crash_run).await;
    let crash_counts = side_counts(&fx, &crash_run).await;
    assert_eq!(crash_counts.get("a").copied(), Some(1));
    assert_eq!(crash_counts.get("b").copied(), Some(1));

    let blob_size = 1024 * 1024 + 17;
    let blob_run = seed_workflow_run(
        &fx,
        BLOB_OUTPUT_WORKFLOW_NAME,
        serde_json::json!({"size": blob_size}),
    )
    .await;
    let (blob_crash_dispatcher, blob_dropped_rx, blob_release_tx) =
        CrashOnceDispatcher::new(gateway_url.clone());
    let blob_crash_dispatcher = Arc::new(blob_crash_dispatcher);
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&blob_crash_dispatcher),
        config("dw15-blob-crash-first"),
    )
    .await
    .expect("blob crash first tick");
    assert_eq!(claimed, 1);
    let blob_ack = expect_completed_ack(
        blob_dropped_rx.await.expect("dropped blob ack"),
        "blob first dispatch",
    );
    assert_ack_run(&blob_ack, &blob_run, "blob first dispatch");
    let dropped_blob = fx
        .pg
        .query_one(
            "SELECT output_kind, output_hash, output \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND name = 'big'",
            &[&blob_run],
        )
        .await
        .expect("load held-ack blob step row");
    assert_eq!(dropped_blob.get::<_, String>("output_kind"), "blob");
    assert!(
        dropped_blob
            .get::<_, Option<serde_json::Value>>("output")
            .is_none(),
        "blob-backed checkpoint must not inline the large output"
    );
    let dropped_hash: String = dropped_blob
        .get::<_, Option<String>>("output_hash")
        .expect("held-ack checkpoint blob hash");
    let _ = blob_release_tx.send(());
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw15-blob-drive"),
        &blob_run,
    )
    .await;
    let blob_step = fx
        .pg
        .query_one(
            "SELECT output_kind, output_hash, output_size, output, output_content_type \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND name = 'big'",
            &[&blob_run],
        )
        .await
        .expect("load blob step row");
    assert_eq!(blob_step.get::<_, String>("output_kind"), "blob");
    let committed_hash: String = blob_step
        .get::<_, Option<String>>("output_hash")
        .expect("committed blob hash");
    assert_eq!(committed_hash, dropped_hash);
    assert!(blob_step.get::<_, Option<serde_json::Value>>("output").is_none());
    assert_eq!(
        blob_step
            .get::<_, Option<String>>("output_content_type")
            .as_deref(),
        Some("application/json")
    );
    let (step_status, step_bytes) =
        get_output_bytes(&control_url, fx.app_id, &blob_run, Some("big"), None).await;
    assert_eq!(step_status, 200);
    assert_eq!(
        blob_step.get::<_, Option<i64>>("output_size"),
        Some(step_bytes.len() as i64)
    );
    let step_json: serde_json::Value =
        serde_json::from_slice(&step_bytes).expect("blob step output json");
    assert_eq!(
        step_json["payload"].as_str().expect("payload string").len(),
        blob_size
    );
    let blob_output = run_output(&fx, &blob_run).await;
    assert_eq!(step_json["digest"], blob_output["digest"]);
    assert_eq!(blob_output["len"].as_u64(), Some(blob_size as u64));
    let (run_status, run_bytes) =
        get_output_bytes(&control_url, fx.app_id, &blob_run, None, None).await;
    assert_eq!(run_status, 200);
    let run_read_json: serde_json::Value =
        serde_json::from_slice(&run_bytes).expect("run output json");
    assert_eq!(run_read_json, blob_output);
    let (range_status, range_bytes) = get_output_bytes(
        &control_url,
        fx.app_id,
        &blob_run,
        Some("big"),
        Some("bytes=0-31"),
    )
    .await;
    assert_eq!(range_status, 206);
    assert_eq!(range_bytes.as_slice(), &step_bytes[..32]);
    let object = fx
        .state
        .workflow_blob_store
        .get_blob(&committed_hash)
        .await
        .expect("workflow blob object");
    assert_eq!(object.as_ref(), step_bytes.as_slice());
    let ref_row = fx
        .pg
        .query_one(
            "SELECT refcount, size \
               FROM zeroship.workflow_blobs \
              WHERE hash = $1",
            &[&committed_hash],
        )
        .await
        .expect("load blob ref row");
    assert_eq!(ref_row.get::<_, i32>("refcount"), 1);
    assert_eq!(ref_row.get::<_, i64>("size"), step_bytes.len() as i64);

    let stream_blob_files_before = count_regular_files(&fx.blob_root.join("wfblob"));
    let stream_run = seed_workflow_run(
        &fx,
        STREAM_LIMIT_WORKFLOW_NAME,
        serde_json::json!({"size": 2_097_153}),
    )
    .await;
    let stream_error = drive_until_failed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw15-stream-limit"),
        &stream_run,
        "stream-limit",
    )
    .await;
    assert_eq!(stream_error["type"], "LimitExceededError");
    assert!(
        step_rows(&fx, &stream_run).await.is_empty(),
        "over-limit stream output must not commit a blob-backed step row"
    );
    assert_eq!(
        count_regular_files(&fx.blob_root.join("wfblob")),
        stream_blob_files_before,
        "over-limit stream output must clean partial workflow blob writes"
    );

    let gc_guard_run = seed_workflow_run(
        &fx,
        BLOB_OUTPUT_WORKFLOW_NAME,
        serde_json::json!({"size": blob_size + 31}),
    )
    .await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw15-gc-guard-drive"),
        &gc_guard_run,
    )
    .await;
    let guard_hash: String = fx
        .pg
        .query_one(
            "SELECT output_hash \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND name = 'big'",
            &[&gc_guard_run],
        )
        .await
        .expect("load guard blob hash")
        .get::<_, Option<String>>("output_hash")
        .expect("guard blob hash");
    let old_ref = Utc::now()
        - ChronoDuration::seconds(workflow_blob_gc::REF_SWEEP_GRACE_SECS + 60);
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_blobs \
                SET refcount = 0, last_referenced_at = $2 \
              WHERE hash = $1",
            &[&guard_hash, &old_ref],
        )
        .await
        .expect("age referenced blob row");
    let guarded_deleted = workflow_blob_gc::tick_ref_sweep(&fx.state)
        .await
        .expect("run guarded ref sweep");
    assert_eq!(
        guarded_deleted.deleted, 0,
        "ref sweep must not delete a blob still referenced by a step row"
    );
    assert!(fx
        .state
        .workflow_blob_store
        .get_blob(&guard_hash)
        .await
        .is_ok());

    let reclaim_run = seed_workflow_run(
        &fx,
        BLOB_OUTPUT_WORKFLOW_NAME,
        serde_json::json!({"size": blob_size + 43}),
    )
    .await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw15-reclaim-drive"),
        &reclaim_run,
    )
    .await;
    let reclaim_hash: String = fx
        .pg
        .query_one(
            "SELECT output_hash \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND name = 'big'",
            &[&reclaim_run],
        )
        .await
        .expect("load reclaim blob hash")
        .get::<_, Option<String>>("output_hash")
        .expect("reclaim blob hash");
    let restart = post_control(
        &control_url,
        fx.app_id,
        &reclaim_run,
        "restart",
        serde_json::json!({"from": {"name": "big"}}),
    )
    .await;
    assert_eq!(restart["runId"], reclaim_run);
    assert_eq!(restart["state"], "queued");
    assert_eq!(restart["restartedFromOrdinal"], 0);
    assert!(
        step_rows(&fx, &reclaim_run).await.is_empty(),
        "restart from blob step must drop the blob-backed step row"
    );
    let reclaim_refcount: i32 = fx
        .pg
        .query_one(
            "SELECT refcount \
               FROM zeroship.workflow_blobs \
              WHERE hash = $1",
            &[&reclaim_hash],
        )
        .await
        .expect("load reclaim refcount")
        .get("refcount");
    assert_eq!(reclaim_refcount, 0);
    fx.pg
        .execute(
            "UPDATE zeroship.workflow_blobs \
                SET last_referenced_at = $2 \
              WHERE hash = $1",
            &[&reclaim_hash, &old_ref],
        )
        .await
        .expect("age reclaim blob row");
    let deploy_blob_files_before_gc = count_regular_files(&fx.blob_root.join("blobs"));
    let manifest_files_before_gc = count_regular_files(&fx.blob_root.join("manifests"));
    let reclaimed = workflow_blob_gc::tick_ref_sweep(&fx.state)
        .await
        .expect("run reclaim ref sweep");
    assert_eq!(reclaimed.deleted, 1);
    assert!(fx
        .state
        .workflow_blob_store
        .get_blob(&reclaim_hash)
        .await
        .is_err());
    assert_eq!(
        count_regular_files(&fx.blob_root.join("blobs")),
        deploy_blob_files_before_gc,
        "workflow blob GC must not touch deploy bundle blobs"
    );
    assert_eq!(
        count_regular_files(&fx.blob_root.join("manifests")),
        manifest_files_before_gc,
        "workflow blob GC must not touch deploy manifests"
    );

    let signal_run = seed_signal_run(&fx, "signal", "PT30S", Some("PT5S")).await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-signal-park"),
        &signal_run,
    )
    .await;
    let _signal_deadline = assert_signal_wait_parked(&fx, &signal_run, Some(5_000)).await;
    let signal_counts = side_counts(&fx, &signal_run).await;
    assert_eq!(signal_counts.get("a").copied(), Some(1));
    assert_eq!(signal_counts.get("b").copied(), None);

    let signal_body = post_signal(
        &control_url,
        fx.app_id,
        &signal_run,
        serde_json::json!({"ok": true, "source": "e2e"}),
    )
    .await;
    let signal_id = signal_body["id"]
        .as_str()
        .expect("signal endpoint id")
        .to_string();
    assert!(signal_id.starts_with("sig_"));
    let after_signal = fx
        .pg
        .query_one(
            "SELECT r.wake_at, s.payload, s.consumed_by \
               FROM zeroship.workflow_runs r \
               JOIN zeroship.workflow_signals s ON s.run_id = r.id \
              WHERE r.id = $1 AND s.id = $2",
            &[&signal_run, &signal_id],
        )
        .await
        .expect("load delivered signal");
    let pulled_wake: DateTime<Utc> = after_signal.get("wake_at");
    assert!(
        pulled_wake <= Utc::now() + ChronoDuration::seconds(1),
        "signal endpoint should pull wake_at to now, got {pulled_wake:?}"
    );
    assert_eq!(
        after_signal.get::<_, serde_json::Value>("payload"),
        serde_json::json!({"ok": true, "source": "e2e"})
    );
    assert_eq!(after_signal.get::<_, Option<String>>("consumed_by"), None);

    register_existing_run_timer(&fx, &signal_run).await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-signal-drive"),
        &signal_run,
    )
    .await;
    assert_signal_success_steps(&fx, &signal_run).await;
    let signal_counts = side_counts(&fx, &signal_run).await;
    assert_eq!(signal_counts.get("a").copied(), Some(1));
    assert_eq!(signal_counts.get("b").copied(), Some(1));
    let consumed = fx
        .pg
        .query_one(
            "SELECT consumed_by FROM zeroship.workflow_signals WHERE id = $1",
            &[&signal_id],
        )
        .await
        .expect("load consumed signal");
    assert_eq!(
        consumed.get::<_, Option<String>>("consumed_by").as_deref(),
        Some(signal_run.as_str())
    );
    let wait_output = fx
        .pg
        .query_one(
            "SELECT output, consumed_signal_id \
               FROM zeroship.workflow_steps \
              WHERE run_id = $1 AND ordinal = 1",
            &[&signal_run],
        )
        .await
        .expect("load signal wait step");
    let output: serde_json::Value = wait_output.get("output");
    assert_eq!(output["payload"], serde_json::json!({"ok": true, "source": "e2e"}));
    assert_eq!(
        wait_output
            .get::<_, Option<String>>("consumed_signal_id")
            .as_deref(),
        Some(signal_id.as_str())
    );
    assert_eq!(
        run_output(&fx, &signal_run).await["signal"]["payload"],
        serde_json::json!({"ok": true, "source": "e2e"})
    );

    let external_run = seed_signal_run(&fx, "external-signal", "PT30S", None).await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw16-external-park"),
        &external_run,
    )
    .await;
    let external_token =
        create_run_signal_token(&control_url, fx.app_id, &external_run, "PT30S").await;
    let (status, external_body) = post_public_signal(
        &gateway_url,
        &external_token,
        serde_json::json!({"ok": true, "source": "public-ingress"}),
    )
    .await;
    assert_eq!(status, 202, "public signal should be accepted: {external_body}");
    let external_signal_id = external_body["id"]
        .as_str()
        .expect("public signal id")
        .to_string();
    register_existing_run_timer(&fx, &external_run).await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw16-external-drive"),
        &external_run,
    )
    .await;
    assert_signal_success_steps(&fx, &external_run).await;
    assert_eq!(
        run_output(&fx, &external_run).await["signal"]["payload"],
        serde_json::json!({"ok": true, "source": "public-ingress"})
    );
    let external_signal = fx
        .pg
        .query_one(
            "SELECT origin, delivery, idempotency_key, consumed_by \
               FROM zeroship.workflow_signals WHERE id = $1",
            &[&external_signal_id],
        )
        .await
        .expect("load external signal");
    assert_eq!(external_signal.get::<_, String>("origin"), "ingress");
    assert_eq!(external_signal.get::<_, String>("delivery"), "direct");
    assert!(
        external_signal
            .get::<_, Option<String>>("idempotency_key")
            .is_some(),
        "external token delivery must store a replay key"
    );
    assert_eq!(
        external_signal
            .get::<_, Option<String>>("consumed_by")
            .as_deref(),
        Some(external_run.as_str())
    );

    let replay = post_public_signal(
        &gateway_url,
        &external_token,
        serde_json::json!({"ok": true, "source": "replay"}),
    )
    .await;
    assert_eq!(replay.0, 409, "replayed token should be rejected: {}", replay.1);

    let forged_run = seed_signal_run(&fx, "forged-token", "PT30S", None).await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw16-forged-park"),
        &forged_run,
    )
    .await;
    let forged_token = create_run_signal_token(&control_url, fx.app_id, &forged_run, "PT30S").await;
    let mut forged_bytes = forged_token.into_bytes();
    let last = forged_bytes.last_mut().expect("token bytes");
    *last = if *last == b'a' { b'b' } else { b'a' };
    let forged_token = String::from_utf8(forged_bytes).expect("forged token utf8");
    let forged = post_public_signal(
        &gateway_url,
        &forged_token,
        serde_json::json!({"ok": false}),
    )
    .await;
    assert!(
        forged.0 == 401 || forged.0 == 403,
        "forged token should be rejected with 401/403, got {} body {}",
        forged.0,
        forged.1
    );

    let expired_run = seed_signal_run(&fx, "expired-token", "PT5M", None).await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw16-expired-park"),
        &expired_run,
    )
    .await;
    let expired_token = create_run_signal_token(&control_url, fx.app_id, &expired_run, "PT1S").await;
    // Wait past TTL(1s) + timestamp-tolerance(1s) with margin for integer-second
    // rounding: exp = mint_second+1, so the token is only strictly expired once
    // now_second >= mint_second+3. 2.25s lands on the mint_second+2 boundary and
    // flakes; 3.5s clears it deterministically for any sub-second mint alignment.
    compio::time::sleep(Duration::from_millis(3_500)).await;
    let expired = post_public_signal(
        &gateway_url,
        &expired_token,
        serde_json::json!({"ok": false}),
    )
    .await;
    assert!(
        expired.0 == 401 || expired.0 == 403,
        "expired token should be rejected with 401/403, got {} body {}",
        expired.0,
        expired.1
    );

    let terminal_run = seed_signal_run(&fx, "terminal-token", "PT30S", None).await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw16-terminal-park"),
        &terminal_run,
    )
    .await;
    let terminal_token =
        create_run_signal_token(&control_url, fx.app_id, &terminal_run, "PT30S").await;
    let _ = post_signal(
        &control_url,
        fx.app_id,
        &terminal_run,
        serde_json::json!({"ok": true, "source": "internal"}),
    )
    .await;
    register_existing_run_timer(&fx, &terminal_run).await;
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw16-terminal-drive"),
        &terminal_run,
    )
    .await;
    let terminal = post_public_signal(
        &gateway_url,
        &terminal_token,
        serde_json::json!({"ok": false}),
    )
    .await;
    assert!(
        terminal.0 == 404 || terminal.0 == 409,
        "terminal run signal should be rejected with 404/409, got {} body {}",
        terminal.0,
        terminal.1
    );

    let topic = format!("dw16-topic-{}", Uuid::new_v4().simple());
    let topic_runs = vec![
        seed_topic_signal_run(&fx, "topic-a", &topic).await,
        seed_topic_signal_run(&fx, "topic-b", &topic).await,
        seed_topic_signal_run(&fx, "topic-c", &topic).await,
    ];
    for (idx, run_id) in topic_runs.iter().enumerate() {
        drive_until_waiting(
            &fx,
            Arc::clone(&real_dispatcher),
            config(&format!("dw16-topic-park-{idx}")),
            run_id,
        )
        .await;
    }
    let sub_count: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_subscriptions \
              WHERE app_id = $1 AND topic = $2",
            &[&fx.app_id, &topic],
        )
        .await
        .expect("count topic subscriptions")
        .get("n");
    assert_eq!(sub_count, 3, "three runs should subscribe to the topic");

    let topic_token = create_topic_signal_token(&control_url, fx.app_id, &topic, "PT30S").await;
    let (topic_status, topic_body) = post_public_signal(
        &gateway_url,
        &topic_token,
        serde_json::json!({"ok": true, "source": "topic-ingress"}),
    )
    .await;
    assert_eq!(
        topic_status, 202,
        "topic public signal should create a broadcast: {topic_body}"
    );
    let broadcast_id = topic_body["id"]
        .as_str()
        .expect("broadcast id")
        .to_string();
    assert!(broadcast_id.starts_with("wbc_"));

    let first_fanout = workflow_signal_fanout::tick_with_config(
        &fx.state,
        FanoutSweepConfig {
            max_broadcasts_per_tick: 1,
            max_deliveries_per_broadcast: 2,
        },
    )
    .await
    .expect("first fanout tick");
    assert_eq!(first_fanout.broadcasts, 1);
    assert_eq!(first_fanout.deliveries, 2);
    let first_delivered_rows = fx
        .pg
        .query(
            "SELECT run_id, COUNT(*)::bigint AS n \
               FROM zeroship.workflow_signals \
              WHERE broadcast_id = $1 \
              GROUP BY run_id \
              ORDER BY run_id",
            &[&broadcast_id],
        )
        .await
        .expect("load partial broadcast deliveries");
    assert_eq!(
        first_delivered_rows.len(),
        2,
        "DW19 fanout crash point should commit only a partial subscriber set"
    );
    for row in &first_delivered_rows {
        assert_eq!(
            row.get::<_, i64>("n"),
            1,
            "partial fanout must not duplicate delivery for {:?}",
            row.get::<_, String>("run_id")
        );
    }
    let pending_state: String = fx
        .pg
        .query_one(
            "SELECT fanout_state FROM zeroship.workflow_broadcasts WHERE id = $1",
            &[&broadcast_id],
        )
        .await
        .expect("load partial broadcast state")
        .get("fanout_state");
    assert_eq!(
        pending_state, "pending",
        "partial fan-out should leave broadcast pending for re-drain"
    );
    let second_fanout = workflow_signal_fanout::tick_with_config(
        &fx.state,
        FanoutSweepConfig {
            max_broadcasts_per_tick: 1,
            max_deliveries_per_broadcast: 100,
        },
    )
    .await
    .expect("second fanout tick");
    assert_eq!(second_fanout.broadcasts, 1);
    assert_eq!(second_fanout.deliveries, 1);
    let third_fanout = workflow_signal_fanout::tick_with_config(
        &fx.state,
        FanoutSweepConfig {
            max_broadcasts_per_tick: 1,
            max_deliveries_per_broadcast: 100,
        },
    )
    .await
    .expect("idempotent fanout tick");
    assert_eq!(third_fanout.deliveries, 0);
    let completed_state: String = fx
        .pg
        .query_one(
            "SELECT fanout_state FROM zeroship.workflow_broadcasts WHERE id = $1",
            &[&broadcast_id],
        )
        .await
        .expect("load completed broadcast state")
        .get("fanout_state");
    assert_eq!(completed_state, "completed");
    for run_id in &topic_runs {
        let _ = wait_for_scheduler_timer(&fx, run_id).await;
    }
    let delivered_rows = fx
        .pg
        .query(
            "SELECT run_id, COUNT(*)::bigint AS n \
               FROM zeroship.workflow_signals \
              WHERE broadcast_id = $1 \
              GROUP BY run_id \
              ORDER BY run_id",
            &[&broadcast_id],
        )
        .await
        .expect("load broadcast deliveries");
    assert_eq!(delivered_rows.len(), 3, "broadcast should reach all subscribers");
    for row in &delivered_rows {
        assert_eq!(
            row.get::<_, i64>("n"),
            1,
            "redrain must not duplicate delivery for {:?}",
            row.get::<_, String>("run_id")
        );
    }
    for (idx, run_id) in topic_runs.iter().enumerate() {
        drive_until_completed(
            &fx,
            Arc::clone(&real_dispatcher),
            config(&format!("dw16-topic-drive-{idx}")),
            run_id,
        )
        .await;
        assert_topic_success_steps(&fx, run_id).await;
        let output = run_output(&fx, run_id).await;
        assert_eq!(
            output["signal"]["payload"],
            serde_json::json!({"ok": true, "source": "topic-ingress"})
        );
        assert_eq!(output["signal"]["delivery"], "topic");
        assert_eq!(output["signal"]["topic"], topic);
        let counts = side_counts(&fx, run_id).await;
        assert_eq!(counts.get("a").copied(), Some(1));
        assert_eq!(counts.get("b").copied(), Some(1));
    }
    let remaining_subs: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_subscriptions \
              WHERE app_id = $1 AND topic = $2",
            &[&fx.app_id, &topic],
        )
        .await
        .expect("count remaining topic subscriptions")
        .get("n");
    assert_eq!(
        remaining_subs, 0,
        "completed topic waits should delete their subscriptions"
    );

    let timeout_run = seed_signal_run(&fx, "timeout", "PT1S", None).await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-timeout-park"),
        &timeout_run,
    )
    .await;
    let timeout_deadline = assert_signal_wait_parked(&fx, &timeout_run, None).await;
    assert!(
        timeout_deadline <= Utc::now() + ChronoDuration::seconds(2),
        "short timeout should park within ~1s, got deadline {timeout_deadline:?}"
    );
    let early_claim = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&real_dispatcher),
        config("dw07-timeout-early"),
    )
    .await
    .expect("early timeout tick");
    assert_eq!(early_claim, 0, "timeout must not claim before waiting deadline");
    assert_eq!(run_state(&fx.pg, &timeout_run).await.0, "waiting");
    let now = Utc::now();
    if timeout_deadline > now {
        let wait_ms = timeout_deadline
            .signed_duration_since(now)
            .num_milliseconds()
            .saturating_add(125)
            .max(0) as u64;
        compio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-timeout-drive"),
        &timeout_run,
    )
    .await;
    assert_signal_timeout_steps(&fx, &timeout_run).await;
    let timeout_wait = fx
        .pg
        .query_one(
            "SELECT error FROM zeroship.workflow_steps WHERE run_id = $1 AND ordinal = 1",
            &[&timeout_run],
        )
        .await
        .expect("timeout wait step");
    let timeout_error: serde_json::Value = timeout_wait.get("error");
    assert_eq!(timeout_error["type"], "WorkflowTimeoutError");
    let timeout_output = run_output(&fx, &timeout_run).await;
    assert_eq!(timeout_output["state"], "timeout");
    assert_eq!(timeout_output["errorName"], "WorkflowTimeoutError");
    let timeout_counts = side_counts(&fx, &timeout_run).await;
    assert_eq!(timeout_counts.get("a").copied(), Some(1));
    assert_eq!(timeout_counts.get("timeout").copied(), Some(1));
    assert_eq!(timeout_counts.get("b").copied(), None);

    let stale_run = seed_signal_run(&fx, "stale", "PT2S", Some("PT0.2S")).await;
    let stale_signal_id = zeroship_core::typed_id::new_workflow_signal_id();
    let stale_created_at = Utc::now() - ChronoDuration::seconds(5);
    fx.pg
        .execute(
            "INSERT INTO zeroship.workflow_signals \
                (id, run_id, type, payload, created_at, origin, delivery) \
             VALUES ($1, $2, 'go', $3, $4, 'app', 'direct')",
            &[
                &stale_signal_id,
                &stale_run,
                &serde_json::json!({"stale": true}),
                &stale_created_at,
            ],
        )
        .await
        .expect("insert stale signal");
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-stale-park"),
        &stale_run,
    )
    .await;
    let stale_deadline = assert_signal_wait_parked(&fx, &stale_run, Some(200)).await;
    assert!(
        stale_deadline <= Utc::now() + ChronoDuration::seconds(3),
        "stale short timeout should park within ~2s, got deadline {stale_deadline:?}"
    );
    let now = Utc::now();
    if stale_deadline > now {
        let wait_ms = stale_deadline
            .signed_duration_since(now)
            .num_milliseconds()
            .saturating_add(125)
            .max(0) as u64;
        compio::time::sleep(Duration::from_millis(wait_ms)).await;
    }
    drive_until_completed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw07-stale-drive"),
        &stale_run,
    )
    .await;
    assert_signal_timeout_steps(&fx, &stale_run).await;
    let stale_signal = fx
        .pg
        .query_one(
            "SELECT consumed_by FROM zeroship.workflow_signals WHERE id = $1",
            &[&stale_signal_id],
        )
        .await
        .expect("load stale signal");
    assert_eq!(
        stale_signal
            .get::<_, Option<String>>("consumed_by")
            .as_deref(),
        Some(stale_run.as_str()),
        "stale over-age signal should be marked consumed without satisfying the wait"
    );
    let stale_counts = side_counts(&fx, &stale_run).await;
    assert_eq!(stale_counts.get("a").copied(), Some(1));
    assert_eq!(stale_counts.get("timeout").copied(), Some(1));
    assert_eq!(stale_counts.get("b").copied(), None);

    let cascade_redeploy_parent = seed_workflow_run(
        &fx,
        PARENT_MANY_CASCADE_WORKFLOW_NAME,
        serde_json::json!({
            "case": "dw19-cascade-redeploy",
            "sleep": "PT5M",
            "values": ["one", "two"],
        }),
    )
    .await;
    drive_until_waiting(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw19-cascade-parent-park"),
        &cascade_redeploy_parent,
    )
    .await;
    let cascade_children = wait_for_child_count(&fx, &cascade_redeploy_parent, 2).await;
    assert_child_dedup_keys(&fx, &cascade_redeploy_parent, 2).await;
    drive_children_until_sleeping(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw19-cascade-children-park"),
        &cascade_children,
    )
    .await;
    for child in &cascade_children {
        assert_eq!(
            side_counts(&fx, child).await.get("child-start").copied(),
            Some(1),
            "child should execute its pre-sleep effect once before cascade cancel"
        );
    }

    let cascade_manifest_raw: String = fx
        .pg
        .query_one(
            "SELECT manifest_json FROM zeroship.apps WHERE id = $1",
            &[&fx.app_id],
        )
        .await
        .expect("load app manifest before cascade redeploy")
        .get::<_, Option<String>>("manifest_json")
        .expect("manifest json");
    let cascade_redeploy_hash = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let redeploy = fx.state.registry.set_deploy_with_manifest(
        &fx.app_id,
        &cascade_redeploy_hash,
        &cascade_manifest_raw,
        None,
    );
    let cancel = post_control(
        &control_url,
        fx.app_id,
        &cascade_redeploy_parent,
        "cancel",
        serde_json::json!({}),
    );
    let (redeploy, cancel) = futures::future::join(redeploy, cancel).await;
    assert!(
        redeploy.expect("DW19 cascade redeploy"),
        "redeploy should bump the active deploy hash"
    );
    assert_eq!(cancel["state"], "cancelled");
    fx.scheduler_store
        .ack_terminal(&cascade_redeploy_parent)
        .await
        .expect("retire cancelled cascade parent scheduler row");
    let active_hash: Option<String> = fx
        .pg
        .query_one(
            "SELECT deploy_hash FROM zeroship.apps WHERE id = $1",
            &[&fx.app_id],
        )
        .await
        .expect("load active deploy hash after cascade redeploy")
        .get("deploy_hash");
    assert_eq!(active_hash.as_deref(), Some(cascade_redeploy_hash.as_str()));
    for child in &cascade_children {
        wait_for_child_cancel_requested(&fx, child).await;
    }
    let cancel_dispatcher = Arc::new(CountingDispatcher::new(gateway_url.clone()));
    drive_children_until_cancelled(
        &fx,
        Arc::clone(&cancel_dispatcher),
        config("dw19-cascade-child-cancel"),
        &cascade_children,
    )
    .await;
    assert_eq!(
        active_child_count_for_deploy(&fx, &cascade_redeploy_parent, &fx.deploy_id).await,
        0,
        "no child on the parent deploy pin should remain active after cascade cancel"
    );
    assert_eq!(run_state(&fx.pg, &cascade_redeploy_parent).await.0, "cancelled");
    for (_, state, claimed_by, deploy_id) in child_states(&fx, &cascade_children).await {
        assert_eq!(state, "cancelled");
        assert_eq!(claimed_by, None);
        assert_eq!(deploy_id, fx.deploy_id);
    }

    let manifest_raw: String = fx
        .pg
        .query_one(
            "SELECT manifest_json FROM zeroship.apps WHERE id = $1",
            &[&fx.app_id],
        )
        .await
        .expect("load app manifest")
        .get::<_, Option<String>>("manifest_json")
        .expect("manifest json");
    let mut manifest_without_schedule: serde_json::Value =
        serde_json::from_str(&manifest_raw).expect("manifest json parses");
    manifest_without_schedule
        .as_object_mut()
        .expect("manifest object")
        .remove("schedules");
    let redeploy_hash = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let redeploy_manifest =
        serde_json::to_string(&manifest_without_schedule).expect("serialize redeploy manifest");
    let updated = fx
        .state
        .registry
        .set_deploy_with_manifest(&fx.app_id, &redeploy_hash, &redeploy_manifest, None)
        .await
        .expect("redeploy without schedule");
    assert!(updated, "redeploy should update app");
    let schedule_count: i64 = fx
        .pg
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_schedules \
              WHERE app_id = $1 AND name = 'dw14-scheduled'",
            &[&fx.app_id],
        )
        .await
        .expect("count schedules after redeploy")
        .get("n");
    assert_eq!(
        schedule_count, 0,
        "redeploy without the schedule must delete the registry row"
    );

    // Teardown: the fixture holds the sole Postgres connection this test
    // opens (every dispatcher here talks to the real gateway over HTTP, not
    // the database directly), and locals are dropped only after the body
    // returns - by which point the runtime is gone and the socket can no
    // longer be closed. Drop it explicitly, then wait for the close to land.
    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn bare_await_body_io_is_rejected() {
    require_dw_e2e_fleet();

    let db_url = required_env("a test database", zeroship_core::config::test_database_url_opt());
    let gateway_url = required_env("ZEROSHIP_DW_E2E_GATEWAY_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_GATEWAY_URL"));
    let app_id: Uuid = required_env("ZEROSHIP_DW_E2E_APP_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_APP_ID"))
        .parse()
        .expect("app id uuid");
    let deploy_id = required_env("ZEROSHIP_DW_E2E_DEPLOY_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_DEPLOY_ID"));
    let fx = build_fixture(&db_url, &gateway_url, app_id, deploy_id).await;
    prepare_side_effect_table(&fx.pg).await;
    // `prepare_side_effect_table` still runs so the `first` step has somewhere to
    // write if it ever gets that far - it must not. BareAwaitWorkflow's body-level
    // `fetch` is rejected synchronously by the dispatch-scoped I/O guard
    // (`installWorkflowIoGuards` replaces `globalThis.fetch` and throws
    // NondeterministicError in body mode - sdks/workflows/src/journal.ts:250-254,
    // :263-273) before the real fetch runs, and therefore before the SSRF floor
    // ever sees the URL. That ordering is what keeps this arm measuring the guard
    // rather than the network, and it is why the fixture's body-level call stayed
    // a `fetch` when the step bodies moved to `env.db`.

    let bare_await_run = seed_workflow_run(
        &fx,
        BARE_AWAIT_WORKFLOW_NAME,
        serde_json::json!({"case": "bare-await"}),
    )
    .await;
    let bare_dispatcher = Arc::new(CountingDispatcher::new(gateway_url.clone()));
    let error = drive_until_failed(
        &fx,
        Arc::clone(&bare_dispatcher),
        config("dw13-bare-await"),
        &bare_await_run,
        "bare-await",
    )
    .await;
    assert_eq!(error["type"], "NondeterministicError");
    assert!(
        bare_dispatcher.count() >= 1,
        "bare-await workflow should have dispatched at least once"
    );

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn scheduler_misfire_lost_register_recovers() {
    require_dw_e2e_fleet();

    let db_url = required_env("a test database", zeroship_core::config::test_database_url_opt());
    let gateway_url = required_env("ZEROSHIP_DW_E2E_GATEWAY_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_GATEWAY_URL"));
    let app_id: Uuid = required_env("ZEROSHIP_DW_E2E_APP_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_APP_ID"))
        .parse()
        .expect("app id uuid");
    let deploy_id = required_env("ZEROSHIP_DW_E2E_DEPLOY_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_DEPLOY_ID"));

    let fx = build_fixture(&db_url, &gateway_url, app_id, deploy_id).await;
    prepare_side_effect_table(&fx.pg).await;

    let run_id = seed_workflow_run(
        &fx,
        WORKFLOW_NAME,
        serde_json::json!({"case": "scheduler-misfire"}),
    )
    .await;
    let first_claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(GatewayStepDispatcher::new(gateway_url.clone(), test_control_service_auth())),
        single_dispatch_config("scheduler-misfire-a"),
    )
    .await
    .expect("misfire first normal tick");
    assert_eq!(first_claimed, 1);
    wait_for_step_count(&fx, &run_id, 1).await;
    let (state, _, claimed_by, dispatch_nonce) = run_state(&fx.pg, &run_id).await;
    assert_eq!(state, "queued");
    assert_eq!(claimed_by, None);
    assert_eq!(dispatch_nonce, None);
    wait_for_scheduler_timer(&fx, &run_id).await;

    let (held_dispatcher, held_rx, release_tx) = CrashOnceDispatcher::new(gateway_url.clone());
    let held_dispatcher = Arc::new(held_dispatcher);
    let first_owner = "scheduler-misfire-sleep";
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&held_dispatcher),
        single_dispatch_config(first_owner),
    )
    .await
    .expect("misfire sleep tick");
    assert_eq!(claimed, 1);

    let first_ack = expect_completed_ack(
        held_rx.await.expect("misfire held dispatch ack"),
        "misfire first dispatch",
    );
    assert_ack_run(&first_ack, &run_id, "misfire first dispatch");

    let (state, wake_at, claimed_by, dispatch_nonce) = run_state(&fx.pg, &run_id).await;
    let _wake_at = wake_at.expect("sleeping run wake_at");
    assert_eq!(state, "sleeping");
    assert_eq!(claimed_by, None);
    assert_eq!(dispatch_nonce, None);
    assert_eq!(
        scheduler_counts(&fx, &run_id).await,
        (0, 1),
        "lost register window should leave only the fire-time inflight row"
    );

    force_inflight_deadline_elapsed(&fx, &run_id).await;
    let reaped = workflow_engine::reap_lapsed_inflight_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::new(GatewayStepDispatcher::new(gateway_url.clone(), test_control_service_auth())),
        single_dispatch_config("scheduler-misfire-reaper"),
        8,
    )
    .await
    .expect("misfire reaper tick");
    assert_eq!(reaped, 1, "lapsed inflight row should redispatch once");

    let _timer_wake_at = wait_for_scheduler_timer(&fx, &run_id).await;
    let _ = release_tx.send(());
    assert_eq!(
        scheduler_counts(&fx, &run_id).await,
        (1, 0),
        "reaper replay should replace inflight with the future timer"
    );

    drive_until_completed(
        &fx,
        Arc::new(GatewayStepDispatcher::new(gateway_url, test_control_service_auth())),
        single_dispatch_config("scheduler-misfire-complete"),
        &run_id,
    )
    .await;
    let counts = side_counts(&fx, &run_id).await;
    assert_eq!(counts.get("a").copied(), Some(1));
    assert_eq!(counts.get("b").copied(), Some(1));
    wait_for_scheduler_counts(&fx, &run_id, (0, 0)).await;

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn scheduler_overfire_duplicate_dispatch_noops() {
    require_dw_e2e_fleet();

    let db_url = required_env("a test database", zeroship_core::config::test_database_url_opt());
    let gateway_url = required_env("ZEROSHIP_DW_E2E_GATEWAY_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_GATEWAY_URL"));
    let app_id: Uuid = required_env("ZEROSHIP_DW_E2E_APP_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_APP_ID"))
        .parse()
        .expect("app id uuid");
    let deploy_id = required_env("ZEROSHIP_DW_E2E_DEPLOY_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_DEPLOY_ID"));

    let fx = build_fixture(&db_url, &gateway_url, app_id, deploy_id).await;
    prepare_side_effect_table(&fx.pg).await;

    let run_id = seed_workflow_run(
        &fx,
        SINGLE_COMMIT_WORKFLOW_NAME,
        serde_json::json!({"case": "scheduler-overfire"}),
    )
    .await;
    force_run_due(&fx, &run_id).await;
    let (held_dispatcher, held_rx, release_tx) = CrashOnceDispatcher::new(gateway_url.clone());
    let held_dispatcher = Arc::new(held_dispatcher);
    let stale_owner = "scheduler-overfire-stale";
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&held_dispatcher),
        single_dispatch_config(stale_owner),
    )
    .await
    .expect("overfire first tick");
    assert_eq!(claimed, 1);

    let stale_ack = expect_completed_ack(
        held_rx.await.expect("overfire held dispatch ack"),
        "overfire first dispatch",
    );
    assert_ack_run(&stale_ack, &run_id, "overfire first dispatch");

    force_claim_lease_elapsed(&fx, &run_id).await;
    force_run_due(&fx, &run_id).await;
    assert_eq!(
        scheduler_counts(&fx, &run_id).await,
        (1, 0),
        "duplicate registered timer should replace the stale inflight row"
    );
    let duplicate_dispatcher = Arc::new(CountingDispatcher::new(gateway_url.clone()));
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&duplicate_dispatcher),
        duplicate_dispatch_config("scheduler-overfire-winner"),
    )
    .await
    .expect("overfire duplicate tick");
    assert_eq!(claimed, 1, "duplicate registered timer should dispatch once");
    wait_for_dispatch_count(&duplicate_dispatcher, 1).await;
    assert_eq!(
        duplicate_dispatcher.count(),
        1,
        "duplicate path must use the real gateway dispatcher exactly once"
    );

    let (duplicate_state, duplicate_wake_at) =
        wait_for_any_state(&fx, &run_id, &["queued", "completed"]).await;
    if duplicate_state == "queued" {
        assert!(
            duplicate_wake_at.is_some(),
            "queued duplicate dispatch must carry a follow-up wake"
        );
    }
    let _ = release_tx.send(());
    compio::time::sleep(Duration::from_millis(150)).await;
    let redrive_dispatcher = Arc::new(GatewayStepDispatcher::new(gateway_url.clone(), test_control_service_auth()));
    drive_until_completed(
        &fx,
        redrive_dispatcher,
        config("scheduler-overfire-redrive"),
        &run_id,
    )
    .await;

    assert_eq!(
        step_rows(&fx, &run_id).await,
        vec![(0, "once".to_string(), "run".to_string(), "completed".to_string())],
        "duplicate dispatch must leave one memoized step row"
    );
    assert_eq!(
        effect_attempt_counts(&fx, &run_id).await.get("once").copied(),
        Some(1),
        "duplicate dispatch should replay the worker-applied journal hit without re-entering the action boundary"
    );
    assert_eq!(
        effect_commit_counts(&fx, &run_id).await.get("once").copied(),
        Some(1),
        "idempotent side effect should commit once despite duplicate dispatch"
    );
    wait_for_scheduler_counts(&fx, &run_id, (0, 0)).await;

    drop(fx);
    common::drain_pg().await;
}

#[compio::test]
#[serial]
async fn compensation_saga_rollback_real_spine() {
    require_dw_e2e_fleet();

    let db_url = required_env("a test database", zeroship_core::config::test_database_url_opt());
    let control_url = required_env("ZEROSHIP_DW_E2E_CONTROL_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_CONTROL_URL"));
    let gateway_url = required_env("ZEROSHIP_DW_E2E_GATEWAY_URL", zeroship_core::test_env!("ZEROSHIP_DW_E2E_GATEWAY_URL"));
    let app_id: Uuid = required_env("ZEROSHIP_DW_E2E_APP_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_APP_ID"))
        .parse()
        .expect("app id uuid");
    let deploy_id = required_env("ZEROSHIP_DW_E2E_DEPLOY_ID", zeroship_core::test_env!("ZEROSHIP_DW_E2E_DEPLOY_ID"));

    let fx = build_fixture(&db_url, &gateway_url, app_id, deploy_id).await;
    prepare_side_effect_table(&fx.pg).await;
    let real_dispatcher = Arc::new(GatewayStepDispatcher::new(gateway_url.clone(), test_control_service_auth()));

    let failed_run = seed_workflow_run(
        &fx,
        COMPENSATION_WORKFLOW_NAME,
        serde_json::json!({"case": "fail"}),
    )
    .await;
    let error = drive_until_failed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw18-failure-rollback"),
        &failed_run,
        "DW18 failure rollback",
    )
    .await;
    assert_eq!(error["compensation"]["outcome"], "completed");
    assert_eq!(
        ordered_side_effect_steps(&fx, &failed_run).await,
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(
        ordered_commit_steps(&fx, &failed_run).await,
        vec!["undo:b".to_string(), "undo:a".to_string()],
        "completed compensators must run in reverse ordinal order"
    );
    assert_eq!(
        compensation_step_rows(&fx, &failed_run).await,
        vec![
            (0, "a".to_string(), Some("completed".to_string()), 1),
            (1, "b".to_string(), Some("completed".to_string()), 1),
        ]
    );
    let (state, target, outcome, _) = run_compensation_status(&fx, &failed_run).await;
    assert_eq!(state, "failed");
    assert_eq!(target.as_deref(), Some("failed"));
    assert_eq!(outcome.as_deref(), Some("completed"));

    let (restart_status, restart_body) = post_control_raw(
        &control_url,
        fx.app_id,
        &failed_run,
        "restart",
        serde_json::json!({"from": {"name": "b"}}),
    )
    .await;
    assert_eq!(
        restart_status, 409,
        "restart past settled compensation should be rejected: {restart_body:?}"
    );
    assert_eq!(restart_body["error"], "RestartError");

    let crash_run = seed_workflow_run(
        &fx,
        COMPENSATION_WORKFLOW_NAME,
        serde_json::json!({"case": "crash"}),
    )
    .await;
    drive_until_compensating(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw18-crash-enter"),
        &crash_run,
        "DW18 crash enter",
    )
    .await;
    let (drop_dispatcher, dropped_rx) = DropOnceDispatcher::new(gateway_url.clone());
    let drop_dispatcher = Arc::new(drop_dispatcher);
    let claimed = workflow_engine::fire_once(
        &fx.scheduler_store,
        &fx.state,
        Arc::clone(&drop_dispatcher),
        config("dw18-crash-drop"),
    )
    .await
    .expect("DW18 crash-drop tick");
    assert_eq!(claimed, 1);
    let dropped_ack = expect_completed_ack(
        dropped_rx.await.expect("DW18 dropped compensation ack"),
        "DW18 dropped compensation",
    );
    assert_ack_run(&dropped_ack, &crash_run, "DW18 dropped compensation");
    compio::time::sleep(Duration::from_millis(150)).await;
    let attempts_after_drop = effect_attempt_counts(&fx, &crash_run).await;
    let commits_after_drop = effect_commit_counts(&fx, &crash_run).await;
    assert_eq!(attempts_after_drop.get("undo:b").copied(), Some(1));
    assert_eq!(commits_after_drop.get("undo:b").copied(), Some(1));

    drive_until_failed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw18-crash-redrive"),
        &crash_run,
        "DW18 crash redrive",
    )
    .await;
    let attempts = effect_attempt_counts(&fx, &crash_run).await;
    let commits = effect_commit_counts(&fx, &crash_run).await;
    assert_eq!(
        attempts.get("undo:b").copied(),
        Some(1),
        "worker-applied lost ack should not redrive a committed compensator effect"
    );
    assert_eq!(
        commits.get("undo:b").copied(),
        Some(1),
        "idempotency marker must commit b's compensation effect once"
    );
    assert_eq!(attempts.get("undo:a").copied(), Some(1));
    assert_eq!(commits.get("undo:a").copied(), Some(1));
    assert_eq!(
        ordered_commit_steps(&fx, &crash_run).await,
        vec!["undo:b".to_string(), "undo:a".to_string()],
        "crash-redriven compensators must still finish in reverse order"
    );
    assert_eq!(
        compensation_step_rows(&fx, &crash_run).await,
        vec![
            (0, "a".to_string(), Some("completed".to_string()), 1),
            (1, "b".to_string(), Some("completed".to_string()), 1),
        ]
    );
    let (state, target, outcome, _) = run_compensation_status(&fx, &crash_run).await;
    assert_eq!(state, "failed");
    assert_eq!(target.as_deref(), Some("failed"));
    assert_eq!(outcome.as_deref(), Some("completed"));

    let nondet_run = seed_workflow_run(
        &fx,
        COMPENSATION_DIVERGENCE_WORKFLOW_NAME,
        serde_json::json!({"case": "nondeterministic"}),
    )
    .await;
    seed_compensable_completed_step(
        &fx,
        &nondet_run,
        "expected",
        serde_json::json!({"step": "expected"}),
    )
    .await;
    let nondet_error = drive_until_failed(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw18-nondet"),
        &nondet_run,
        "DW18 nondeterministic fail-closed",
    )
    .await;
    assert_eq!(nondet_error["type"], "NondeterministicError");
    assert!(
        effect_commit_counts(&fx, &nondet_run).await.is_empty(),
        "nondeterministic replay must not run compensators off an untrusted prefix"
    );
    let (state, target, outcome, _) = run_compensation_status(&fx, &nondet_run).await;
    assert_eq!(state, "failed");
    assert!(target.is_none());
    assert!(outcome.is_none());

    let cancel_run = seed_workflow_run(
        &fx,
        COMPENSATION_WORKFLOW_NAME,
        serde_json::json!({"case": "cancel-compensate"}),
    )
    .await;
    drive_until_sleeping(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw18-cancel-park"),
        &cancel_run,
        "DW18 cancel park",
    )
    .await;
    let cancel = post_control(
        &control_url,
        fx.app_id,
        &cancel_run,
        "cancel",
        serde_json::json!({"mode": "compensate"}),
    )
    .await;
    assert_eq!(cancel["state"], "compensating");
    let _ = wait_for_scheduler_timer(&fx, &cancel_run).await;
    drive_until_cancelled(
        &fx,
        Arc::clone(&real_dispatcher),
        config("dw18-cancel-compensate"),
        &cancel_run,
        "DW18 cancel compensate",
    )
    .await;
    assert_eq!(
        ordered_commit_steps(&fx, &cancel_run).await,
        vec!["undo:b".to_string(), "undo:a".to_string()]
    );
    let (state, target, outcome, error) = run_compensation_status(&fx, &cancel_run).await;
    assert_eq!(state, "cancelled");
    assert_eq!(target.as_deref(), Some("cancelled"));
    assert_eq!(outcome.as_deref(), Some("completed"));
    assert_eq!(
        error
            .and_then(|e| e.get("compensation").cloned())
            .and_then(|c| c.get("outcome").cloned()),
        Some(serde_json::Value::String("completed".to_string()))
    );
    wait_for_scheduler_counts(&fx, &cancel_run, (0, 0)).await;

    drop(fx);
    common::drain_pg().await;
}

/// A control-plane identity for fixtures that drive a STUB gateway.
///
/// It MINTS and verifies nobody: the bundle is empty on purpose, because the
/// stub these tests point at does no verification and a fixture that pretended
/// otherwise would be asserting against itself. What it does bind is that the
/// dispatcher can produce a credential at all - an unconfigured `ServiceAuth`
/// turns every advance into backpressure, so without this the fixtures would
/// measure the mint failing rather than the workflow advancing.
fn test_control_service_auth() -> std::sync::Arc<zeroship_core::service_peers::ServiceAuth> {
    use zeroship_core::service_assertion::{
        ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier,
    };
    use zeroship_core::service_peers::{
        service_issuer, ServiceAuth, ServiceKeyring, CONTROL_SERVICE_NAME,
    };

    let issuer = service_issuer(CONTROL_SERVICE_NAME).expect("control issuer");
    let key = ServiceSigningKey::generate();
    let keyring = ServiceKeyring::from_parts(issuer, key, ServiceTrustBundle::new())
        .expect("control keyring");
    std::sync::Arc::new(ServiceAuth::new(
        keyring,
        std::sync::Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
    ))
}
