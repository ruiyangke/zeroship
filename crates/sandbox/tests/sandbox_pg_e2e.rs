//! Phase-0 sandbox-pg-state integration tests.
//!
//! These tests need a live Postgres. They are marked `#[ignore]` by
//! default so `cargo test -p zeroship-sandbox` stays self-contained;
//! to run them:
//!
//! ```bash
//! docker compose up -d postgres
//! PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
//!     cargo test -p zeroship-sandbox --test sandbox_pg_e2e -- --ignored --test-threads=1
//! ```
//!
//! Each test creates a unique schema, points the migration runner at
//! it via the proposal's "schema name is configurable" hatch
//! (Phase 0 hard-codes `sandbox`; we run each test against a fresh
//! database state by `DROP SCHEMA sandbox CASCADE` before the test).
//! Tests are serialised via `--test-threads=1` to avoid stomping on
//! each other's schema reset.

use std::time::Duration;

use compio_postgres::{NoTls, Pool, PoolConfig};
use zeroship_sandbox::db::{Database, LATEST_MIGRATION_VERSION};

const TEST_URL_ENV: &str = "PG_TEST_URL";
const DEFAULT_URL: &str = "postgres://postgres:zeroship@localhost:5440/zeroship";

fn test_url() -> String {
    std::env::var(TEST_URL_ENV).unwrap_or_else(|_| DEFAULT_URL.to_string())
}

/// Drop the `sandbox` schema if it exists. Idempotent.
async fn reset_schema(url: &str) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg)
        .await
        .expect("connect for reset");
    let client = pool.get().await.expect("acquire for reset");
    client
        .batch_execute("DROP SCHEMA IF EXISTS sandbox CASCADE")
        .await
        .expect("drop schema");
}

// ────────────────────────────────────────────────────────────────────
// 1. Migration up: fresh pg → run_pending_migrations → schema present
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; run with `docker compose up -d postgres`"]
async fn migration_up_creates_full_schema() {
    let url = test_url();
    reset_schema(&url).await;

    let db = Database::from_test_config(url.clone(), true, 30)
        .await
        .expect("from_test_config");

    let n = db.run_pending_migrations().await.expect("apply migrations");
    assert!(n >= 1, "expected at least one migration applied, got {n}");
    assert_eq!(
        db.current_schema_version().await.unwrap(),
        LATEST_MIGRATION_VERSION
    );

    // Spot-check the columns the design depends on. `generation` is
    // the round-6 CAS counter; if it disappears, the HA design is
    // broken.
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let row = client
        .query_one(
            "SELECT data_type FROM information_schema.columns
              WHERE table_schema = 'sandbox'
                AND table_name = 'sandboxes'
                AND column_name = 'generation'",
            &[],
        )
        .await
        .expect("generation column must exist");
    let ty: &str = row.get(0);
    assert_eq!(ty, "bigint", "generation must be BIGINT");

    // events is partitioned + has the BRIN index.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'sandbox' AND c.relname = 'events_default'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 1, "events_default partition must exist");

    // Six monthly partitions. relkind='r' filters indexes /
    // sequences off pg_class; we want only the six partition tables.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'sandbox'
                AND c.relkind = 'r'
                AND c.relname LIKE 'events\\_2026\\_%'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 6, "expected 6 monthly events partitions, got {n}");

    // BRIN index on events.ts.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_indexes
              WHERE schemaname = 'sandbox' AND indexname = 'idx_events_ts_brin'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 1, "BRIN index on events.ts must exist");

    // deleted_sandboxes tombstone table.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_tables
              WHERE schemaname = 'sandbox' AND tablename = 'deleted_sandboxes'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 1, "deleted_sandboxes tombstone must exist");
}

// ────────────────────────────────────────────────────────────────────
// 2. Idempotent migrate: applying twice is a no-op (round-2 fix)
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn migration_apply_is_idempotent() {
    let url = test_url();
    reset_schema(&url).await;

    let db = Database::from_test_config(url.clone(), true, 30)
        .await
        .unwrap();

    let first = db.run_pending_migrations().await.expect("first apply");
    assert!(first >= 1);

    // Second invocation: every migration version is now <= MAX, so
    // nothing pending. UNIQUE on schema_migrations.version makes
    // this safe even if the DDL re-runs (it won't here because the
    // pending filter eliminates the migration upfront).
    let second = db.run_pending_migrations().await.expect("second apply");
    assert_eq!(second, 0, "second apply must be a no-op, got {second}");

    assert_eq!(
        db.current_schema_version().await.unwrap(),
        LATEST_MIGRATION_VERSION
    );
}

