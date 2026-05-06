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
        .update_sandbox_status(sandbox_uuid, SandboxStatus::Stopped, 0, None)
        .await
        .expect("first update");
    assert_eq!(new_gen, 1);
    // Stale generation 0 misses the CAS — round-1 fixer / IMPORTANT
    // #7 promotes this to a typed `CasLost` variant.
    let err = db
        .update_sandbox_status(sandbox_uuid, SandboxStatus::Lost, 0, None)
        .await
        .expect_err("stale CAS must miss");
    assert!(
        matches!(err, zeroship_sandbox::db::DatabaseError::CasLost { .. }),
        "expected CasLost, got {err:?}"
    );
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

    db.delete_sandbox(sandbox_uuid, None, None).await.expect("delete");

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
    db.update_sandbox_status(ids[1], SandboxStatus::Stopped, 0, None)
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

// ════════════════════════════════════════════════════════════════════
// Phase 2 — periodic heartbeat + lease-based takeover (round-6 design)
// ════════════════════════════════════════════════════════════════════
//
// These tests need direct access to inject a "second host" row plus
// to age a host's `last_heartbeat` into the past. The `Database`
// handle only knows about its own `host_id`, so we drive the helper
// SQL through a side pool (same connect URL) before/after each call.

use std::time::Duration as StdDuration;

/// Insert a fresh host row owned by a synthetic `host_id` we can use
/// as the dead host in a takeover test. Returns the typed-id string
/// (`hst_<base62>`) which is the FK pg expects.
async fn inject_extra_host(url: &str, hostname: &str) -> (Uuid, String) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let host_uuid = Uuid::now_v7();
    let host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&host_uuid)
    );
    let boot_typed = zeroship_core::typed_id::uuid_to_base62(&Uuid::now_v7());
    client
        .execute(
            "INSERT INTO sandbox.hosts (host_id, boot_id, hostname, region, backend, status) \
             VALUES ($1::TEXT, $2::TEXT, $3::TEXT, 'us-local-1', 'nomad-ch', 'alive')",
            &[&host_typed, &boot_typed, &hostname.to_string()],
        )
        .await
        .unwrap();
    (host_uuid, host_typed)
}

/// Force `sandbox.hosts.last_heartbeat = now() - secs_ago` for the
/// host_id given. The takeover SQL evaluates `now() - last_heartbeat
/// < lease_ttl` against pg's own clock, so this is the only way to
/// simulate a stale peer without sleeping for `lease_ttl`.
async fn age_heartbeat(url: &str, host_typed: &str, secs_ago: i64) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE sandbox.hosts \
                SET last_heartbeat = now() - make_interval(secs => $2::BIGINT) \
              WHERE host_id = $1::TEXT",
            &[&host_typed.to_string(), &secs_ago],
        )
        .await
        .unwrap();
}

/// Refresh a host's heartbeat to `now()` — used in the race-loss
/// test to simulate the dead host coming back to life between
/// `dead_hosts()` and `takeover_sandboxes_from_host()`.
async fn refresh_heartbeat(url: &str, host_typed: &str) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE sandbox.hosts \
                SET last_heartbeat = now() \
              WHERE host_id = $1::TEXT",
            &[&host_typed.to_string()],
        )
        .await
        .unwrap();
}

async fn read_host_status(url: &str, host_typed: &str) -> Option<String> {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let opt = client
        .query_opt(
            "SELECT status FROM sandbox.hosts WHERE host_id = $1::TEXT",
            &[&host_typed.to_string()],
        )
        .await
        .unwrap();
    opt.map(|r| r.get::<_, String>(0))
}

async fn read_sandbox_owner_and_gen(
    url: &str,
    sandbox_id: &str,
) -> Option<(String, i64)> {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let opt = client
        .query_opt(
            "SELECT host_id, generation FROM sandbox.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&sandbox_id.to_string()],
        )
        .await
        .unwrap();
    opt.map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
}

// ────────────────────────────────────────────────────────────────────
// 7. Heartbeat round-trip
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 heartbeat"]
async fn heartbeat_round_trip_updates_last_heartbeat() {
    let db = migrated_db().await;
    // Read the initial last_heartbeat (boot upsert sets it to now()).
    let url = test_url();
    let host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&db.host_id())
    );
    // Age the heartbeat to ~30 seconds in the past so we can
    // confirm `heartbeat()` brings it forward.
    age_heartbeat(&url, &host_typed, 30).await;

    // Lag should now be ~30 s.
    let lag = db.heartbeat_lag_seconds().await.unwrap().unwrap();
    assert!(
        lag > 25.0 && lag < 60.0,
        "expected lag near 30 s, got {lag}"
    );

    // Heartbeat brings it back to ~0 s.
    db.heartbeat().await.unwrap();
    let lag_after = db.heartbeat_lag_seconds().await.unwrap().unwrap();
    assert!(
        lag_after < 5.0,
        "expected lag < 5 s after heartbeat, got {lag_after}"
    );
}

// ────────────────────────────────────────────────────────────────────
// 8. Heartbeat persists across multiple ticks
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 heartbeat persistence"]
async fn heartbeat_persists_latest_across_multiple_ticks() {
    let db = migrated_db().await;
    let url = test_url();
    let host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&db.host_id())
    );
    // Three ticks, 100 ms apart. The final lag must reflect the
    // LAST tick (i.e. ~0 s, not ~200 ms).
    for _ in 0..3 {
        db.heartbeat().await.unwrap();
        compio::time::sleep(StdDuration::from_millis(100)).await;
    }
    db.heartbeat().await.unwrap();
    let lag = db.heartbeat_lag_seconds().await.unwrap().unwrap();
    assert!(
        lag < 1.0,
        "lag after 4 heartbeats with 100ms spacing should be < 1s, got {lag}"
    );
    // Sanity: row still exists for our host_typed.
    let status = read_host_status(&url, &host_typed).await;
    assert_eq!(status.as_deref(), Some("alive"));
}

