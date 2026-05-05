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

// ════════════════════════════════════════════════════════════════════
// Phase 1 — pg-write round-trips (round-8: pg as system of record)
// ════════════════════════════════════════════════════════════════════

use uuid::Uuid;
use zeroship_sandbox::backend::SandboxInfo;
use zeroship_sandbox::db::{EventRow, ShareRow, SandboxStatus};

fn typed_id(prefix: &str) -> String {
    zeroship_core::typed_id::generate(prefix)
}

fn fresh_host(db: &Database) -> Uuid {
    db.host_id()
}

async fn migrated_db() -> Database {
    let url = test_url();
    reset_schema(&url).await;
    let db = Database::from_test_config(url, true, 30).await.unwrap();
    db.run_pending_migrations().await.unwrap();
    db.upsert_host("test-host", "nomad-ch").await.unwrap();
    db
}

fn fresh_info(host_user: &str) -> (SandboxInfo, Uuid) {
    let sandbox_id = Uuid::now_v7();
    let info = SandboxInfo {
        sandbox_id: format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        ),
        user_id: typed_id("usr"),
        project_id: typed_id("prj"),
        backend: "nomad-ch".into(),
        backend_hint: format!("job=zsbx-x vm_index=42 key_fp={}", "0".repeat(32)),
        created_at_secs: 1_700_000_000,
        last_used_at_secs: 1_700_000_000,
    };
    let _ = host_user;
    (info, sandbox_id)
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 round-trip"]
async fn insert_sandbox_and_list_for_host_round_trips_fields() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, _) = fresh_info("alice");
    let key_fp = "0123456789abcdef".repeat(2);
    db.insert_sandbox(&info, host_id, &key_fp, Some("http://10.99.142.2:7777"), Some(42))
        .await
        .expect("insert");
    let rows = db
        .list_running_sandboxes_for_host(host_id)
        .await
        .expect("list");
    let row = rows
        .iter()
        .find(|r| r.sandbox_id == info.sandbox_id)
        .expect("inserted row");
    assert_eq!(row.user_id, info.user_id);
    assert_eq!(row.project_id, info.project_id);
    assert_eq!(row.backend, "nomad-ch");
    assert_eq!(row.vm_index, Some(42));
    assert_eq!(row.agent_url.as_deref(), Some("http://10.99.142.2:7777"));
    assert_eq!(row.key_fp, key_fp);
    assert_eq!(row.status, SandboxStatus::Running);
    assert_eq!(row.generation, 0);
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 CAS"]
async fn update_status_increments_generation_and_cas_loses_on_stale() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sandbox_uuid) = fresh_info("alice");
    let key_fp = "0".repeat(32);
    db.insert_sandbox(&info, host_id, &key_fp, Some("http://10.99.142.2:7777"), Some(7))
        .await
        .unwrap();
    // Generation starts at 0; first update bumps to 1.
    let new_gen = db
        .update_sandbox_status(sandbox_uuid, SandboxStatus::Stopped, 0)
        .await
        .expect("first update");
    assert_eq!(new_gen, 1);
    // Stale generation 0 misses the CAS.
    let err = db
        .update_sandbox_status(sandbox_uuid, SandboxStatus::Lost, 0)
        .await
        .expect_err("stale CAS must miss");
    assert!(format!("{err:?}").contains("CAS missed"), "got {err:?}");
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 share insert + list"]
async fn insert_share_and_list_returns_metadata_no_secret() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sandbox_uuid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();
    let row = ShareRow {
        token_id: typed_id("tok"),
        sandbox_id: info.sandbox_id.clone(),
        port: 5173,
        scope: "ro".into(),
        secret_version: 1,
        issued_at_secs: 1_700_000_000,
        expires_at_secs: 1_700_003_600,
        iss: Some(info.user_id.clone()),
    };
    db.insert_share(&row).await.expect("insert share");
    let metas = db
        .list_shares_for_sandbox(sandbox_uuid, 5173)
        .await
        .expect("list shares");
    assert_eq!(metas.len(), 1);
    let m = &metas[0];
    assert_eq!(m.token_id, row.token_id);
    assert_eq!(m.port, 5173);
    assert_eq!(m.scope, "ro");
    assert_eq!(m.secret_version, 1);
    assert_eq!(m.iss.as_deref(), Some(info.user_id.as_str()));
    assert_eq!(m.use_count, 0);
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 rotate_share_secret"]
async fn rotate_share_secret_revokes_existing_rows() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sandbox_uuid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();
    for sv in 1..=3 {
        let row = ShareRow {
            token_id: typed_id("tok"),
            sandbox_id: info.sandbox_id.clone(),
            port: 5173,
            scope: "ro".into(),
            secret_version: sv,
            issued_at_secs: 1_700_000_000,
            expires_at_secs: 1_700_003_600,
            iss: None,
        };
        db.insert_share(&row).await.unwrap();
    }
    // Pre-rotate: 3 active rows visible.
    let pre = db.list_shares_for_sandbox(sandbox_uuid, 5173).await.unwrap();
    assert_eq!(pre.len(), 3);
    // Rotate: revoke them all.
    let n = db.rotate_share_secret(sandbox_uuid).await.unwrap();
    assert_eq!(n as usize, 3);
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 event size cap"]
async fn insert_event_round_trips_and_enforces_8kib_data_cap() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, _) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();
    // Small payload: passes.
    let small = EventRow {
        event_id: typed_id("evt"),
        sandbox_id: info.sandbox_id.clone(),
        user_id: info.user_id.clone(),
        kind: "created".into(),
        data_json: serde_json::json!({"backend": "nomad-ch"}).to_string(),
    };
    db.insert_event(&small).await.expect("small event ok");

    // Large payload (> 8 KiB JSONB binary form): rejected by the
    // CHECK (pg_column_size(data) <= 8192).
    let big_string = "x".repeat(9000);
    let big = EventRow {
        event_id: typed_id("evt"),
        sandbox_id: info.sandbox_id.clone(),
        user_id: info.user_id.clone(),
        kind: "stopped".into(),
        data_json: serde_json::json!({"reason": big_string}).to_string(),
    };
    let err = db
        .insert_event(&big)
        .await
        .expect_err("> 8 KiB must be rejected");
    let msg = format!("{err:?}");
    assert!(
        msg.contains("events_data_size") || msg.to_lowercase().contains("check"),
        "expected events_data_size CHECK violation, got: {msg}"
    );
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 delete tombstone"]
async fn delete_sandbox_moves_row_to_tombstone() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sandbox_uuid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();

    db.delete_sandbox(sandbox_uuid).await.expect("delete");

    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&test_url(), cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    // Original row gone.
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 0);
    // Tombstone present.
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.deleted_sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 1);
}

#[compio::test]
#[ignore = "needs Postgres; Phase-1 list_running excludes non-running"]
async fn list_running_sandboxes_filters_by_status_and_host() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    // Insert 3 sandboxes; stop one of them. The list query should
    // see 2.
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (info, sid) = fresh_info("alice");
        db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
            .await
            .unwrap();
        ids.push(sid);
    }
    // Stop one.
    db.update_sandbox_status(ids[1], SandboxStatus::Stopped, 0)
        .await
        .unwrap();
    let listed = db
        .list_running_sandboxes_for_host(host_id)
        .await
        .unwrap();
    assert_eq!(
        listed.len(),
        2,
        "list_running must exclude the stopped row"
    );
}
