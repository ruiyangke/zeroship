//! Integration tests for the spend-reconcile cron (billing PR5, ISS-31).
//!
//! Runs the REAL `cron::spend_reconcile::tick` against a live Postgres: it takes
//! the advisory lock (#2), runs `SpendEngine::evaluate_all`, and on a transition
//! writes the enriched `SpendStateChange` audit row (#8). No shims.
//!
//! Gated on `CONTROL_TEST_DB`; silent skip otherwise. The DB must have changeset
//! 0039 applied.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use zeroship_bundle::{BlobStore, LocalDiskBlobStore};
use zeroship_control::cron::spend_reconcile;
use zeroship_control::metering::Metering;
use zeroship_control::{
    AppState, EnvStore, Quota, RateLimiter, Registry, SecretString, StripeStore,
};
use zeroship_core::types::{AppUsage, UsageReport};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

/// `spend_reconcile::tick` single-flights the fleet-wide sweep via
/// `pg_try_advisory_lock`. The `..._skips_when_advisory_lock_held` test
/// deliberately HOLDS that lock for its duration, so a concurrent
/// `..._writes_enriched_spend_audit` tick would also skip (n == 0) and fail its
/// `n >= 1` assertion. Serialize the two with a process-wide lock (mirrors the
/// production single-flight; poison-recovered).
static SWEEP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const TEST_MASTER_KEY: &str = "test-master-key-deadbeefcafebabe";

fn tmpdir(label: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("zs-spend-{label}-{}", Uuid::new_v4().simple()));
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
    zeroship_control::bootstrap_console::seed_plans(&registry)
        .await
        .expect("seed built-in plans");
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
        tax_provider: zeroship_control::tax::build_tax_provider(
            &zeroship_control::tax::TaxProviderConfig::native(),
        )
        .expect("native tax provider builds"),
        notifier: std::sync::Arc::new(zeroship_control::notify::RecordingNotifier::new()),
        pairwise_salt: [0u8; 32],
    });

    Fixture {
        state,
        blob_root,
        deploy_tmp_dir,
    }
}

/// Seed a plan charging 1 cent/request with `limit` default, then an app on it.
/// CU pricing: global weight `requests` = 1 CU/op × fx 10^12 pico-cents/CU
/// (= 1 cent/CU) ⇒ 1 request = 1 cent.
async fn make_over_limit_app(state: &AppState, limit: i64) -> Uuid {
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
             VALUES ('requests', 1, 1) \
             ON CONFLICT (metric) DO UPDATE SET units_per_op = 1, per_units = 1",
            &[],
        )
        .await
        .expect("upsert requests weight");
    let plan_id = format!("pln_spend_{}", Uuid::new_v4().simple());
    let fx_one_cent: i64 = 1_000_000_000_000;
    state
        .control_pg
        .execute(
            "INSERT INTO zeroship.plans \
               (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                runtime_limits_json, spend_limit_default_cents) \
             VALUES ($1, 'spend-test', 0, 0, $3, \
                     '{\"cpu_limit_ms\":50,\"wall_timeout_ms\":5000,\"heap_limit_mb\":64}', $2)",
            &[&plan_id, &limit, &fx_one_cent],
        )
        .await
        .expect("seed priced plan");
    let name = format!("spend-{}", Uuid::new_v4());
    let rows = state
        .control_pg
        .query(
            "INSERT INTO zeroship.apps (name, plan_id, api_key, api_key_hash) \
             VALUES ($1, $2, $3, '') RETURNING id",
            &[&name, &plan_id, &Uuid::new_v4().to_string()],
        )
        .await
        .expect("insert app");
    rows[0].get("id")
}

fn report(worker: &str, seq: u64, app: Uuid, requests: u64) -> UsageReport {
    let mut counters = HashMap::new();
    counters.insert(app, AppUsage { requests, ..Default::default() });
    UsageReport {
        worker_id: worker.to_string(),
        report_id: Uuid::now_v7(),
        sequence: seq,
        counters,
    }
}