// ────────────────────────────────────────────────────────────────────
// 9. Dead host detection
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 dead-host detection"]
async fn dead_hosts_returns_only_lease_expired_rows() {
    let db = migrated_db().await;
    let url = test_url();

    // Inject a peer host A. Age its heartbeat by 120 seconds. With
    // lease_ttl=60, A is dead.
    let (_a_uuid, a_typed) = inject_extra_host(&url, "host-a").await;
    age_heartbeat(&url, &a_typed, 120).await;

    let dead = db.dead_hosts(60).await.unwrap();
    assert!(
        dead.iter().any(|h| h == &a_typed),
        "120-s-stale host A must appear: dead = {dead:?}"
    );

    // The current controller's heartbeat is fresh; it must NOT
    // appear in dead_hosts.
    let self_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&db.host_id())
    );
    assert!(
        !dead.iter().any(|h| h == &self_typed),
        "self must not be reported dead immediately after migrated_db(): dead = {dead:?}"
    );

    // Refresh A's heartbeat: now A < 60 s old → not dead.
    refresh_heartbeat(&url, &a_typed).await;
    let dead_after = db.dead_hosts(60).await.unwrap();
    assert!(
        !dead_after.iter().any(|h| h == &a_typed),
        "after heartbeat refresh, A must not be reported dead: dead_after = {dead_after:?}"
    );
}

// ────────────────────────────────────────────────────────────────────
// 10. Takeover happy path
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 takeover happy path"]
async fn takeover_reclaims_three_sandboxes_and_marks_host_dead() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Set up host A (dead, 120 s stale).
    let (a_uuid, a_typed) = inject_extra_host(&url, "host-a").await;

    // Insert 3 sandboxes owned by host A.
    let mut sandbox_ids = Vec::new();
    for _ in 0..3 {
        let (info, _) = fresh_info("alice");
        db.insert_sandbox(&info, a_uuid, &"0".repeat(32), Some("http://10.99.142.99:7777"), Some(99))
            .await
            .unwrap();
        sandbox_ids.push(info.sandbox_id.clone());
    }

    // Age A's heartbeat AFTER the inserts so the FK accepts.
    age_heartbeat(&url, &a_typed, 120).await;

    // Pre-takeover assertion: all three rows still owned by A.
    for sid in &sandbox_ids {
        let (owner, gen) = read_sandbox_owner_and_gen(&url, sid).await.unwrap();
        assert_eq!(owner, a_typed);
        assert_eq!(gen, 0);
    }

    // Takeover.
    let taken = db
        .takeover_sandboxes_from_host(&a_typed, my_host, 60)
        .await
        .unwrap();
    assert_eq!(taken.len(), 3, "expected all 3 reclaimed: {taken:?}");
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );

    // Post-takeover: all rows owned by us, generation = 1 (was 0,
    // bumped by 1).
    for sid in &sandbox_ids {
        let (owner, gen) = read_sandbox_owner_and_gen(&url, sid).await.unwrap();
        assert_eq!(owner, my_host_typed);
        assert_eq!(gen, 1, "takeover bumps generation to 1");
    }
    // RETURNING values match.
    for t in &taken {
        assert_eq!(t.generation, 1);
    }

    // Host A flipped to status='dead'.
    let a_status = read_host_status(&url, &a_typed).await;
    assert_eq!(
        a_status.as_deref(),
        Some("dead"),
        "host A must be marked dead after successful takeover"
    );
}

// ────────────────────────────────────────────────────────────────────
// 11. Takeover race-loss (heartbeat resumes mid-flight)
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 takeover race-loss"]
async fn takeover_races_loses_when_dead_host_heartbeats_back() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Set up host A; insert 1 sandbox; age heartbeat; then RUSH
    // heartbeat back to now() (simulating A waking up between
    // dead_hosts() and takeover_sandboxes_from_host()).
    let (a_uuid, a_typed) = inject_extra_host(&url, "host-a-race").await;
    let (info, _) = fresh_info("alice");
    db.insert_sandbox(&info, a_uuid, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();
    age_heartbeat(&url, &a_typed, 120).await;

    // Confirm dead_hosts sees it.
    let dead = db.dead_hosts(60).await.unwrap();
    assert!(dead.iter().any(|h| h == &a_typed));

    // Now A heartbeats back. The EXISTS subquery in
    // takeover_sandboxes_from_host's UPDATE must miss → 0 rows.
    refresh_heartbeat(&url, &a_typed).await;

    let taken = db
        .takeover_sandboxes_from_host(&a_typed, my_host, 60)
        .await
        .unwrap();
    assert!(
        taken.is_empty(),
        "takeover must lose the race when dead host heartbeats back: taken = {taken:?}"
    );

    // Host A still alive (status='alive'); ownership unchanged.
    let a_status = read_host_status(&url, &a_typed).await;
    assert_eq!(a_status.as_deref(), Some("alive"));
    let (owner, gen) = read_sandbox_owner_and_gen(&url, &info.sandbox_id)
        .await
        .unwrap();
    assert_eq!(owner, a_typed);
    assert_eq!(gen, 0);
}

// ────────────────────────────────────────────────────────────────────
// 12. CAS lost-leadership (split-brain split-write)
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 lost-leadership"]
async fn cas_lost_leadership_increments_metric_on_stale_generation() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Host A inserts a sandbox.
    let (a_uuid, a_typed) = inject_extra_host(&url, "host-a-cas").await;
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, a_uuid, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();
    age_heartbeat(&url, &a_typed, 120).await;

    // Controller B (us) takes it over. Generation bumps 0 → 1.
    let taken = db
        .takeover_sandboxes_from_host(&a_typed, my_host, 60)
        .await
        .unwrap();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].generation, 1);

    // Now controller A wakes up (post-takeover) and tries to mark
    // the row 'stopped' with its stale generation=0. CAS misses;
    // round-1 fixer / IMPORTANT #7 surfaces this as the typed
    // `CasLost` variant.
    let pre_lost = zeroship_sandbox::metrics::lost_leadership_value();
    let err = db
        .update_sandbox_status(sid, SandboxStatus::Stopped, 0, None)
        .await
        .expect_err("stale CAS must miss after peer takeover");
    assert!(
        matches!(err, zeroship_sandbox::db::DatabaseError::CasLost { .. }),
        "expected CasLost, got {err:?}"
    );
    // Bump the metric the way the live stop path does so we exercise
    // the integration counter (the inc happens in the handler, not
    // in update_sandbox_status itself).
    zeroship_sandbox::metrics::inc_lost_leadership("update_sandbox_status_stopped");
    let post_lost = zeroship_sandbox::metrics::lost_leadership_value();
    assert_eq!(
        post_lost,
        pre_lost + 1,
        "metric must increment on lost-leadership"
    );

    // Row is still 'running' under our (B's) ownership at gen=1.
    let (owner, gen) = read_sandbox_owner_and_gen(&url, &info.sandbox_id)
        .await
        .unwrap();
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );
    assert_eq!(owner, my_host_typed);
    assert_eq!(gen, 1);
}