// ────────────────────────────────────────────────────────────────────
// 3. Concurrent migrators: two simultaneous SANDBOX_PG_RUN_MIGRATIONS=1
//    processes both succeed; only one row in schema_migrations.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn migration_concurrent_migrators_race() {
    let url = test_url();
    reset_schema(&url).await;

    let db_a = Database::from_test_config(url.clone(), true, 30)
        .await
        .unwrap();
    let db_b = Database::from_test_config(url.clone(), true, 30)
        .await
        .unwrap();

    // Compio is single-threaded; we can't truly run them in parallel
    // OS-thread-wise, but we can interleave their futures so the
    // batch_execute / INSERT race resolves via SQLSTATE 23505 on the
    // loser, exactly like the production race-tolerance fallback
    // sketch in § 7.1.
    let (a, b) = futures::join!(db_a.run_pending_migrations(), db_b.run_pending_migrations());
    a.expect("A apply succeeds");
    b.expect("B apply succeeds");

    // schema_migrations has exactly one row per version, no
    // duplicates — pg's PRIMARY KEY on `version` is the lock-free
    // arbiter (§ 7.1 race-tolerance fallback). Whether one or both
    // INSERTs ran, the row count is canonical.
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.schema_migrations",
            &[],
        )
        .await
        .unwrap();
    let n: i64 = row.get(0);
    assert_eq!(n, LATEST_MIGRATION_VERSION);
}

// ────────────────────────────────────────────────────────────────────
// 4. Regular controller waits for migrator
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn ensure_schema_at_version_blocks_until_migrator_applies() {
    let url = test_url();
    reset_schema(&url).await;

    // Migrator with run_migrations=true; non-migrator with =false
    // and a generous boot_timeout. Drive the non-migrator's wait
    // loop concurrently with the migrator's apply.
    let migrator = Database::from_test_config(url.clone(), true, 30)
        .await
        .unwrap();
    let observer = Database::from_test_config(url.clone(), false, 30)
        .await
        .unwrap();

    // Race the migrator against the observer's `ensure` — observer
    // starts first, sees version=0, sleeps 2s, repeats; migrator
    // applies in the meantime; observer's next poll sees version
    // >= target and returns Ok.
    let observer_fut = observer.ensure_schema_at_version(LATEST_MIGRATION_VERSION);
    // Give the observer one poll-cycle's head-start, then apply.
    let migrator_fut = async {
        compio::time::sleep(Duration::from_millis(500)).await;
        migrator.run_pending_migrations().await
    };

    let (obs, mig) = futures::join!(observer_fut, migrator_fut);
    obs.expect("observer ensure ok");
    mig.expect("migrator apply ok");

    assert_eq!(
        observer.current_schema_version().await.unwrap(),
        LATEST_MIGRATION_VERSION
    );
}

// ────────────────────────────────────────────────────────────────────
// 5. Boot timeout: regular controller times out without a migrator
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn ensure_schema_at_version_times_out_without_migrator() {
    let url = test_url();
    reset_schema(&url).await;

    // boot_timeout_secs=2 — no migrator runs; the wait loop polls
    // twice (initial + after 2s sleep) then gives up.
    let observer = Database::from_test_config(url, false, 2).await.unwrap();
    let err = observer
        .ensure_schema_at_version(LATEST_MIGRATION_VERSION)
        .await
        .expect_err("must time out");
    match err {
        zeroship_sandbox::db::DatabaseError::BootTimeout { required } => {
            assert_eq!(required, LATEST_MIGRATION_VERSION);
        }
        other => panic!("expected BootTimeout, got {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// 6. Connection-pool baseline: 16 concurrent ping connections
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn pool_baseline_handles_16_concurrent_ping() {
    let url = test_url();
    // Build a pool sized to the design's D-17 default.
    let mut cfg = PoolConfig::default();
    cfg.max_size = 16;
    let pool = Pool::connect_with_config(&url, cfg)
        .await
        .expect("connect pool");
    let mut futs = Vec::with_capacity(16);
    for _ in 0..16 {
        let pool_ref = &pool;
        futs.push(async move {
            let c = pool_ref.get().await.expect("acquire");
            let rows = c.query("SELECT 1::INTEGER", &[]).await.expect("query");
            assert_eq!(rows.len(), 1);
        });
    }
    futures::future::join_all(futs).await;

    // Database::ping smoke-test (also opens a transient pool internally).
    let db = Database::from_test_config(test_url(), false, 5)
        .await
        .unwrap();
    db.ping().await.expect("ping ok");
}

// Reference to NoTls so the use-import isn't dead.
#[allow(dead_code)]
fn _no_tls_anchor(_t: NoTls) {}