/// #8: a transition through the REAL cron `tick` writes a `SpendStateChange`
/// audit row whose detail carries the money context `{from,to,spend_cents,
/// limit_cents}` — matching the doc on `audit::Action::SpendStateChange`.
/// (This also exercises #2: `tick` takes + releases the advisory lock around
/// the sweep; a single instance acquires it and proceeds.)
#[compio::test]
async fn reconcile_tick_writes_enriched_spend_audit() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "audit").await;
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let metering = Metering::new(fx.state.registry.clone());

    // 100-cent cap, 1 cent/request, 100 requests ⇒ 100% ⇒ Allow→Block.
    let app = make_over_limit_app(&fx.state, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering.ingest(&report(&worker, 1, app, 100)).await.unwrap();

    let n = spend_reconcile::tick(&fx.state).await.expect("tick");
    assert!(n >= 1, "at least our app transitioned");

    // Read the audit row for our app.
    let rows = fx
        .state
        .control_pg
        .query(
            "SELECT action, detail::text AS detail FROM zeroship.app_audit \
             WHERE app_id = $1 AND action = 'spend_state_change'",
            &[&app],
        )
        .await
        .expect("read audit");
    assert_eq!(rows.len(), 1, "exactly one spend_state_change audit row");
    let detail: String = rows[0].get("detail");
    let v: serde_json::Value = serde_json::from_str(&detail).expect("detail is JSON");
    assert_eq!(v["from"], "allow", "records the from-state");
    assert_eq!(v["to"], "block", "records the to-state");
    assert_eq!(v["spend_cents"], 100, "audit detail carries spend_cents (#8)");
    assert_eq!(v["limit_cents"], 100, "audit detail carries limit_cents (#8)");
}

/// #2: the advisory lock is single-flight. While one connection holds
/// `pg_try_advisory_lock(<spend key>)`, a concurrent `tick` cannot acquire it
/// and SKIPS (returns 0) rather than racing a duplicate sweep. We hold the lock
/// on a side connection using the SAME key the cron uses, then assert the tick
/// no-ops even though an over-limit app is present.
#[compio::test]
async fn reconcile_tick_skips_when_advisory_lock_held() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let fx = build_state(&url, "lock").await;
    let _sweep = SWEEP_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let metering = Metering::new(fx.state.registry.clone());

    let app = make_over_limit_app(&fx.state, 100).await;
    let worker = format!("w-{}", Uuid::new_v4());
    metering.ingest(&report(&worker, 1, app, 100)).await.unwrap();

    // Hold the spend-sweep advisory lock on a dedicated side connection (same
    // key the cron derives — kept in sync with `spend_reconcile`). A fresh PG
    // session so the lock is genuinely held independently of the cron's conn.
    let (holder, holder_conn) = compio_postgres::connect(&url, compio_postgres::NoTls)
        .await
        .expect("holder connect");
    compio::runtime::spawn(async move {
        let _ = holder_conn.run().await;
    })
    .detach();
    let key: i64 = 0x7a73_7370_6e64_0001;
    let got = holder
        .query("SELECT pg_try_advisory_lock($1) AS locked", &[&key])
        .await
        .expect("acquire lock");
    assert!(got[0].get::<_, bool>("locked"), "side conn acquires the lock");

    // The cron tick must NOT run the sweep — the lock is held elsewhere.
    let n = spend_reconcile::tick(&fx.state).await.expect("tick skips");
    assert_eq!(n, 0, "tick skips when the advisory lock is held by another holder");

    // No transition was persisted (the sweep never ran).
    let state_rows = fx
        .state
        .control_pg
        .query(
            "SELECT 1 FROM zeroship.app_spend_state WHERE app_id = $1",
            &[&app],
        )
        .await
        .expect("read state");
    assert!(state_rows.is_empty(), "no spend state written while the lock was held");

    // Release the lock; now a tick proceeds and transitions the app.
    holder
        .execute("SELECT pg_advisory_unlock($1)", &[&key])
        .await
        .expect("unlock");
    let n2 = spend_reconcile::tick(&fx.state).await.expect("tick runs");
    assert!(n2 >= 1, "after release, the sweep runs and transitions our app");
}