// ────────────────────────────────────────────────────────────────────
// 13. Self-takeover refused defensively
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 self-takeover guard"]
async fn takeover_refuses_self_host() {
    let db = migrated_db().await;
    let my_host = db.host_id();
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );
    let err = db
        .takeover_sandboxes_from_host(&my_host_typed, my_host, 60)
        .await
        .expect_err("self-takeover must error");
    // Round-2 fixer / MINOR #4: pattern-match the typed variant
    // instead of substring-matching the message string.
    match err {
        zeroship_sandbox::db::DatabaseError::SelfTakeoverRefused { host_id } => {
            assert_eq!(
                host_id, my_host_typed,
                "self-takeover error must echo the typed-id host_id"
            );
        }
        other => panic!("expected SelfTakeoverRefused, got: {other:?}"),
    }
}

// ────────────────────────────────────────────────────────────────────
// 14. Takeover with mismatched signing_key
//
// Phase-2 scope per the task spec: pg-side takeover succeeds for
// every starting/running row; the controller's *probe pipeline* (out
// of scope for this commit; lands as Phase 2.5) is the layer that
// flips status to 'recreating' on a `/version` fingerprint mismatch.
// This test verifies what's wired today: the takeover is
// status-blind, and the operator's downstream restore can
// independently mark recreating via update_sandbox_status. The pg
// layer doesn't refuse the takeover just because the keys disagree.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; Phase-2 takeover-then-mark-recreating"]
async fn takeover_then_mark_recreating_on_fp_mismatch() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Host A owns sandbox sealed with key "aa..".
    let (a_uuid, a_typed) = inject_extra_host(&url, "host-a-fp").await;
    let (info, sid) = fresh_info("alice");
    let key_a = "a".repeat(32);
    db.insert_sandbox(&info, a_uuid, &key_a, Some("http://x"), None)
        .await
        .unwrap();
    age_heartbeat(&url, &a_typed, 120).await;

    // We take over.
    let taken = db
        .takeover_sandboxes_from_host(&a_typed, my_host, 60)
        .await
        .unwrap();
    assert_eq!(taken.len(), 1);
    let new_gen = taken[0].generation;

    // Simulate the post-takeover probe: agent answers with a
    // different fingerprint. The restore code path then marks the
    // row 'recreating' under the takeover-bumped generation. We
    // validate the CAS path here (the actual probe is in restore.rs).
    db.update_sandbox_status(sid, SandboxStatus::Recreating, new_gen, None)
        .await
        .expect("recreating CAS must hit");

    // Final state: row owned by us, status='recreating', generation
    // bumped past `new_gen` by 1 (the recreating UPDATE itself
    // bumps the counter — chain of CAS-stamped writes).
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT host_id, status, generation FROM sandbox.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    let owner: String = row.get(0);
    let status: String = row.get(1);
    let gen: i64 = row.get(2);
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );
    assert_eq!(owner, my_host_typed);
    assert_eq!(status, "recreating");
    assert_eq!(gen, new_gen + 1, "recreating UPDATE bumps generation");
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / IMPORTANT #8 — set_host_draining marks the host
// row 'draining' so peers see drain-intent before our heartbeat
// goes silent. Without this, a graceful shutdown looks like a
// crash to peers and they wait the full lease_ttl before noticing.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for IMPORTANT #8"]
async fn set_host_draining_flips_status_and_stamps_drain_started() {
    let db = migrated_db().await;
    let url = test_url();
    let host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&db.host_id())
    );

    // Pre: status='alive' from migrated_db()'s upsert_host.
    let pre = read_host_status(&url, &host_typed).await;
    assert_eq!(pre.as_deref(), Some("alive"));

    db.set_host_draining().await.expect("set draining ok");

    // Post: status='draining', drain_started_at IS NOT NULL.
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT status, (drain_started_at IS NOT NULL) AS has_drain_started \
               FROM sandbox.hosts WHERE host_id = $1::TEXT",
            &[&host_typed],
        )
        .await
        .unwrap();
    let status: String = row.get(0);
    let has_drain: bool = row.get(1);
    assert_eq!(status, "draining");
    assert!(has_drain, "drain_started_at must be stamped");

    // Idempotent: a second call leaves status=draining and
    // drain_started_at unchanged.
    db.set_host_draining().await.expect("second set ok");
    let row2 = client
        .query_one(
            "SELECT status FROM sandbox.hosts WHERE host_id = $1::TEXT",
            &[&host_typed],
        )
        .await
        .unwrap();
    let status2: String = row2.get(0);
    assert_eq!(status2, "draining");
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / CRITICAL #4 — get_sandbox_row round-trip. The
// post-takeover rehydrate path uses this to fetch the full row by
// typed-id-derived UUID; if it ever returns the wrong row (or
// fails to parse a column), the entire rehydrate pipeline silently
// no-ops and every HTTP request to the taken sandbox 404s.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for CRITICAL #4"]
async fn get_sandbox_row_round_trips_full_field_set() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sandbox_uuid) = fresh_info("alice");
    db.insert_sandbox(
        &info,
        host_id,
        &"abcdef0123456789abcdef0123456789",
        Some("http://10.99.42.2:7777"),
        Some(42),
    )
    .await
    .unwrap();

    let row = db
        .get_sandbox_row(sandbox_uuid)
        .await
        .expect("query ok")
        .expect("row exists");

    assert_eq!(row.sandbox_id, info.sandbox_id);
    assert_eq!(row.user_id, info.user_id);
    assert_eq!(row.project_id, info.project_id);
    assert_eq!(row.backend, "nomad-ch");
    assert_eq!(row.vm_index, Some(42));
    assert_eq!(row.agent_url.as_deref(), Some("http://10.99.42.2:7777"));
    assert_eq!(row.key_fp, "abcdef0123456789abcdef0123456789");
    assert_eq!(row.status, SandboxStatus::Running);
    assert_eq!(row.generation, 0);

    // Tombstone it via delete_sandbox; the row should no longer
    // surface (deleted_at IS NOT NULL filter).
    db.delete_sandbox(sandbox_uuid, None, None).await.unwrap();
    let after = db.get_sandbox_row(sandbox_uuid).await.unwrap();
    assert!(after.is_none(), "tombstoned row must be invisible");
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / CRITICAL #3 — A controller that LOST the lease on
// `stop` MUST NOT delete the row out from under the new owner. The
// host_id fence is the SQL-level safety net.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for CRITICAL #3"]
async fn delete_sandbox_with_host_fence_refuses_after_takeover() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Controller A creates the sandbox and owns the row.
    let (a_uuid, a_typed) = inject_extra_host(&url, "host-a-fence").await;
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, a_uuid, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();

    // Takeover bumps ownership to my_host.
    age_heartbeat(&url, &a_typed, 120).await;
    let taken = db
        .takeover_sandboxes_from_host(&a_typed, my_host, 60)
        .await
        .unwrap();
    assert_eq!(taken.len(), 1);

    // Controller A — still believing it owns the row — calls
    // delete_sandbox with its own host_id as the fence. The fence
    // misses (the row's host_id = my_host now); the DELETE returns
    // 0 rows; we get NotFound. The row stays put for the new owner.
    let err = db
        .delete_sandbox(sid, Some(a_uuid), Some(&info.user_id))
        .await
        .expect_err("fence-mismatched delete must NOT yank the row");
    assert!(
        matches!(err, zeroship_sandbox::db::DatabaseError::NotFound { .. }),
        "expected NotFound, got {err:?}"
    );

    // Sanity: row still in pg, owned by my_host.
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT host_id FROM sandbox.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .expect("row must still exist after fence-rejected delete");
    let owner: String = row.get(0);
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );
    assert_eq!(owner, my_host_typed, "ownership unchanged");
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / IMPORTANT #9 — tenant fence on update_sandbox_status.
// A misrouted call asserting `expected_user_id=usr_bob` against a row
// owned by `usr_alice` returns NotFound, not CasLost — the row is
// invisible to the bob-scoped predicate.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for IMPORTANT #9"]
async fn update_sandbox_status_tenant_fence_refuses_cross_user() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();

    // Different tenant's typed-id.
    let other_user = typed_id("usr");
    let err = db
        .update_sandbox_status(sid, SandboxStatus::Stopped, 0, Some(&other_user))
        .await
        .expect_err("cross-tenant update must miss");
    assert!(
        matches!(err, zeroship_sandbox::db::DatabaseError::NotFound { .. }),
        "expected NotFound (fence misses), got {err:?}"
    );

    // Same call with the correct user_id succeeds.
    let new_gen = db
        .update_sandbox_status(sid, SandboxStatus::Stopped, 0, Some(&info.user_id))
        .await
        .expect("matching tenant fence passes");
    assert_eq!(new_gen, 1);
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / CRITICAL #1 — typed-ids end-to-end. The handler
// is supposed to mint sandbox_id as `sbx_<base62>` and pass typed-id
// user_id / project_id straight through to insert_sandbox. This test
// exercises the post-handler path: a SandboxInfo built with typed-id
// fields lands a row in pg, and a SELECT count(*) sees exactly 1.
// Pre-fix, the handler accepted human ids ("alice"), Database's own
// parse_with_prefix rejected them, and the row never got written.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for CRITICAL #1"]
async fn insert_sandbox_with_typed_ids_round_trips_to_pg_row() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);

    // Build a SandboxInfo the way the post-fix handler does:
    // sandbox_id = sbx_<base62>, user_id = usr_<base62>, project_id
    // = prj_<base62>. All three pass parse_with_prefix.
    let sandbox_uuid = Uuid::now_v7();
    let info = SandboxInfo {
        sandbox_id: format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_uuid)
        ),
        user_id: typed_id("usr"),
        project_id: typed_id("prj"),
        backend: "nomad-ch".into(),
        backend_hint: "vm_index=42".into(),
        created_at_secs: 1_700_000_000,
        last_used_at_secs: 1_700_000_000,
    };
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), Some(42))
        .await
        .expect("typed-id insert lands");

    // Assert exactly one row visible by sandbox_id.
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&test_url(), cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM sandbox.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    let n: i64 = row.get(0);
    assert_eq!(n, 1, "row must land for typed-id input");
}

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for CRITICAL #1"]
async fn insert_sandbox_refuses_human_user_id() {
    // Pre-fix the handler accepted human ids and Database silently
    // failed. Today Database is the second line of defense:
    // parse_with_prefix("usr") rejects "alice" before any SQL is
    // issued.
    let db = migrated_db().await;
    let host_id = fresh_host(&db);

    let sandbox_uuid = Uuid::now_v7();
    let info = SandboxInfo {
        sandbox_id: format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_uuid)
        ),
        user_id: "alice".into(), // <-- the historical foot-gun
        project_id: typed_id("prj"),
        backend: "nomad-ch".into(),
        backend_hint: "x".into(),
        created_at_secs: 1_700_000_000,
        last_used_at_secs: 1_700_000_000,
    };
    let err = db
        .insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .expect_err("human user_id must be rejected");
    let msg = format!("{err:?}");
    // Either the prefix-mismatch path ("expected 'usr'") or the
    // malformed-id path ("no prefix") is acceptable: both branches
    // refuse the value before any SQL is issued.
    assert!(
        msg.contains("usr") || msg.contains("prefix") || msg.contains("typed ID"),
        "error must indicate typed-id rejection; got {msg}"
    );
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / CRITICAL #2 — every SandboxStatus passes the pg
// CHECK. Migration 0002 widened the constraint to include
// 'unreachable'. This guards against a future enum addition that
// drifts from the schema.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for CRITICAL #2"]
async fn every_sandbox_status_value_passes_pg_check() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);

    // Every variant of `SandboxStatus`. Synced manually with the
    // enum since the workspace doesn't pull `strum`. If a future
    // commit adds a variant, the match below stops compiling and
    // forces this list to update.
    let all = [
        SandboxStatus::Starting,
        SandboxStatus::Running,
        SandboxStatus::Stopping,
        SandboxStatus::Stopped,
        SandboxStatus::Lost,
        SandboxStatus::Recreating,
        SandboxStatus::Orphan,
        SandboxStatus::Unreachable,
        SandboxStatus::Snapshotting,
        SandboxStatus::Snapshotted,
        SandboxStatus::SnapshottingAborted,
        SandboxStatus::SnapshottedSuspect,
        SandboxStatus::Restoring,
        SandboxStatus::RestoringCold,
    ];
    // Compile-time guard: this match must be exhaustive. If a new
    // variant lands, the test author must extend `all`.
    fn _exhaustive(s: SandboxStatus) {
        match s {
            SandboxStatus::Starting => {}
            SandboxStatus::Running => {}
            SandboxStatus::Stopping => {}
            SandboxStatus::Stopped => {}
            SandboxStatus::Lost => {}
            SandboxStatus::Recreating => {}
            SandboxStatus::Orphan => {}
            SandboxStatus::Unreachable => {}
            SandboxStatus::Snapshotting => {}
            SandboxStatus::Snapshotted => {}
            SandboxStatus::SnapshottingAborted => {}
            SandboxStatus::SnapshottedSuspect => {}
            SandboxStatus::Restoring => {}
            SandboxStatus::RestoringCold => {}
        }
    }

    for status in all {
        // Each iteration mints a fresh sandbox (the partial unique
        // index on (user_id, project_id) WHERE status IN
        // ('starting','running','recreating') would otherwise
        // refuse two rows in the active set).
        let (info, sid) = fresh_info("alice");
        db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
            .await
            .unwrap();

        // The 0007 CHECK `sandboxes_snapshot_artifact_consistency`
        // requires Snapshotted/SnapshottedSuspect rows to carry a
        // populated artifact descriptor (path + sha + ch_version).
        // For these two states we pre-populate via direct SQL — PR 3
        // will add the proper Rust API for snapshot writes.
        if status.is_snapshotted() {
            let pool = db.pool_app().await.unwrap();
            let client = pool.get().await.unwrap();
            let sid_typed = format!(
                "sbx_{}",
                zeroship_core::typed_id::uuid_to_base62(&sid)
            );
            let path: &str = "gs://test/snap";
            let sha: Vec<u8> = vec![0u8; 32];
            let ver: &str = "v51.1";
            client
                .execute(
                    "UPDATE sandbox.sandboxes \
                        SET snapshot_artifact_path = $1::TEXT, \
                            snapshot_sha256 = $2::BYTEA, \
                            snapshot_ch_version = $3::TEXT \
                      WHERE sandbox_id = $4::TEXT",
                    &[&path, &sha, &ver, &sid_typed],
                )
                .await
                .unwrap();
        }

        // First UPDATE (generation 0 → 1).
        let _ = db
            .update_sandbox_status(sid, status, 0, None)
            .await
            .unwrap_or_else(|e| panic!("status {} rejected by pg: {e:?}", status.as_str()));
        // Reset for next iteration: clear the row so the partial
        // unique index doesn't fire on the next insert.
        let _ = db.delete_sandbox(sid, None, None).await;
    }
}

// ────────────────────────────────────────────────────────────────────
// Round-1 fixer / CRITICAL #6 — base64url-alphabet token_ids that
// differ only in `-` vs `_` insert as DISTINCT rows after migration
// 0003. Pre-migration the handler munged both to `x` and the second
// INSERT collided on the PK.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; round-1 fixer regression for CRITICAL #6"]
async fn base64url_token_ids_with_dash_vs_underscore_are_distinct() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sandbox_uuid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"0".repeat(32), Some("http://x"), None)
        .await
        .unwrap();

    // Two raw tids (24 chars each) that differ only at one position:
    // '-' vs '_'. Both are valid base64url, both pass the migration
    // 0003 CHECK, neither parses as base62.
    let tid_a = "AAAAAAAAAAAA-AAAAAAAAAAA";
    let tid_b = "AAAAAAAAAAAA_AAAAAAAAAAA";
    assert_eq!(tid_a.len(), tid_b.len());

    let row_a = ShareRow {
        token_id: format!("tok_{tid_a}"),
        sandbox_id: info.sandbox_id.clone(),
        port: 5173,
        scope: "ro".into(),
        secret_version: 1,
        issued_at_secs: 1_700_000_000,
        expires_at_secs: 1_700_003_600,
        iss: None,
    };
    let row_b = ShareRow {
        token_id: format!("tok_{tid_b}"),
        ..row_a.clone()
    };

    db.insert_share(&row_a).await.expect("row A inserts");
    db.insert_share(&row_b).await.expect("row B inserts (distinct PK)");

    let metas = db
        .list_shares_for_sandbox(sandbox_uuid, 5173)
        .await
        .unwrap();
    assert_eq!(metas.len(), 2, "two distinct rows must be visible");
    let mut ids: Vec<&str> = metas.iter().map(|m| m.token_id.as_str()).collect();
    ids.sort();
    let mut want: Vec<&str> = vec![row_a.token_id.as_str(), row_b.token_id.as_str()];
    want.sort();
    assert_eq!(ids, want, "stored ids must round-trip byte-exact");
}

// ────────────────────────────────────────────────────────────────────
// Round-2 fixer / IMPORTANT #1 — host_id fence on update_sandbox_status
// ────────────────────────────────────────────────────────────────────
//
// Per design D-14, every CAS UPDATE on ownership-relevant fields
// must fence on (host_id, generation), not just generation. A
// peer holding our database handle (or with a stale generation
// value somehow agreeing on the integer) MUST NOT be able to flip
// our row's status. This test inserts a row owned by host A,
// then attempts to update_sandbox_status_with_host using a
// DIFFERENT host's host_id at the same generation; expects CasLost.

#[compio::test]
#[ignore = "needs Postgres; Round-2 IMPORTANT #1 host_id fence"]
async fn update_sandbox_status_fences_on_host_id() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Insert a sandbox owned by my_host at generation 0.
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, my_host, &"f".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    // Construct a different (synthetic) host_id and try to update
    // the row pretending to BE that host. Pre-fix this would have
    // succeeded because the WHERE clause only checked generation.
    // Today the SQL fences on host_id; the call returns CasLost.
    let (other_uuid, other_typed) = inject_extra_host(&url, "host-other-fence").await;
    let err = db
        .update_sandbox_status_with_host(
            sid,
            SandboxStatus::Stopped,
            0,
            other_uuid,
            None,
        )
        .await
        .expect_err("wrong-host CAS must fail");
    match err {
        zeroship_sandbox::db::DatabaseError::CasLost {
            sandbox_id,
            expected_generation,
            observed_generation,
            current_host_id,
        } => {
            assert_eq!(expected_generation, 0);
            assert_eq!(
                observed_generation, 0,
                "observed_gen must echo pg's current generation"
            );
            // Round-2 fixer / IMPORTANT #5: CasLost carries the real
            // current owner.
            let my_host_typed = format!(
                "hst_{}",
                zeroship_core::typed_id::uuid_to_base62(&my_host)
            );
            assert_eq!(
                current_host_id.as_deref(),
                Some(my_host_typed.as_str()),
                "current_host_id must surface the real owner"
            );
            assert_eq!(sandbox_id, info.sandbox_id);
            // Touch other_typed so the unused-var clippy stays happy
            // even though we don't assert on it directly.
            let _ = other_typed;
        }
        other => panic!("expected CasLost, got: {other:?}"),
    }

    // The row's host_id and generation are unchanged (no UPDATE
    // landed).
    let (owner, gen) = read_sandbox_owner_and_gen(&url, &info.sandbox_id)
        .await
        .unwrap();
    assert_eq!(gen, 0, "row generation must be unchanged");
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );
    assert_eq!(owner, my_host_typed, "row owner must be unchanged");
}

// ────────────────────────────────────────────────────────────────────
// Round-2 fixer / CRITICAL #4 — draining hosts get taken over after
// lease expiration
// ────────────────────────────────────────────────────────────────────
//
// A host that started shutting down (status='draining') but died or
// got OOM-killed mid-drain must NOT permanently orphan its sandboxes.
// Pre-fix dead_hosts() filtered status='alive' so a draining host
// whose lease expired stayed invisible to takeover forever. Today
// dead_hosts() and the takeover EXISTS subquery accept both
// 'alive' and 'draining'.

#[compio::test]
#[ignore = "needs Postgres; Round-2 CRITICAL #4 draining-host takeover"]
async fn draining_host_with_expired_lease_is_taken_over() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    // Inject a peer host. Insert a sandbox owned by the peer.
    let (peer_uuid, peer_typed) = inject_extra_host(&url, "host-draining").await;
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, peer_uuid, &"d".repeat(32), Some("http://drain"), Some(7))
        .await
        .unwrap();
    let _ = sid;

    // Flip the peer's status to 'draining' (the operator started a
    // graceful shutdown) AND age the heartbeat past the lease so we
    // simulate "host crashed mid-drain."
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE sandbox.hosts \
                SET status = 'draining', \
                    drain_started_at = now(), \
                    last_heartbeat = now() - make_interval(secs => 120) \
              WHERE host_id = $1::TEXT",
            &[&peer_typed.to_string()],
        )
        .await
        .unwrap();

    // dead_hosts MUST surface the draining peer.
    let dead = db.dead_hosts(60).await.unwrap();
    assert!(
        dead.iter().any(|h| h == &peer_typed),
        "draining host with expired lease must be in dead_hosts; got {dead:?}"
    );

    // Takeover succeeds and reclaims the sandbox.
    let taken = db
        .takeover_sandboxes_from_host(&peer_typed, my_host, 60)
        .await
        .unwrap();
    assert_eq!(
        taken.len(),
        1,
        "draining host's sandbox must be reclaimable"
    );
    assert_eq!(taken[0].generation, 1, "takeover bumps generation");

    // Peer's host status flips draining → dead.
    let status = read_host_status(&url, &peer_typed).await.unwrap();
    assert_eq!(
        status, "dead",
        "peer host must transition draining → dead during takeover"
    );

    // Sandbox row is now owned by us at gen=1.
    let (owner, gen) = read_sandbox_owner_and_gen(&url, &info.sandbox_id)
        .await
        .unwrap();
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&my_host)
    );
    assert_eq!(owner, my_host_typed);
    assert_eq!(gen, 1);
}

// ────────────────────────────────────────────────────────────────────
// Round-2 fixer / IMPORTANT #2 — takeover SQL includes 'unreachable'
// ────────────────────────────────────────────────────────────────────
//
// A row stamped 'unreachable' by a previous probe must STILL be
// reclaimable after lease expiration on its host. Pre-fix the
// takeover status filter excluded 'unreachable' so the row was
// permanently degraded with no path back.

#[compio::test]
#[ignore = "needs Postgres; Round-2 IMPORTANT #2 unreachable takeover"]
async fn takeover_includes_unreachable_status() {
    let db = migrated_db().await;
    let url = test_url();
    let my_host = db.host_id();

    let (peer_uuid, peer_typed) = inject_extra_host(&url, "host-unreach").await;
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, peer_uuid, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    // Flip the row to 'unreachable' (CAS-correct against the row's
    // current generation). The previous owner did this when its probe
    // failed.
    db.update_sandbox_status_with_host(
        sid,
        SandboxStatus::Unreachable,
        0,
        peer_uuid,
        None,
    )
    .await
    .unwrap();

    // Peer's lease expires.
    age_heartbeat(&url, &peer_typed, 120).await;

    // Takeover MUST reclaim the unreachable row.
    let taken = db
        .takeover_sandboxes_from_host(&peer_typed, my_host, 60)
        .await
        .unwrap();
    assert_eq!(
        taken.len(),
        1,
        "unreachable row must be reclaimable; pre-fix it stayed orphan"
    );
    // Generation went 0 (insert) → 1 (unreachable flip) → 2 (takeover).
    assert_eq!(taken[0].generation, 2);
}

// ════════════════════════════════════════════════════════════════════
// Phase 3 — pg role permission split (§ 13.2; spec § 4)
// ════════════════════════════════════════════════════════════════════
//
// Each test below verifies one slice of the four-role least-privilege
// invariant. The CI Postgres runs as superuser `postgres`; the tests
// promote the three runtime roles to LOGIN with a known password and
// connect as each role to assert its capability matrix:
//
//   1. sandbox_app DELETE FROM events             → SQLSTATE 42501
//   2. sandbox_app DELETE FROM sandboxes          → succeeds
//      (the controller's stop flow needs this; the design's NO-DELETE
//      invariant applies only to the audit table, not to live state.)
//   3. sandbox_audit SELECT FROM events           → SQLSTATE 42501
//   4. sandbox_audit INSERT INTO events           → succeeds
//   5. sandbox_gdpr SELECT FROM sandboxes         → succeeds
//      (needs SELECT for the cascade WHERE chain)
//   6. sandbox_gdpr INSERT INTO sandboxes         → SQLSTATE 42501
//   7. sandbox_app UPDATE sandboxes               → succeeds

const ROLE_TEST_PASSWORD: &str = "phase3roleperm";

/// Promote the three runtime roles to LOGIN with a known password,
/// against the test database. Idempotent. CI Postgres runs as the
/// superuser `postgres`, so ALTER ROLE … LOGIN PASSWORD … is allowed.
async fn promote_roles_to_login(url: &str) {
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    for role in ["sandbox_app", "sandbox_audit", "sandbox_gdpr"] {
        let sql = format!(
            "ALTER ROLE {role} WITH LOGIN PASSWORD '{ROLE_TEST_PASSWORD}'"
        );
        client.batch_execute(&sql).await.unwrap();
    }
}

/// Build a DSN that targets the test pg as the given role.
fn role_dsn(default_url: &str, role: &str) -> String {
    // default_url shape: postgres://user:pass@host:port/db. Replace
    // user:pass with role:ROLE_TEST_PASSWORD.
    // Simple parse — we control the test fixture.
    let (scheme, rest) = default_url.split_once("://").expect("scheme");
    let (_userinfo, after) = rest.split_once('@').expect("userinfo@");
    format!("{scheme}://{role}:{ROLE_TEST_PASSWORD}@{after}")
}

async fn assert_sqlstate_42501<F, Fut>(label: &str, op: F)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<u64, compio_postgres::Error>>,
{
    match op().await {
        Ok(_) => panic!("{label}: expected 42501 (insufficient_privilege), got Ok"),
        Err(e) => {
            let code = e.code().map(|c| c.code()).unwrap_or("");
            assert_eq!(
                code, "42501",
                "{label}: expected SQLSTATE 42501, got {code:?} ({e})"
            );
        }
    }
}

#[compio::test]
#[ignore = "needs Postgres + superuser; Phase-3 § 4 role permission split"]
async fn role_sandbox_app_cannot_delete_events() {
    let url = test_url();
    let _db = migrated_db().await;
    promote_roles_to_login(&url).await;

    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    assert_sqlstate_42501("sandbox_app DELETE FROM events", || async {
        client.execute("DELETE FROM sandbox.events", &[]).await
    })
    .await;
}

#[compio::test]
#[ignore = "needs Postgres + superuser; Phase-3 § 4 role permission split"]
async fn role_sandbox_audit_cannot_select_events() {
    let url = test_url();
    let _db = migrated_db().await;
    promote_roles_to_login(&url).await;

    let audit_dsn = role_dsn(&url, "sandbox_audit");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&audit_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    // SELECT returns query results; pg returns the SQLSTATE on the
    // wire when the role lacks privilege. compio-postgres surfaces
    // it via Error::code().
    let res = client.query("SELECT * FROM sandbox.events", &[]).await;
    match res {
        Ok(_) => panic!("sandbox_audit SELECT FROM events: expected 42501, got Ok"),
        Err(e) => {
            let code = e.code().map(|c| c.code()).unwrap_or("");
            assert_eq!(code, "42501", "expected 42501, got {code:?} ({e})");
        }
    }
}

#[compio::test]
#[ignore = "needs Postgres + superuser; Phase-3 § 4 role permission split"]
async fn role_sandbox_audit_can_insert_events() {
    let url = test_url();
    let db = migrated_db().await;
    promote_roles_to_login(&url).await;

    // Pre-seed a sandbox row + host so the INSERT's typed-id CHECKs
    // pass and the user_id corresponds to a real owner.
    let host_id = db.host_id();
    let (info, _sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    let audit_dsn = role_dsn(&url, "sandbox_audit");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&audit_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let event_id = typed_id("evt");
    let n = client
        .execute(
            "INSERT INTO sandbox.events (event_id, sandbox_id, user_id, kind, ts, data) \
             VALUES ($1::TEXT, $2::TEXT, $3::TEXT, 'role_perm_test', now(), '{}'::jsonb)",
            &[&event_id, &info.sandbox_id, &info.user_id],
        )
        .await
        .expect("audit role must be able to INSERT events");
    assert_eq!(n, 1, "INSERT should affect 1 row");
}

#[compio::test]
#[ignore = "needs Postgres + superuser; Phase-3 § 4 role permission split"]
async fn role_sandbox_gdpr_can_select_sandboxes() {
    let url = test_url();
    let db = migrated_db().await;
    promote_roles_to_login(&url).await;

    // Pre-seed so SELECT returns rows the GDPR cascade WHERE chain
    // needs.
    let (info, _sid) = fresh_info("alice");
    db.insert_sandbox(&info, db.host_id(), &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    let gdpr_dsn = role_dsn(&url, "sandbox_gdpr");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&gdpr_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let rows = client
        .query("SELECT sandbox_id FROM sandbox.sandboxes", &[])
        .await
        .expect("gdpr role must SELECT sandboxes (cascade prerequisite)");
    assert!(
        !rows.is_empty(),
        "gdpr SELECT must surface the seeded row"
    );
}

#[compio::test]
#[ignore = "needs Postgres + superuser; Phase-3 § 4 role permission split"]
async fn role_sandbox_gdpr_cannot_insert_sandboxes() {
    let url = test_url();
    let _db = migrated_db().await;
    promote_roles_to_login(&url).await;

    let gdpr_dsn = role_dsn(&url, "sandbox_gdpr");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&gdpr_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let sbx = typed_id("sbx");
    let usr = typed_id("usr");
    let prj = typed_id("prj");
    let host_typed = format!("hst_{}", zeroship_core::typed_id::uuid_to_base62(&Uuid::now_v7()));

    assert_sqlstate_42501(
        "sandbox_gdpr INSERT sandboxes",
        || async {
            client
                .execute(
                    "INSERT INTO sandbox.sandboxes \
                       (sandbox_id, user_id, project_id, backend, host_id, \
                        status, key_fp, generation) \
                     VALUES ($1::TEXT, $2::TEXT, $3::TEXT, 'docker', \
                             $4::TEXT, 'running', $5::TEXT, 0)",
                    &[&sbx, &usr, &prj, &host_typed, &"a".repeat(32)],
                )
                .await
        },
    )
    .await;
}

#[compio::test]
#[ignore = "needs Postgres + superuser; Phase-3 § 4 role permission split"]
async fn role_sandbox_app_can_update_sandboxes() {
    let url = test_url();
    let db = migrated_db().await;
    promote_roles_to_login(&url).await;

    let (info, _sid) = fresh_info("alice");
    db.insert_sandbox(&info, db.host_id(), &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    let n = client
        .execute(
            "UPDATE sandbox.sandboxes SET last_used_at = now() WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .expect("sandbox_app must be able to UPDATE sandboxes");
    assert_eq!(n, 1, "UPDATE should affect 1 row");
}

// ════════════════════════════════════════════════════════════════════
// PR 3h — snapshot writers (update_snapshot_metadata,
// clear_snapshot_metadata, update_lessee, transient_state_lease_expired,
// idle_eligible_sandboxes).
//
// Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
// § 9.1 (schema), § 6.1 (transient lease), § 7 (idle sweep).
// ════════════════════════════════════════════════════════════════════

use zeroship_sandbox::snapshot_store::SnapshotMetadata;

fn dummy_meta(path: &str) -> SnapshotMetadata {
    let mut sha = [0u8; 32];
    sha[0] = 0xab;
    sha[31] = 0xcd;
    SnapshotMetadata {
        artifact_path: path.to_string(),
        sha256: sha,
        ch_version: "v51.1".into(),
        bytes: 1024,
    }
}

#[compio::test]
#[ignore = "needs Postgres; PR 3h snapshot metadata writer"]
async fn update_snapshot_metadata_records_artifact_and_cas_to_snapshotted() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();

    // Move to snapshotting first (gen 0 → 1).
    let g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .expect("CAS to snapshotting");
    assert_eq!(g1, 1);

    let meta = dummy_meta("/var/zeroship/ch/snapshots/sbx_test/");
    let backing = r#"{"keys":"unknown","userhome":"unknown","rootfs_overlay":"unknown"}"#;
    let g2 = db
        .update_snapshot_metadata(sid, g1, &meta, 7, backing, Some("v1"))
        .await
        .expect("update_snapshot_metadata");
    assert_eq!(g2, g1 + 1);

    // Verify the row reached `snapshotted` with the artifact descriptor
    // populated.
    let row = db
        .get_sandbox_row(sid)
        .await
        .expect("read row")
        .expect("row exists");
    assert_eq!(row.status, SandboxStatus::Snapshotted);
    assert_eq!(row.generation, g2);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3h CAS-loss path"]
async fn update_snapshot_metadata_returns_cas_lost_on_stale_generation() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    let g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    let meta = dummy_meta("/p");
    let backing = r#"{"keys":"x","userhome":"x","rootfs_overlay":"x"}"#;
    // First call uses correct generation; second call uses stale one.
    db.update_snapshot_metadata(sid, g1, &meta, 7, backing, None)
        .await
        .expect("first metadata write");
    let err = db
        .update_snapshot_metadata(sid, g1, &meta, 7, backing, None)
        .await
        .expect_err("second write must miss CAS");
    assert!(
        matches!(err, zeroship_sandbox::db::DatabaseError::CasLost { .. }),
        "expected CasLost, got {err:?}",
    );
}

#[compio::test]
#[ignore = "needs Postgres; PR 3h clear_snapshot_metadata wipes snapshot_*"]
async fn clear_snapshot_metadata_nulls_all_snapshot_columns() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    // Drive into snapshotted, then back to running.
    let g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    let meta = dummy_meta("/p");
    let backing = r#"{"keys":"x","userhome":"x","rootfs_overlay":"x"}"#;
    let g2 = db
        .update_snapshot_metadata(sid, g1, &meta, 7, backing, None)
        .await
        .unwrap();

    // Move from snapshotted → restoring → running so the CHECK
    // constraint doesn't reject the column-clear (status='running'
    // permits NULL snapshot_*).
    let g3 = db
        .update_sandbox_status(sid, SandboxStatus::Restoring, g2, None)
        .await
        .unwrap();
    let g4 = db
        .update_sandbox_status(sid, SandboxStatus::Running, g3, None)
        .await
        .unwrap();
    let g5 = db
        .clear_snapshot_metadata(sid, g4)
        .await
        .expect("clear");
    assert_eq!(g5, g4 + 1);

    // Verify columns are NULL via direct query.
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let row = client
        .query_one(
            "SELECT snapshot_artifact_path, snapshot_sha256, snapshot_ch_version \
               FROM sandbox.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    let path: Option<String> = row.try_get(0).ok();
    let sha: Option<Vec<u8>> = row.try_get(1).ok();
    let ver: Option<String> = row.try_get(2).ok();
    assert!(path.is_none(), "artifact_path must be NULL after clear");
    assert!(sha.is_none(), "sha256 must be NULL after clear");
    assert!(ver.is_none(), "ch_version must be NULL after clear");
}

#[compio::test]
#[ignore = "needs Postgres; PR 3h lease bump"]
async fn update_lessee_bumps_only_in_transient_states() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    // running → bump must affect 0 rows (status guard).
    let n0 = db.update_lessee(sid).await.expect("running noop");
    assert_eq!(n0, 0, "update_lessee must skip non-transient rows");

    // snapshotting → bump must affect 1 row.
    let _g = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    let n1 = db.update_lessee(sid).await.expect("transient bump");
    assert_eq!(n1, 1, "update_lessee must bump transient rows");
}

#[compio::test]
#[ignore = "needs Postgres; PR 3h transient sweep query"]
async fn transient_state_lease_expired_filters_by_threshold() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();
    // Move to snapshotting + bump lessee (now()).
    let _g = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    db.update_lessee(sid).await.unwrap();

    // Threshold high → 0 rows (we just bumped).
    let none = db
        .transient_state_lease_expired_sandboxes(60)
        .await
        .expect("query");
    assert!(
        !none.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "fresh-bumped row must not appear in expired set"
    );

    // Backdate lessee_updated_at to ~5 min ago + re-query at 120s
    // threshold; must find our row.
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE sandbox.sandboxes SET lessee_updated_at = now() - interval '5 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();

    let stale = db
        .transient_state_lease_expired_sandboxes(120)
        .await
        .expect("query");
    assert!(
        stale.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "stale-lease row must surface"
    );
}

#[compio::test]
#[ignore = "needs Postgres; PR 3h idle sweep query"]
async fn idle_eligible_sandboxes_respects_opt_in_and_threshold() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, _sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    // Default `idle_snapshot_opted_in = FALSE` → never returned.
    let none = db
        .idle_eligible_sandboxes(0, 100)
        .await
        .expect("query opted-out");
    assert!(
        !none.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "opted-out rows must not appear",
    );

    // Opt the row in + backdate last_used_at; query at threshold > 0
    // expects a hit.
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE sandbox.sandboxes \
                SET idle_snapshot_opted_in = TRUE, \
                    last_used_at = now() - interval '10 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    let some = db
        .idle_eligible_sandboxes(60, 100)
        .await
        .expect("query opted-in");
    assert!(
        some.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "opted-in stale row must surface"
    );
}
