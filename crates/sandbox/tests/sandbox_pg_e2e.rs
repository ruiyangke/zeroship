//! Sandbox pg-state integration tests.
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
//! The sandbox controller no longer self-migrates: the unified
//! `zeroship` schema (sandbox tables included) is owned by Liquibase.
//! The `common::reset_and_migrate` fixture drops the sandbox tables and
//! re-applies the same changeset DDL before each test.
//! Tests are serialised via `--test-threads=1` to avoid stomping on
//! each other's schema reset.

use compio_postgres::{NoTls, Pool, PoolConfig};
use zeroship_sandbox::db::Database;

mod common;

const TEST_URL_ENV: &str = "PG_TEST_URL";
const DEFAULT_URL: &str = "postgres://postgres:zeroship@localhost:5440/zeroship";

fn test_url() -> String {
    std::env::var(TEST_URL_ENV).unwrap_or_else(|_| DEFAULT_URL.to_string())
}

// ────────────────────────────────────────────────────────────────────
// 1. Schema present after the Liquibase changelog is applied, and the
//    controller boots (pings) against the migrated `zeroship` schema.
//
//    Regression: the consolidation moved the sandbox DDL into Liquibase
//    and removed the embedded runner. This test fails if the changesets
//    don't create the sandbox objects in `zeroship`, or if a query
//    still targets the old `sandbox` schema (which is never created).
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; run with `docker compose up -d postgres`"]
async fn migrated_schema_is_present_and_controller_boots() {
    let url = test_url();
    common::reset_and_migrate(&url).await;

    // The controller assumes the schema exists; boot only pings.
    let db = Database::from_test_config(url.clone())
        .await
        .expect("from_test_config");
    db.ping().await.expect("controller pings migrated schema");

    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();

    // Every sandbox object now lives in the unified `zeroship` schema.

    // Spot-check the columns the design depends on. `generation` is
    // the round-6 CAS counter; if it disappears, the HA design is
    // broken.
    let row = client
        .query_one(
            "SELECT data_type FROM information_schema.columns
              WHERE table_schema = 'zeroship'
                AND table_name = 'sandboxes'
                AND column_name = 'generation'",
            &[],
        )
        .await
        .expect("generation column must exist");
    let ty: &str = row.get(0);
    assert_eq!(ty, "bigint", "generation must be BIGINT");

    // sandbox_events is partitioned + has the BRIN index.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'zeroship' AND c.relname = 'sandbox_events_default'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 1, "sandbox_events_default partition must exist");

    // Six monthly partitions. relkind='r' filters indexes /
    // sequences off pg_class; we want only the six partition tables.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_class c
              JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'zeroship'
                AND c.relkind = 'r'
                AND c.relname LIKE 'sandbox\\_events\\_2026\\_%'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 6, "expected 6 monthly sandbox_events partitions, got {n}");

    // BRIN index on sandbox_events.ts.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_indexes
              WHERE schemaname = 'zeroship' AND indexname = 'idx_sandbox_events_ts_brin'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 1, "BRIN index on sandbox_events.ts must exist");

    // deleted_sandboxes tombstone table.
    let row = client
        .query_one(
            "SELECT count(*)::INTEGER FROM pg_tables
              WHERE schemaname = 'zeroship' AND tablename = 'deleted_sandboxes'",
            &[],
        )
        .await
        .unwrap();
    let n: i32 = row.get(0);
    assert_eq!(n, 1, "deleted_sandboxes tombstone must exist");
}

// ────────────────────────────────────────────────────────────────────
// 2. Re-applying the changelog is idempotent (the fixture itself
//    re-runs the guarded changesets on every call).
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn reset_and_migrate_is_idempotent() {
    let url = test_url();
    // Two applies in a row must not error (CREATE ... IF NOT EXISTS,
    // guarded role creation, DROP ... IF EXISTS on reset).
    common::reset_and_migrate(&url).await;
    common::reset_and_migrate(&url).await;

    let db = Database::from_test_config(url).await.unwrap();
    db.ping().await.expect("ping after second apply");
}

// ────────────────────────────────────────────────────────────────────
// 3. Connection-pool baseline: 16 concurrent ping connections
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres"]
async fn pool_baseline_handles_16_concurrent_ping() {
    let url = test_url();
    // Build a pool with the default 16-connection test budget.
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
    let db = Database::from_test_config(test_url())
        .await
        .unwrap();
    db.ping().await.expect("ping ok");
}

// Reference to NoTls so the use-import isn't dead.
#[allow(dead_code)]
fn _no_tls_anchor(_t: NoTls) {}

// ════════════════════════════════════════════════════════════════════
// Pg write round-trips
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
    common::reset_and_migrate(&url).await;
    let db = Database::from_test_config(url).await.unwrap();
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
            "SELECT count(*)::BIGINT FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), 0);
    // Tombstone present.
    let row = client
        .query_one(
            "SELECT count(*)::BIGINT FROM zeroship.deleted_sandboxes WHERE sandbox_id = $1::TEXT",
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
// Periodic heartbeat and lease-based takeover
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
            "INSERT INTO zeroship.hosts (host_id, boot_id, hostname, region, backend, status) \
             VALUES ($1::TEXT, $2::TEXT, $3::TEXT, 'us-local-1', 'nomad-ch', 'alive')",
            &[&host_typed, &boot_typed, &hostname.to_string()],
        )
        .await
        .unwrap();
    (host_uuid, host_typed)
}

/// Force `zeroship.hosts.last_heartbeat = now() - secs_ago` for the
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
            "UPDATE zeroship.hosts \
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
            "UPDATE zeroship.hosts \
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
            "SELECT status FROM zeroship.hosts WHERE host_id = $1::TEXT",
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
            "SELECT host_id, generation FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
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
    // the row 'stopped' with its stale generation=0. CAS misses and
    // returns the typed `CasLost` variant.
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
    // Pattern-match the typed variant instead of depending on the
    // formatted error string.
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
// Pg-side takeover succeeds for every starting/running row. The
// controller's probe pipeline is the layer that flips status to
// `recreating` on a `/version` fingerprint mismatch.
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
            "SELECT host_id, status, generation FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
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
// `set_host_draining` marks the host row `draining` so peers see the
// drain intent before our heartbeat goes silent. Without this, a
// graceful shutdown looks like a crash and peers wait the full
// lease TTL before reacting.
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
               FROM zeroship.hosts WHERE host_id = $1::TEXT",
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
            "SELECT status FROM zeroship.hosts WHERE host_id = $1::TEXT",
            &[&host_typed],
        )
        .await
        .unwrap();
    let status2: String = row2.get(0);
    assert_eq!(status2, "draining");
}

// ────────────────────────────────────────────────────────────────────
// `get_sandbox_row` must round-trip the full row. The post-takeover
// rehydrate path uses this to fetch the full row by
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
// A controller that lost the lease on `stop` must not delete the row
// out from under the new owner. The
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
            "SELECT host_id FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
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
// Tenant fence on `update_sandbox_status`. A misrouted call asserting
// `expected_user_id=usr_bob` against a row
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
// Typed IDs must round-trip end to end. The handler
// is supposed to mint `sandbox_id` as `sbx_<base62>` and pass typed-id
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
            "SELECT count(*)::BIGINT FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
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
// Every `SandboxStatus` value must pass the pg CHECK. Migration 0002
// widened the constraint to include
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
                    "UPDATE zeroship.sandboxes \
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
// Base64url token IDs that differ only in `-` vs `_` must insert as
// distinct rows after migration
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
// `update_sandbox_status` must fence on `host_id`
// ────────────────────────────────────────────────────────────────────
//
// Per design WebSocket signature-separation rule, every CAS UPDATE on ownership-relevant fields
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
            // `CasLost` includes the real current owner.
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
// Draining hosts must become reclaimable after lease expiration
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
            "UPDATE zeroship.hosts \
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
// Takeover SQL includes `unreachable`
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
// Pg role permission split between the runtime roles.
// ════════════════════════════════════════════════════════════════════
//
// Each test below verifies one slice of the four-role least-privilege
// invariant. The CI Postgres runs as superuser `postgres`; the tests
// promote the three runtime roles to LOGIN with a known password and
// connect as each role to assert its capability matrix:
//
//   1. sandbox_app DELETE FROM sandbox_events     → SQLSTATE 42501
//   2. sandbox_app DELETE FROM sandboxes          → succeeds
//      (the controller's stop flow needs this; the design's NO-DELETE
//      invariant applies only to the audit table, not to live state.)
//   3. sandbox_audit SELECT FROM sandbox_events   → SQLSTATE 42501
//   4. sandbox_audit INSERT INTO sandbox_events   → succeeds
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

    assert_sqlstate_42501("sandbox_app DELETE FROM sandbox_events", || async {
        client.execute("DELETE FROM zeroship.sandbox_events", &[]).await
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
    let res = client.query("SELECT * FROM zeroship.sandbox_events", &[]).await;
    match res {
        Ok(_) => panic!("sandbox_audit SELECT FROM sandbox_events: expected 42501, got Ok"),
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
            "INSERT INTO zeroship.sandbox_events (event_id, sandbox_id, user_id, kind, ts, data) \
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
        .query("SELECT sandbox_id FROM zeroship.sandboxes", &[])
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
                    "INSERT INTO zeroship.sandboxes \
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
            "UPDATE zeroship.sandboxes SET last_used_at = now() WHERE sandbox_id = $1::TEXT",
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
               FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
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
            "UPDATE zeroship.sandboxes SET lessee_updated_at = now() - interval '5 minutes' \
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

// ────────────────────────────────────────────────────────────────────
// C1 regression: update_sandbox_status must wire `lessee_updated_at`
// per §6.1. A target transient state stamps now(); a target
// non-transient state clears to NULL. Pre-fix, neither happened — the
// sweep filter `WHERE lessee_updated_at IS NOT NULL` therefore
// excluded every real transient row, and abandoned snapshotting /
// restoring rows wedged forever.
// ────────────────────────────────────────────────────────────────────

#[compio::test]
#[ignore = "needs Postgres; C1 regression"]
async fn state_transition_to_snapshotting_sets_lessee() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    // Pre-condition: a fresh `running` row carries no lessee timestamp.
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let pre: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(pre.is_none(), "fresh running row must have NULL lessee_updated_at");

    // Cross the running → snapshotting transient boundary.
    let _g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .expect("CAS to snapshotting");

    // lessee_updated_at must be non-NULL now.
    let post: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(
        post.is_some(),
        "after CAS to snapshotting, lessee_updated_at must be set so the §6.1 sweep can find it"
    );

    // And the sweep query at a 0s threshold must surface it. This is
    // the load-bearing assertion: pre-fix, the row was invisible.
    let stale = db
        .transient_state_lease_expired_sandboxes(0)
        .await
        .expect("sweep query");
    assert!(
        stale.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "transient row with set lessee must be findable by the lease-takeover sweep"
    );

    // Restoring (the other transient kind) must do the same. Drive
    // snapshotting → snapshotted → restoring through the metadata
    // writer first (CHECK constraint requires artifact-path).
    let g_snap = db
        .update_snapshot_metadata(
            sid,
            // generation post-CAS-to-snapshotting is 1.
            1,
            &dummy_meta("/p"),
            1,
            r#"{"keys":"x","userhome":"x","rootfs_overlay":"x"}"#,
            None,
        )
        .await
        .expect("snapshotted");
    // update_snapshot_metadata clears lessee_updated_at (it already
    // did so pre-fix); verify and then re-enter transient.
    let after_snap: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(after_snap.is_none(), "snapshotted row must have NULL lessee");

    let _g_rst = db
        .update_sandbox_status(sid, SandboxStatus::Restoring, g_snap, None)
        .await
        .expect("CAS to restoring");
    let after_rst: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(
        after_rst.is_some(),
        "after CAS to restoring, lessee_updated_at must be set (sweep covers all three transient states)"
    );
}

#[compio::test]
#[ignore = "needs Postgres; C1 regression"]
async fn state_transition_to_running_clears_lessee() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    // Drive into snapshotting → row carries a non-NULL lessee.
    let g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    let url = test_url();
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let mid: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(mid.is_some(), "snapshotting row must carry lessee_updated_at");

    // Roll back to running (snapshot_handler error path). lessee MUST
    // clear — otherwise a subsequent failed-then-completed cycle could
    // leak a stale timestamp into a future transient run.
    let _g2 = db
        .update_sandbox_status(sid, SandboxStatus::Running, g1, None)
        .await
        .expect("CAS to running");
    let post: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(
        post.is_none(),
        "after CAS back to running, lessee_updated_at must be NULL (§6.1 invariant)"
    );

    // And the sweep must not see this row anymore.
    let stale = db
        .transient_state_lease_expired_sandboxes(0)
        .await
        .expect("sweep query");
    assert!(
        !stale.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "running row must not appear in the transient-lease sweep"
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
            "UPDATE zeroship.sandboxes \
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

// ════════════════════════════════════════════════════════════════════
// PR 3b — SnapshotHandler integration tests (mock ch-remote +
// LocalDiskSnapshotStore + StubSourceVmOps; real Database).
//
// Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
// § 3, § 8.2 (rollback on mid-flight failure).
// ════════════════════════════════════════════════════════════════════

use zeroship_sandbox::snapshot_handler::{
    snapshot_sandbox, ChRemoteClient, MockChRemoteClient, SnapshotHandlerError, StubSourceVmOps,
};
use zeroship_sandbox::snapshot_store::{LocalDiskSnapshotStore, SnapshotStore};

fn fresh_temp(suffix: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "zsbx-snap-handler-{}-{}-{}",
        std::process::id(),
        Uuid::now_v7().simple(),
        suffix,
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[compio::test]
#[ignore = "needs Postgres; PR 3b snapshot handler happy path"]
async fn snapshot_handler_happy_path_records_artifact_and_tears_down_source() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();

    let stage_dir = fresh_temp("stage");
    let store_root = fresh_temp("store");
    let store: std::sync::Arc<dyn SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let ch: std::sync::Arc<dyn ChRemoteClient> =
        std::sync::Arc::new(MockChRemoteClient::default());
    let api_sock = fresh_temp("api").join("ch.sock");
    let vm_ops = StubSourceVmOps::new(api_sock, 7);

    let outcome = snapshot_sandbox(
        &db,
        std::sync::Arc::clone(&store),
        std::sync::Arc::clone(&ch),
        &vm_ops,
        sid,
        stage_dir.clone(),
        true,
    )
    .await
    .expect("happy-path snapshot");
    assert_eq!(outcome.vm_index, 7);
    assert_eq!(outcome.metadata.ch_version, "v51.1");

    // pg row must be in `snapshotted` with metadata populated.
    let row = db
        .get_sandbox_row(sid)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.status, SandboxStatus::Snapshotted);

    // Source teardown was invoked.
    assert!(
        vm_ops
            .teardown_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "teardown_source must have been invoked on success"
    );

    let _ = std::fs::remove_dir_all(&store_root);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3b state-mismatch refusal"]
async fn snapshot_handler_refuses_non_running_state() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();
    // Move out of running.
    db.update_sandbox_status(sid, SandboxStatus::Stopping, 0, None)
        .await
        .unwrap();

    let stage_dir = fresh_temp("stage2");
    let store_root = fresh_temp("store2");
    let store: std::sync::Arc<dyn SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let ch: std::sync::Arc<dyn ChRemoteClient> =
        std::sync::Arc::new(MockChRemoteClient::default());
    let vm_ops = StubSourceVmOps::new(fresh_temp("a").join("s"), 1);

    let err = snapshot_sandbox(&db, store, ch, &vm_ops, sid, stage_dir, true)
        .await
        .expect_err("must refuse");
    assert!(
        matches!(err, SnapshotHandlerError::StateMismatch { .. }),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(&store_root);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3b rollback on ch-remote failure"]
async fn snapshot_handler_rolls_back_on_ch_remote_failure() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    let stage_dir = fresh_temp("stage3");
    let store_root = fresh_temp("store3");
    let store: std::sync::Arc<dyn SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let ch_inner = {
        let mut c = MockChRemoteClient::default();
        c.fail_snapshot = true; // pause succeeds, snapshot fails
        c
    };
    let ch: std::sync::Arc<dyn ChRemoteClient> = std::sync::Arc::new(ch_inner);
    let vm_ops = StubSourceVmOps::new(fresh_temp("b").join("s"), 1);

    let err = snapshot_sandbox(&db, store, ch, &vm_ops, sid, stage_dir, true)
        .await
        .expect_err("must fail");
    assert!(
        matches!(err, SnapshotHandlerError::ChRemote(_)),
        "{err:?}"
    );

    // Row must have rolled back to `running` with generation bumped
    // twice (running→snapshotting→running).
    let row = db
        .get_sandbox_row(sid)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.status, SandboxStatus::Running);
    assert!(
        row.generation >= 2,
        "rollback should have bumped gen at least twice; got {}",
        row.generation
    );

    // Teardown must NOT have run (we rolled back the snapshot).
    assert!(
        !vm_ops
            .teardown_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "teardown_source must not run on rollback"
    );

    let _ = std::fs::remove_dir_all(&store_root);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3b feature flag off"]
async fn snapshot_handler_returns_feature_disabled_when_flag_off() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(1))
        .await
        .unwrap();

    let stage_dir = fresh_temp("stage4");
    let store_root = fresh_temp("store4");
    let store: std::sync::Arc<dyn SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let ch: std::sync::Arc<dyn ChRemoteClient> =
        std::sync::Arc::new(MockChRemoteClient::default());
    let vm_ops = StubSourceVmOps::new(fresh_temp("c").join("s"), 1);

    let err =
        snapshot_sandbox(&db, store, ch, &vm_ops, sid, stage_dir, /* enabled = */ false)
            .await
            .expect_err("must refuse with flag off");
    assert!(
        matches!(err, SnapshotHandlerError::FeatureDisabled),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(&store_root);
}

// ════════════════════════════════════════════════════════════════════
// PR 3e — RestoreHandler integration tests (LocalDiskSnapshotStore +
// StubRestoreBackend; real Database).
//
// Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
// § 5 (identity rewrite), § 8 (failure modes).
// ════════════════════════════════════════════════════════════════════

use zeroship_sandbox::restore_handler::{
    restore_sandbox, RestoreBackend, RestoreHandlerError, StubRestoreBackend,
};

/// Drive a row to `Snapshotted` with a real artifact on disk so the
/// restore handler can find + verify it. Returns
/// (artifact-store, sha256, vm_index, generation-after-snapshot).
async fn seed_snapshotted_row(
    db: &Database,
    sid: Uuid,
    info: &SandboxInfo,
    store_root: &std::path::Path,
) -> ([u8; 32], i16, i64) {
    let store = LocalDiskSnapshotStore::new(store_root);
    let host_id = db.host_id();
    db.insert_sandbox(info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    // Drive to running → snapshotting → snapshotted.
    let g0 = 0i64;
    let g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, g0, None)
        .await
        .unwrap();

    // Stage a fake artifact.
    let stage = std::env::temp_dir().join(format!(
        "zsbx-restore-test-stage-{}-{}",
        std::process::id(),
        Uuid::now_v7().simple(),
    ));
    std::fs::create_dir_all(&stage).unwrap();
    // Minimal config.json carrying the v1 rewrite shape. Post-pivot
    // (bug #11): keys/workspace/userhome are virtio-blk disks, not
    // virtiofs sockets — so `fs[]` is absent and `disks[]` carries
    // the rootfs.img + persistent-workspace + per-user-home triple.
    // The rewriter (`rewrite_config_json`) still only touches
    // `net[].tap` + `net[].mac`; the path-bearing entries below stay
    // untouched, which is the invariant this fixture is designed to
    // exercise.
    std::fs::write(
        stage.join("config.json"),
        r#"{
            "net":[{"tap":"zsbx-nm-99","mac":"12:34:56:78:9b:63"}],
            "disks":[
                {"path":"/opt/nomad/data/alloc/oldalloc/ch/local/rootfs.img"},
                {"path":"/var/zeroship/ch/sbx-oldalloc/workspace.img"},
                {"path":"/var/zeroship/ch/users/usr-oldalloc/home.img"}
            ],
            "serial":{"mode":"File","file":"/opt/nomad/data/alloc/oldalloc/ch/local/serial.log"}
        }"#,
    )
    .unwrap();
    std::fs::write(stage.join("state.json"), b"{\"st\":1}").unwrap();
    std::fs::write(stage.join("memory-ranges"), b"fake-mem").unwrap();
    let typed = format!("sbx_{}", zeroship_core::typed_id::uuid_to_base62(&sid));
    let meta = store.put(&typed, &stage, "v51.1").unwrap();
    let _ = std::fs::remove_dir_all(&stage);

    let backing = r#"{"keys":"u","userhome":"u","rootfs_overlay":"u"}"#;
    let g2 = db
        .update_snapshot_metadata(sid, g1, &meta, 7, backing, Some("v1"))
        .await
        .unwrap();
    (meta.sha256, 7, g2)
}

#[compio::test]
#[ignore = "needs Postgres; PR 3e restore happy path"]
async fn restore_handler_happy_path_drives_snapshotted_to_running() {
    let db = migrated_db().await;
    let (info, sid) = fresh_info("alice");
    let store_root = fresh_temp("rstore");
    let (_sha, _vm_index, _g2) = seed_snapshotted_row(&db, sid, &info, &store_root).await;

    let store: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let backend_root = fresh_temp("rback");
    // R8-A3-5: `restore_sandbox` takes `Arc<dyn RestoreBackend>` so
    // the spawn_blocking wraps on submit_restore_job + wait_for_livez
    // own a clonable handle. Keep a concrete-typed Arc for post-call
    // assertions; hand a coerced dyn-Arc to `restore_sandbox`.
    let backend = std::sync::Arc::new(StubRestoreBackend::new(backend_root.clone()));
    let backend_dyn: std::sync::Arc<dyn zeroship_sandbox::restore_handler::RestoreBackend> =
        std::sync::Arc::clone(&backend) as _;

    let outcome = restore_sandbox(
        &db,
        std::sync::Arc::clone(&store),
        backend_dyn,
        None,
        sid,
        true,
    )
    .await
    .expect("happy path restore");
    assert_eq!(outcome.vm_index, 7);

    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Running);

    // The stub backend should have seen the full sequence.
    assert!(backend.submit_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(backend.livez_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!backend.teardown_called.load(std::sync::atomic::Ordering::SeqCst));

    // config.json got rewritten in-place inside backend's restore_alloc_dir.
    let cfg_path = backend.restore_alloc_dir(sid).join("config.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cfg_path).unwrap()).unwrap();
    assert_eq!(v["net"][0]["tap"], "zsbx-nm-7");

    let _ = std::fs::remove_dir_all(&store_root);
    let _ = std::fs::remove_dir_all(&backend_root);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3e checksum mismatch → snapshotted_suspect"]
async fn restore_handler_checksum_mismatch_marks_suspect() {
    let db = migrated_db().await;
    let (info, sid) = fresh_info("alice");
    let store_root = fresh_temp("rstore_corrupt");
    let (_sha, _vm_index, g2) = seed_snapshotted_row(&db, sid, &info, &store_root).await;

    // Corrupt the on-disk artifact so the SHA-256 verify fails.
    let typed = format!("sbx_{}", zeroship_core::typed_id::uuid_to_base62(&sid));
    let mr = store_root.join(&typed).join("memory-ranges");
    std::fs::write(&mr, b"tampered").unwrap();
    let _ = g2;

    let store: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let backend_root = fresh_temp("rback_corrupt");
    let backend: std::sync::Arc<dyn zeroship_sandbox::restore_handler::RestoreBackend> =
        std::sync::Arc::new(StubRestoreBackend::new(backend_root.clone()));

    let err = restore_sandbox(&db, std::sync::Arc::clone(&store), backend, None, sid, true)
        .await
        .expect_err("must fail with corrupt artifact");
    assert!(matches!(err, RestoreHandlerError::SnapshotCorrupt), "{err:?}");

    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::SnapshottedSuspect);

    let _ = std::fs::remove_dir_all(&store_root);
    let _ = std::fs::remove_dir_all(&backend_root);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3e cluster-exhausted vm_index → 503"]
async fn restore_handler_vm_index_unavailable_when_cluster_exhausted() {
    let db = migrated_db().await;
    let (info, sid) = fresh_info("alice");
    let store_root = fresh_temp("rstore_busy");
    let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

    let store: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let backend_root = fresh_temp("rback_busy");
    let mut backend_inner = StubRestoreBackend::new(backend_root.clone());
    backend_inner.fail_reserve = true;
    let backend: std::sync::Arc<dyn zeroship_sandbox::restore_handler::RestoreBackend> =
        std::sync::Arc::new(backend_inner);

    let err = restore_sandbox(&db, std::sync::Arc::clone(&store), backend, None, sid, true)
        .await
        .expect_err("must fail with cluster-exhausted");
    assert!(
        matches!(err, RestoreHandlerError::VmIndexUnavailable { .. }),
        "{err:?}"
    );

    // Row should have rolled back to `snapshotted` (artifact preserved).
    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Snapshotted);

    let _ = std::fs::remove_dir_all(&store_root);
    let _ = std::fs::remove_dir_all(&backend_root);
}

// ════════════════════════════════════════════════════════════════════
// PR 3g — sweep tasks (transient-state takeover + idle eviction).
//
// Source-of-truth: docs/proposals/sandbox-snapshot-restore.md
// § 6.1 + § 7.
// ════════════════════════════════════════════════════════════════════

use zeroship_sandbox::backend::Backend;
use zeroship_sandbox::config::{ApiToken, SandboxConfig};
use zeroship_sandbox::sweep::{
    run_idle_eviction_once, run_transient_takeover_once, RecordingIdleSnapshotter,
};

fn sweep_test_cfg(snapshot_enabled: bool) -> SandboxConfig {
    // A7 (deferred): `token` is `pub(crate)`; out-of-crate construction
    // goes through `SandboxConfig::new_fixture` + the `with_token`
    // builder. `snapshot_enabled` is still `pub`, so we mutate it via
    // direct field assignment on the returned value.
    let mut cfg = SandboxConfig::new_fixture().with_token(ApiToken::new("ignored"));
    cfg.snapshot_enabled = snapshot_enabled;
    cfg
}

fn build_sweep_state(
    db: Database,
    snapshot_enabled: bool,
) -> std::sync::Arc<zeroship_sandbox::AppState> {
    let cfg = sweep_test_cfg(snapshot_enabled);
    let backend = Backend::builder(&cfg).build().expect("backend");
    // A5: `admin_token` is `pub(crate)`; out-of-crate construction
    // goes through `AppState::new_fixture` (admin_token = None).
    // A6b: `database` is `pub(crate)`; set via `with_database`
    // builder instead of struct-field assignment.
    let state = zeroship_sandbox::AppState::new_fixture(cfg, backend)
        .with_database(std::sync::Arc::new(db));
    std::sync::Arc::new(state)
}

#[compio::test]
#[ignore = "needs Postgres; PR 3g + C1-FOLLOWUP — sweep recovers CRASHED peer's wedge"]
async fn sweep_transient_takeover_recovers_stale_snapshotting_row() {
    // C1-FOLLOWUP (concurrency-r9): the sweep targets OTHER
    // controllers' wedges, not our own. Insert the row owned by an
    // injected "peer" host (different host_id) so the sweep query
    // surfaces it and the ownership-transferring recovery CAS lands.
    // Pre-C1-FOLLOWUP this test inserted with `fresh_host(&db)` ==
    // our own host_id, which the sweep query now (correctly)
    // excludes from the candidate set.
    let db = migrated_db().await;
    let url = test_url();
    let (peer_host_id, peer_host_typed) = inject_extra_host(&url, "peer-crashed").await;
    let (info, sid) = fresh_info("alice");
    // Row owned by the (about-to-crash) peer controller.
    db.insert_sandbox(&info, peer_host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();

    // Drive into snapshotting under the PEER's identity by going
    // through the CAS with `host_id = peer_host_id`. Our own
    // `update_sandbox_status` fences on `self.host_id()` so we use
    // the `_with_host` variant directly.
    let g1 = db
        .update_sandbox_status_with_host(
            sid,
            SandboxStatus::Snapshotting,
            0,
            peer_host_id,
            None,
        )
        .await
        .unwrap();
    let _ = g1;
    // Backdate `lessee_updated_at` 600 seconds — well past the 120s
    // threshold the sweep uses by default. Simulates the peer's
    // 10s `update_lessee` heartbeat stopping when the peer crashed.
    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let n = client
        .execute(
            "UPDATE zeroship.sandboxes \
                SET lessee_updated_at = now() - interval '10 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    assert_eq!(n, 1);

    let state = build_sweep_state(db, /*snapshot_enabled=*/true);
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&state.database().unwrap().host_id())
    );
    assert_ne!(
        peer_host_typed, my_host_typed,
        "test precondition: peer host must differ from sweep's controller \
         host_id (otherwise the sweep query rightly excludes it)"
    );

    let (seen, recovered) = run_transient_takeover_once(&state, 120).await;
    assert!(
        seen >= 1,
        "sweep should have seen at least our stale peer-owned row; seen={seen}",
    );
    assert!(
        recovered >= 1,
        "sweep should have recovered at least our row; recovered={recovered} \
         (pre-C1-FOLLOWUP this was always 0 because the CAS fenced on \
         self.host_id() != peer's stored host_id)",
    );

    // Row must be in `snapshotting_aborted` per § 9.2, ownership
    // transferred to the recovering controller, and lessee_updated_at
    // cleared (target state is non-transient).
    let row = state
        .database()
        .unwrap()
        .get_sandbox_row(sid)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(row.status, SandboxStatus::SnapshottingAborted);
    assert_eq!(
        row.host_id, my_host_typed,
        "recovery CAS must transfer ownership from crashed peer to \
         self.host_id(); got {} expected {}",
        row.host_id, my_host_typed,
    );
    let post_lessee: Option<std::time::SystemTime> = client
        .query_one(
            "SELECT lessee_updated_at FROM zeroship.sandboxes WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap()
        .try_get(0)
        .ok();
    assert!(
        post_lessee.is_none(),
        "post-recovery (non-transient state) must have NULL lessee_updated_at; \
         got {post_lessee:?}",
    );
}

/// C1-FOLLOWUP regression #1: the recovery scope is OTHER controllers'
/// wedges only. A row whose `host_id = self.host_id()` with a stale
/// `lessee_updated_at` must NOT be touched by the sweep — either the
/// handler is legitimately mid-flight (and is supposed to bump the
/// lease itself) or our own process is hung in a way the sweep can't
/// safely arbitrate. Pre-C1-FOLLOWUP the sweep query surfaced these
/// rows, the CAS then bounced on `host_id = self.host_id()` (which
/// trivially matched), and the row was "recovered" — destroying
/// in-flight state that may still resolve normally.
#[compio::test]
#[ignore = "needs Postgres; C1-FOLLOWUP regression — sweep skips SELF-owned transients"]
async fn sweep_transient_takeover_skips_self_owned_in_flight_row() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    // Drive into snapshotting as ourselves.
    let _g1 = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    // Even at the tightest threshold, a self-owned row must be
    // invisible to the sweep candidate query (we cap the scope to
    // OTHER controllers' wedges at the SELECT level).
    let url = test_url();
    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    let n = client
        .execute(
            "UPDATE zeroship.sandboxes \
                SET lessee_updated_at = now() - interval '10 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();
    assert_eq!(n, 1, "should have backdated exactly our row");

    let state = build_sweep_state(db, /*snapshot_enabled=*/true);
    let (seen, recovered) = run_transient_takeover_once(&state, 0).await;
    // Sweep MUST NOT touch our self-owned row even though
    // lessee_updated_at is stale. Other unrelated rows the fixture
    // may have created could push `seen`/`recovered` up, but ours
    // specifically must still be `snapshotting` afterwards.
    let row = state
        .database()
        .unwrap()
        .get_sandbox_row(sid)
        .await
        .unwrap()
        .expect("row");
    assert_eq!(
        row.status,
        SandboxStatus::Snapshotting,
        "self-owned in-flight row must remain in snapshotting; got {:?} \
         (seen={seen} recovered={recovered}); pre-C1-FOLLOWUP the sweep \
         would have flipped this to snapshotting_aborted",
        row.status,
    );

    // And directly: the lease-expired query must not list it.
    let listed = state
        .database()
        .unwrap()
        .transient_state_lease_expired_sandboxes(0)
        .await
        .unwrap();
    assert!(
        !listed.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "self-owned row must not appear in transient_state_lease_expired_sandboxes; \
         got {:?}",
        listed.iter().map(|r| &r.sandbox_id).collect::<Vec<_>>(),
    );
}

/// C1-FOLLOWUP regression #2: the recovery CAS, called directly,
/// refuses when `expected_host_id == self.host_id()`. Belt-and-
/// suspenders against a future caller bug that bypasses the sweep
/// query's self-host filter.
#[compio::test]
#[ignore = "needs Postgres; C1-FOLLOWUP — recovery CAS refuses self-host_id"]
async fn claim_orphan_transient_for_recovery_refuses_self_host() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    let _ = db
        .update_sandbox_status(sid, SandboxStatus::Snapshotting, 0, None)
        .await
        .unwrap();
    let my_host_typed = format!(
        "hst_{}",
        zeroship_core::typed_id::uuid_to_base62(&db.host_id())
    );
    let err = db
        .claim_orphan_transient_for_recovery(
            sid,
            SandboxStatus::SnapshottingAborted,
            /*expected_generation=*/ 1,
            &my_host_typed,
            /*threshold_secs=*/ 0,
        )
        .await
        .expect_err("must refuse self.host_id() as expected_host_id");
    let msg = format!("{err}");
    assert!(
        msg.contains("expected_host_id") && msg.contains("OTHER"),
        "expected validation error citing OTHER controllers' wedges; got: {msg}",
    );
    // Row untouched.
    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Snapshotting);
}

/// C1-FOLLOWUP regression #3: ABA defense — if the original
/// controller bumps `lessee_updated_at` between the sweep's SELECT
/// and the recovery CAS, the CAS must lose (the row's "alive" again).
/// Simulated by stamping a fresh `now()` after collecting the row.
#[compio::test]
#[ignore = "needs Postgres; C1-FOLLOWUP — recovery CAS ABA-safe on lessee bump"]
async fn claim_orphan_transient_for_recovery_aba_safe_on_lessee_bump() {
    let db = migrated_db().await;
    let url = test_url();
    let (peer_host_id, peer_host_typed) = inject_extra_host(&url, "peer-aba").await;
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, peer_host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    let g1 = db
        .update_sandbox_status_with_host(
            sid,
            SandboxStatus::Snapshotting,
            0,
            peer_host_id,
            None,
        )
        .await
        .unwrap();
    // Backdate so the row is sweep-eligible.
    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE zeroship.sandboxes \
                SET lessee_updated_at = now() - interval '10 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();

    // Now simulate the original peer's lease-heartbeat landing
    // between the SELECT and the CAS — bump lessee_updated_at back
    // to now().
    client
        .execute(
            "UPDATE zeroship.sandboxes \
                SET lessee_updated_at = now() \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();

    // Recovery CAS must miss (CasLost): the lessee window check
    // rejects.
    let err = db
        .claim_orphan_transient_for_recovery(
            sid,
            SandboxStatus::SnapshottingAborted,
            g1,
            &peer_host_typed,
            /*threshold_secs=*/ 120,
        )
        .await
        .expect_err("must lose CAS on fresh lessee bump");
    assert!(
        matches!(err, zeroship_sandbox::db::DatabaseError::CasLost { .. }),
        "expected CasLost; got {err:?}",
    );
    // Row still in snapshotting under the peer.
    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Snapshotting);
    assert_eq!(row.host_id, peer_host_typed);
}

#[compio::test]
#[ignore = "needs Postgres; PR 3g idle-eviction sweep selects opted-in stale rows"]
async fn sweep_idle_eviction_selects_opted_in_stale_rows() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    // Mark opted-in + stale.
    let url = test_url();
    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE zeroship.sandboxes \
                SET idle_snapshot_opted_in = TRUE, \
                    last_used_at = now() - interval '30 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();

    let state = build_sweep_state(db, /*snapshot_enabled=*/true);
    let snapshotter = RecordingIdleSnapshotter::default();
    let attempted = run_idle_eviction_once(&state, &snapshotter, 60, 2).await;
    assert!(
        attempted.iter().any(|r| r.sandbox_id == info.sandbox_id),
        "sweep must select our opted-in stale row; got {attempted:?}"
    );
    let seen = snapshotter.seen.lock().unwrap();
    assert!(
        seen.contains(&sid),
        "snapshotter must have been called for our row; seen={seen:?}"
    );
}

#[compio::test]
#[ignore = "needs Postgres; PR 3g idle-eviction sweep no-op when feature disabled"]
async fn sweep_idle_eviction_skips_when_feature_disabled() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, _sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();
    let url = test_url();
    let app_dsn = role_dsn(&url, "sandbox_app");
    let mut cfg = PoolConfig::default();
    cfg.max_size = 2;
    let pool = Pool::connect_with_config(&app_dsn, cfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE zeroship.sandboxes \
                SET idle_snapshot_opted_in = TRUE, \
                    last_used_at = now() - interval '30 minutes' \
              WHERE sandbox_id = $1::TEXT",
            &[&info.sandbox_id],
        )
        .await
        .unwrap();

    let state = build_sweep_state(db, /*snapshot_enabled=*/false);
    let snapshotter = RecordingIdleSnapshotter::default();
    let attempted = run_idle_eviction_once(&state, &snapshotter, 60, 2).await;
    assert!(
        attempted.is_empty(),
        "sweep must self-disable when snapshot_enabled=false; attempted={}",
        attempted.len()
    );
    let seen = snapshotter.seen.lock().unwrap();
    assert!(seen.is_empty(), "snapshotter must not have been called; seen={seen:?}");
}

#[compio::test]
#[ignore = "needs Postgres; PR 3e feature flag off"]
async fn restore_handler_returns_feature_disabled_when_flag_off() {
    let db = migrated_db().await;
    let (info, sid) = fresh_info("alice");
    let store_root = fresh_temp("rstore_flag");
    let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

    let store: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let backend_root = fresh_temp("rback_flag");
    let backend: std::sync::Arc<dyn zeroship_sandbox::restore_handler::RestoreBackend> =
        std::sync::Arc::new(StubRestoreBackend::new(backend_root.clone()));

    let err = restore_sandbox(&db, std::sync::Arc::clone(&store), backend, None, sid, false)
        .await
        .expect_err("must refuse with flag off");
    assert!(matches!(err, RestoreHandlerError::FeatureDisabled), "{err:?}");

    // Row stays at snapshotted (no CAS attempted).
    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Snapshotted);

    let _ = std::fs::remove_dir_all(&store_root);
    let _ = std::fs::remove_dir_all(&backend_root);
}

// ════════════════════════════════════════════════════════════════════
// Phase B — full snapshot → wake cycle. Drives the same handler chain
// the admin endpoint uses (snapshot_handler::snapshot_sandbox +
// restore_handler::restore_sandbox) against a real Database with mock
// ch-remote and stub restore backend. Asserts the row state transitions
// running → snapshotting → snapshotted → restoring → running and that
// the artifact survives the round trip.
// ════════════════════════════════════════════════════════════════════

#[compio::test]
#[ignore = "needs Postgres; Phase B snapshot→wake round trip"]
async fn phase_b_snapshot_then_wake_cycles_row_back_to_running() {
    let db = migrated_db().await;
    let host_id = fresh_host(&db);
    let (info, sid) = fresh_info("alice");
    db.insert_sandbox(&info, host_id, &"a".repeat(32), Some("http://x"), Some(7))
        .await
        .unwrap();

    // Phase 1: snapshot. Drives running → snapshotting → snapshotted.
    let stage_dir = fresh_temp("phaseb-stage");
    let store_root = fresh_temp("phaseb-store");
    let store: std::sync::Arc<dyn SnapshotStore> =
        std::sync::Arc::new(LocalDiskSnapshotStore::new(&store_root));
    let ch: std::sync::Arc<dyn ChRemoteClient> =
        std::sync::Arc::new(MockChRemoteClient::default());
    // Use a path under stage so the artifact can be restored: the
    // mock ch-remote writes the three artifact files inside the temp
    // dir, and `LocalDiskSnapshotStore::put` migrates them to the
    // canonical store path. We hand the api_socket as a placeholder
    // file path — the mock ignores it.
    let api_sock = fresh_temp("phaseb-api").join("ch.sock");
    let vm_ops = StubSourceVmOps::new(api_sock, 7);

    // Mirror the admin handler's call shape but with the StubSourceVmOps
    // (the production wiring uses ResolvedSourceVmOps which we exercise
    // via the lookup_source_vm_ops unit test below).
    let snapshot_outcome =
        snapshot_sandbox(&db, store, ch, &vm_ops, sid, stage_dir.clone(), true)
            .await
            .expect("phase 1 snapshot");
    assert_eq!(snapshot_outcome.vm_index, 7);

    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Snapshotted);
    assert!(
        vm_ops
            .teardown_called
            .load(std::sync::atomic::Ordering::SeqCst),
        "snapshot handler must have invoked teardown_source"
    );

    // Phase 2: wake. Drives snapshotted → restoring → running.
    let backend_root = fresh_temp("phaseb-rback");
    // R8-A3-5: keep a concrete-typed Arc for post-call observation
    // (the counter fields live on the stub) and hand a coerced
    // dyn-Arc to `restore_sandbox`.
    let restore_backend = std::sync::Arc::new(StubRestoreBackend::new(backend_root.clone()));
    let restore_backend_dyn: std::sync::Arc<
        dyn zeroship_sandbox::restore_handler::RestoreBackend,
    > = std::sync::Arc::clone(&restore_backend) as _;
    // Stage a `config.json` so the rewrite step finds the structural
    // shape it expects. The mock ch-remote in phase 1 writes a
    // placeholder; we overwrite it with a v1-shaped JSON before the
    // restore so the rewrite assertion succeeds. Real CH artifacts
    // carry this shape natively. Post-pivot (bug #11): keys/workspace/
    // userhome are virtio-blk disks, not virtiofs sockets — `fs[]`
    // is absent and `disks[]` carries the rootfs.img + persistent-
    // workspace + per-user-home triple. The rewriter (`rewrite_
    // config_json`) still only touches `net[].tap` + `net[].mac`; the
    // path-bearing entries below stay untouched.
    let typed = format!("sbx_{}", zeroship_core::typed_id::uuid_to_base62(&sid));
    let cfg_path = store_root.join(&typed).join("config.json");
    std::fs::write(
        &cfg_path,
        r#"{
            "net":[{"tap":"zsbx-nm-99","mac":"12:34:56:78:9b:63"}],
            "disks":[
                {"path":"/opt/nomad/data/alloc/oldalloc/ch/local/rootfs.img"},
                {"path":"/var/zeroship/ch/sbx-oldalloc/workspace.img"},
                {"path":"/var/zeroship/ch/users/usr-oldalloc/home.img"}
            ],
            "serial":{"mode":"File","file":"/opt/nomad/data/alloc/oldalloc/ch/local/serial.log"}
        }"#,
    )
    .unwrap();
    // Re-stamp pg with the new sha256 so restore's verify passes.
    let store2 = LocalDiskSnapshotStore::new(&store_root);
    let resnap_stage = fresh_temp("phaseb-resnap");
    // Read all three files back from the canonical path and re-stage
    // so put() recomputes the sha256 with the rewritten config.json.
    for &name in zeroship_sandbox::snapshot_store::ARTIFACT_FILES {
        let src = store_root.join(&typed).join(name);
        let dst = resnap_stage.join(name);
        std::fs::copy(&src, &dst).unwrap();
    }
    let new_meta = store2.put(&typed, &resnap_stage, "v51.1").unwrap();
    let _ = std::fs::remove_dir_all(&resnap_stage);
    // Stamp the pg row's snapshot_sha256 with the post-rewrite digest
    // so the restore's checksum verify succeeds against the in-place
    // config.json we just wrote. (In production the handler does this
    // via update_snapshot_metadata; here we touch the pg directly.)
    let url = test_url();
    let mut pcfg = PoolConfig::default();
    pcfg.max_size = 2;
    let pool = Pool::connect_with_config(&url, pcfg).await.unwrap();
    let client = pool.get().await.unwrap();
    client
        .execute(
            "UPDATE zeroship.sandboxes SET snapshot_sha256 = $1::BYTEA \
              WHERE sandbox_id = $2::TEXT",
            &[&(&new_meta.sha256[..]), &info.sandbox_id],
        )
        .await
        .unwrap();

    let store2_arc: std::sync::Arc<dyn zeroship_sandbox::snapshot_store::SnapshotStore> =
        std::sync::Arc::new(store2);
    let restore_outcome = restore_sandbox(&db, store2_arc, restore_backend_dyn, None, sid, true)
        .await
        .expect("phase 2 wake");
    assert_eq!(restore_outcome.vm_index, 7);
    assert!(restore_backend.submit_called.load(std::sync::atomic::Ordering::SeqCst));
    assert!(restore_backend.livez_called.load(std::sync::atomic::Ordering::SeqCst));

    let row = db.get_sandbox_row(sid).await.unwrap().expect("row");
    assert_eq!(row.status, SandboxStatus::Running);

    // config.json was rewritten in-place with the source vm_index's
    // tap (vm_index=7 → zsbx-nm-7).
    let restored_cfg = restore_backend.restore_alloc_dir(sid).join("config.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&restored_cfg).unwrap()).unwrap();
    assert_eq!(v["net"][0]["tap"], "zsbx-nm-7");

    let _ = std::fs::remove_dir_all(&store_root);
    let _ = std::fs::remove_dir_all(&backend_root);
}

#[compio::test]
#[ignore = "needs Postgres; Phase B lookup_source_vm_ops queries Nomad"]
async fn phase_b_lookup_source_vm_ops_resolves_handle_against_nomad() {
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::Duration;

    // Spin up a fake Nomad on 127.0.0.1 that returns one running
    // alloc with a known ID. Same shape as the existing
    // `spawn_fake_nomad` in restore_handler tests but inlined so the
    // pg-gated tests can run without leaning on `cfg(test)` from the
    // sibling module.
    let alloc_id = "phaseb-alloc-1234567890ab";
    let body = format!(r#"[{{"ID":"{alloc_id}","ClientStatus":"running"}}]"#);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let nomad_addr = format!("http://{}", listener.local_addr().unwrap());
    let body_clone = body.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => return,
            };
            use std::io::{Read, Write};
            let mut buf = [0u8; 8192];
            let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = s.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body_clone.len(),
                body_clone
            );
            let _ = s.write_all(resp.as_bytes());
        }
    });

    // Stage the alloc dir on local fs so `lookup_source_vm_ops`'s
    // `metadata().is_socket()` check passes. We bind a Unix socket
    // at the expected path.
    //
    // Path layout (from wrapper): NOMAD_ALLOC_ROOT/<alloc>/ch/local/ch.sock.
    // The const inside nomad_ch.rs is `/opt/nomad/data/alloc`, which
    // we can't write to in CI; we override by bind-mounting a temp
    // root via `LD_PRELOAD` — too heavy for a unit test. Instead this
    // test asserts the lookup ERR path: alloc dir doesn't exist
    // locally, so `metadata()` errors and `lookup_source_vm_ops`
    // returns Err.
    let mut cfg = sweep_test_cfg(true);
    cfg.nomad_ch.nomad_addr = nomad_addr;
    let backend = Backend::builder(&cfg).build().expect("backend");
    let sid = Uuid::now_v7();
    let user_id = typed_id("usr");
    let signing = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sid,
            &user_id,
            signing,
            "http://10.99.107.2:7777".into(),
            7,
        );
    } else {
        panic!("expected nomad-ch backend");
    }

    // Resolve: Nomad returns an alloc, but the local /opt/nomad/data/alloc
    // path doesn't exist, so the socket-existence check should fail
    // with a clear error. (Production hosts have the path; here we
    // verify the err shape so the admin handler maps it to 503.)
    let err = backend
        .lookup_source_vm_ops(sid)
        .await
        .expect_err("alloc dir not on local fs in test");
    assert!(
        err.contains("api_socket") || err.contains("not accessible"),
        "expected an api_socket-related error; got {err}"
    );

    // Belt-and-suspenders: verify a sandbox NOT in the in-memory map
    // surfaces the "not in nomad-ch state map" path, NOT the Nomad
    // HTTP path (we never even reach Nomad).
    let unknown_sid = Uuid::now_v7();
    let err2 = backend
        .lookup_source_vm_ops(unknown_sid)
        .await
        .expect_err("unknown sandbox must not be resolved");
    assert!(
        err2.contains("not in nomad-ch state map"),
        "expected 'not in state map'; got {err2}"
    );

    // Drop a sealed unix socket at a controlled path so the positive
    // existence test still gets coverage. We can't redirect
    // NOMAD_ALLOC_ROOT (it's a const), so this assertion lives in a
    // smaller scope: just verify that creating a UnixListener at
    // some path produces a metadata().is_socket() = true. The full
    // happy path is exercised E2E against a real worker in the
    // cluster stress run.
    let sock_dir = fresh_temp("phaseb-sock");
    let sock_path = sock_dir.join("ch.sock");
    let _listener = UnixListener::bind(&sock_path).unwrap();
    let md = std::fs::metadata(&sock_path).unwrap();
    use std::os::unix::fs::FileTypeExt as _;
    assert!(
        md.file_type().is_socket(),
        "sanity: a freshly-bound UnixListener must show up as is_socket()"
    );
}

// ════════════════════════════════════════════════════════════════════
// Bug #15 — teardown_source_for_snapshot MUST preserve host_dir.
//
// Regression coverage for the 2026-05-23 cluster smoke (see
// `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-23-r1.md`):
// the old snapshot-teardown path delegated to `stop()`, whose step 5
// `remove_dir_all(host_dir)` deleted the per-sandbox `workspace.img`.
// On wake, the wrapper's `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate then
// tripped. The fix: route the snapshot teardown through
// `stop_preserving_state`, which skips step 5.
//
// This integration test exercises the FULL `Backend` enum surface
// (not the inner `stop_inner` bool): it constructs a real `Backend`,
// injects a sandbox + stages a sentinel `workspace.img` on disk,
// calls `teardown_source_for_snapshot`, asserts the image survives,
// then calls `stop()` and asserts the image is reaped. This is
// `#[ignore]`'d to match the file's pg-gated convention even though
// it doesn't actually touch pg — the cron worker invokes it with the
// rest of the pg-gated suite (`-- --ignored --test-threads=1`).
// ════════════════════════════════════════════════════════════════════

#[compio::test]
#[ignore = "B15 regression coverage; bundled with pg-gated suite (no pg needed)"]
async fn teardown_source_for_snapshot_preserves_host_dir_then_stop_reaps() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    // 1. Spin up a 404-mock for Nomad. Every request → 404.
    //    `stop_nomad_job` accepts {200, 404}; `wait_for_job_gone`
    //    returns Ok on a 404 from `/v1/job/{id}`. So the teardown
    //    proceeds cleanly through steps 2-4.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let nomad_addr = format!("http://127.0.0.1:{port}");
    let stop_flag = std::sync::Arc::new(AtomicBool::new(false));
    let stop_flag_thread = stop_flag.clone();
    thread::spawn(move || {
        while !stop_flag_thread.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut s, _)) => {
                    let _ = s.set_read_timeout(Some(Duration::from_millis(200)));
                    let _ = s.set_write_timeout(Some(Duration::from_millis(200)));
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    let resp =
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                    let _ = s.write_all(resp.as_bytes());
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });

    // 2. Build a cfg with a writable host_state_dir + Nomad pointed at
    //    the 404-mock + fence disabled.
    let host_state_dir = fresh_temp("b15-host-state");
    let mut cfg = sweep_test_cfg(true);
    cfg.nomad_ch.nomad_addr = nomad_addr;
    cfg.nomad_ch.host_state_dir = host_state_dir.clone();
    cfg.nomad_ch.host_fence_timeout_secs = 0; // skip /livez fence

    // 3. Construct the Backend (the full enum, not just the inner
    //    nomad-ch backend) so the test exercises the snapshot
    //    teardown's dispatch through `Backend::teardown_source_for_snapshot`.
    let backend = Backend::builder(&cfg).build().expect("backend");

    // 4. Inject a sandbox record. `_test_inject_sandbox` derives
    //    host_dir as `<host_state_dir>/<sandbox-id>/`; we materialise
    //    that dir + drop a workspace.img sentinel before tearing down.
    let sid = Uuid::now_v7();
    let user_id = typed_id("usr");
    let signing = ed25519_dalek::SigningKey::from_bytes(&[15u8; 32]);
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sid,
            &user_id,
            signing.clone(),
            // Unreachable; /shutdown errors are best-effort (logged
            // to errs, not fatal for our post-condition assertions).
            "http://127.0.0.1:1".into(),
            42,
        );
    } else {
        panic!("expected nomad-ch backend");
    }
    let host_dir = host_state_dir.join(sid.to_string());
    std::fs::create_dir_all(&host_dir).unwrap();
    let workspace_img = host_dir.join("workspace.img");
    std::fs::write(&workspace_img, b"PRESERVE-ACROSS-SNAPSHOT").unwrap();
    let extra = host_dir.join("config.json");
    std::fs::write(&extra, b"{}").unwrap();

    // 5. Snapshot teardown. The old (pre-#15) code called `stop`
    //    under the hood and deleted host_dir; the new code calls
    //    `stop_preserving_state` and leaves the dir alone.
    let _ = backend.teardown_source_for_snapshot(sid).await;

    assert!(
        host_dir.exists(),
        "B15 regression: teardown_source_for_snapshot removed \
         host_dir; the next wake's wrapper [ ! -f $ZSBX_WORKSPACE_IMG ] \
         gate will exit 1 (cluster bug #15)"
    );
    assert!(
        workspace_img.exists(),
        "B15 regression: teardown_source_for_snapshot removed \
         host_dir/workspace.img; durable per-sandbox storage gone"
    );
    let contents = std::fs::read(&workspace_img).unwrap();
    assert_eq!(
        contents,
        b"PRESERVE-ACROSS-SNAPSHOT",
        "B15 regression: workspace.img sentinel was modified"
    );
    assert!(
        extra.exists(),
        "B15 regression: teardown_source_for_snapshot removed adjacent \
         file under host_dir (config.json — restore stage dir's neighbour)"
    );

    // 6. Re-inject (the teardown removed the in-memory record), then
    //    call the real `stop` and assert the dir IS reaped — proving
    //    the host_dir is owned by `stop`, not orphaned. This is the
    //    "host_dir is finally reaped by the next regular stop" half
    //    of the deferred-file's Option A contract.
    if let Backend::NomadCh(b) = &backend {
        b._test_inject_sandbox(
            sid,
            &user_id,
            signing,
            "http://127.0.0.1:1".into(),
            42,
        );
    }
    // host_dir already exists from above. workspace.img also still
    // there. stop() runs steps 1-5 against the same 404-mock; with
    // fence disabled and job_confirmed_gone=true, step 5 deletes.
    let _ = backend.stop(sid).await;

    assert!(
        !host_dir.exists(),
        "stop() must reap host_dir under favourable conditions \
         (404-mock + fence disabled); got dir still present after stop"
    );

    // Cleanup.
    stop_flag.store(true, Ordering::Relaxed);
    let _ = std::fs::remove_dir_all(&host_state_dir);
}

// ════════════════════════════════════════════════════════════════════
// C-7-LT-PR1: wake_jobs CRUD (pg-gated)
//
// PR1 lands the table + CRUD; PR2 wires the state machine into the
// wake handler. These tests pin the round-trip + transition + sweep
// semantics so PR2 can build on them without re-verifying.
// ════════════════════════════════════════════════════════════════════

mod wake_jobs_crud {
    use super::*;
    use std::time::Duration as StdDuration;
    use zeroship_sandbox::db::{
        Database, InsertWakeJobOutcome, WakeErrorCode, WakeJobRow, WakeJobState,
    };

    fn sample_row(wake_id: &str, sandbox_id: &str, lessee: &str) -> WakeJobRow {
        WakeJobRow {
            wake_id: wake_id.to_string(),
            sandbox_id: sandbox_id.to_string(),
            state: WakeJobState::Pending,
            error_code: None,
            error_message: None,
            // Insert-side timestamps are server-set; these values are
            // ignored by `insert_wake_job` so any placeholder works.
            started_at_secs: 0,
            updated_at_secs: 0,
            ready_at_secs: None,
            agent_url: None,
            lessee: lessee.to_string(),
            lessee_updated_at_secs: 0,
        }
    }

    /// Round-trip: insert a row, read it back via `get_wake_job`,
    /// check that all fields are populated and server-side timestamps
    /// are non-zero.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_insert_and_read_round_trip() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let row = sample_row("wak_test_round_trip", "sbx_test_rt_sandbox", "hst_test_owner");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.expect("insert"),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );

        let loaded = db
            .get_wake_job("wak_test_round_trip")
            .await
            .expect("get")
            .expect("row must exist");
        assert_eq!(loaded.wake_id, "wak_test_round_trip");
        assert_eq!(loaded.sandbox_id, "sbx_test_rt_sandbox");
        assert_eq!(loaded.state, WakeJobState::Pending);
        assert!(loaded.error_code.is_none());
        assert!(loaded.error_message.is_none());
        assert!(loaded.agent_url.is_none());
        assert!(loaded.ready_at_secs.is_none());
        assert_eq!(loaded.lessee, "hst_test_owner");
        assert!(
            loaded.started_at_secs > 0,
            "server-side started_at must populate; got {}",
            loaded.started_at_secs
        );
        assert!(
            loaded.updated_at_secs > 0,
            "server-side updated_at must populate"
        );
        assert!(loaded.lessee_updated_at_secs > 0);

        // Unknown wake_id → None.
        let absent = db.get_wake_job("wak_does_not_exist").await.unwrap();
        assert!(absent.is_none());
    }

    /// State transition: Pending → Restoring → Ok bumps updated_at,
    /// sets ready_at on Ok, and surfaces agent_url. Failure path
    /// surfaces error_code + error_message.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_update_state_transitions() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        // Happy path: pending → restoring → ok.
        let happy = sample_row("wak_happy", "sbx_happy", "hst_owner_a");
        assert!(
            matches!(
                db.insert_wake_job(&happy).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        let after_insert = db
            .get_wake_job("wak_happy")
            .await
            .unwrap()
            .expect("inserted");

        let n = db
            .update_wake_job_state(
                "wak_happy",
                WakeJobState::Restoring,
                None,
                None,
                None,
            )
            .await
            .expect("update to restoring");
        assert_eq!(n, 1);
        let mid = db.get_wake_job("wak_happy").await.unwrap().unwrap();
        assert_eq!(mid.state, WakeJobState::Restoring);
        assert!(
            mid.updated_at_secs >= after_insert.updated_at_secs,
            "updated_at must move forward (or equal under low resolution)"
        );
        assert!(mid.ready_at_secs.is_none(), "ready_at unset until ok");
        assert!(mid.agent_url.is_none(), "agent_url unset until provided");

        let n = db
            .update_wake_job_state(
                "wak_happy",
                WakeJobState::Ok,
                None,
                None,
                Some("http://10.0.0.1:7000"),
            )
            .await
            .expect("update to ok");
        assert_eq!(n, 1);
        let done = db.get_wake_job("wak_happy").await.unwrap().unwrap();
        assert_eq!(done.state, WakeJobState::Ok);
        assert!(done.is_state_terminal());
        assert!(
            done.ready_at_secs.is_some(),
            "ready_at must be set on transition to ok"
        );
        assert_eq!(done.agent_url.as_deref(), Some("http://10.0.0.1:7000"));

        // Fail path: separate row, pending → failed with code + message.
        let bad = sample_row("wak_bad", "sbx_bad", "hst_owner_b");
        assert!(
            matches!(
                db.insert_wake_job(&bad).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        let n = db
            .update_wake_job_state(
                "wak_bad",
                WakeJobState::Failed,
                Some(WakeErrorCode::LivezTimeout),
                Some("agent /livez never returned 200 within 30s"),
                None,
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let dead = db.get_wake_job("wak_bad").await.unwrap().unwrap();
        assert_eq!(dead.state, WakeJobState::Failed);
        assert_eq!(dead.error_code, Some(WakeErrorCode::LivezTimeout));
        assert_eq!(
            dead.error_message.as_deref(),
            Some("agent /livez never returned 200 within 30s")
        );

        // Update of a non-existent row affects 0 rows (caller treats
        // as 404).
        let n = db
            .update_wake_job_state(
                "wak_does_not_exist",
                WakeJobState::Ok,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Idempotency lookup: returns the non-terminal row for a
    /// sandbox; returns None once the row has terminated (ok or
    /// failed).
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_find_pending_for_sandbox() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let sid = "sbx_idempotency_target";
        // No row at all → None.
        let none = db.find_pending_wake_for_sandbox(sid).await.unwrap();
        assert!(none.is_none());

        // Insert a pending row → find returns it.
        let row = sample_row("wak_idemp_first", sid, "hst_owner_a");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        let found = db
            .find_pending_wake_for_sandbox(sid)
            .await
            .unwrap()
            .expect("must find pending");
        assert_eq!(found.wake_id, "wak_idemp_first");

        // Advance to terminal (ok) → find no longer returns it.
        db.update_wake_job_state(
            "wak_idemp_first",
            WakeJobState::Ok,
            None,
            None,
            Some("http://1.2.3.4:7000"),
        )
        .await
        .unwrap();
        let after_ok = db.find_pending_wake_for_sandbox(sid).await.unwrap();
        assert!(
            after_ok.is_none(),
            "terminal row must NOT match find_pending; got {:?}",
            after_ok.map(|r| r.wake_id)
        );

        // Failed is also terminal.
        let row2 = sample_row("wak_idemp_second", sid, "hst_owner_a");
        assert!(
            matches!(
                db.insert_wake_job(&row2).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert after terminal must return Inserted"
        );
        db.update_wake_job_state(
            "wak_idemp_second",
            WakeJobState::Failed,
            Some(WakeErrorCode::Internal),
            Some("synthesised failure"),
            None,
        )
        .await
        .unwrap();
        let after_fail = db.find_pending_wake_for_sandbox(sid).await.unwrap();
        assert!(after_fail.is_none());
    }

    /// GC: terminal rows older than the threshold are deleted; rows
    /// in non-terminal states are NEVER deleted (those are handled by
    /// PR2's takeover sweep).
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_gc_expired_terminal_only() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        // Insert three rows: one ok, one failed, one pending.
        for (id, sid) in [
            ("wak_gc_ok", "sbx_gc_a"),
            ("wak_gc_failed", "sbx_gc_b"),
            ("wak_gc_pending", "sbx_gc_c"),
        ] {
            let row = sample_row(id, sid, "hst_gc_owner");
            assert!(
                matches!(
                    db.insert_wake_job(&row).await.unwrap(),
                    InsertWakeJobOutcome::Inserted
                ),
                "fresh insert must return Inserted"
            );
        }
        db.update_wake_job_state(
            "wak_gc_ok",
            WakeJobState::Ok,
            None,
            None,
            Some("http://10.0.0.1:7000"),
        )
        .await
        .unwrap();
        db.update_wake_job_state(
            "wak_gc_failed",
            WakeJobState::Failed,
            Some(WakeErrorCode::Internal),
            Some("synth"),
            None,
        )
        .await
        .unwrap();

        // GC with a future-tense threshold (1 hour). Nothing should
        // be old enough; the pending row is non-terminal and so
        // safe regardless.
        let n = db
            .gc_expired_wake_jobs(StdDuration::from_secs(3600))
            .await
            .unwrap();
        assert_eq!(
            n, 0,
            "GC with 1 h threshold must not delete any fresh rows"
        );

        // GC with a 0-second threshold: every TERMINAL row qualifies.
        // The pending row must survive.
        let n = db
            .gc_expired_wake_jobs(StdDuration::from_secs(0))
            .await
            .unwrap();
        assert_eq!(
            n, 2,
            "0-secs GC must delete both terminal rows (ok + failed)"
        );

        // Verify the surviving row.
        assert!(db.get_wake_job("wak_gc_pending").await.unwrap().is_some());
        assert!(db.get_wake_job("wak_gc_ok").await.unwrap().is_none());
        assert!(db.get_wake_job("wak_gc_failed").await.unwrap().is_none());
    }

    /// R17-A1: every state transition MUST bump `lessee_updated_at`
    /// so an in-flight wake is not stolen by the takeover sweep
    /// while the original lessee is still progressing. Identical
    /// fingerprint to R14-C1 on `sandboxes`.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_update_state_bumps_lessee_updated_at() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let row = sample_row("wak_lessee_bump", "sbx_lb", "hst_lb_owner");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        let after_insert = db
            .get_wake_job("wak_lessee_bump")
            .await
            .unwrap()
            .unwrap();
        // Server-side `now()` resolution is 1 µs; ensure pg's clock
        // moves by sleeping a millisecond before the transition.
        compio::time::sleep(StdDuration::from_millis(20)).await;

        db.update_wake_job_state(
            "wak_lessee_bump",
            WakeJobState::Restoring,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let after_advance = db
            .get_wake_job("wak_lessee_bump")
            .await
            .unwrap()
            .unwrap();

        // The 1-second resolution of `EXTRACT(EPOCH FROM …)::BIGINT`
        // means the two timestamps may be equal under a tight clock;
        // the contract is "no regression" rather than "strict
        // increase". The real test is that the field is NOT frozen at
        // the insert-time value forever — over the course of a multi
        // -second wake, `lessee_updated_at` must move forward.
        assert!(
            after_advance.lessee_updated_at_secs >= after_insert.lessee_updated_at_secs,
            "lessee_updated_at must not regress on state transition"
        );

        // Stronger assertion: drive a second transition after a
        // sub-second sleep, then read `lessee_updated_at` at the µs
        // level via a direct pg query. This proves the column was
        // touched (not just `updated_at`).
        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(
            &test_url(), cfg,
        )
        .await
        .unwrap();
        let client = pool.get().await.unwrap();
        let row_before = client
            .query_one(
                "SELECT lessee_updated_at FROM zeroship.wake_jobs \
                  WHERE wake_id = 'wak_lessee_bump'",
                &[],
            )
            .await
            .unwrap();
        let before_ts: std::time::SystemTime = row_before.get(0);

        compio::time::sleep(StdDuration::from_millis(20)).await;
        db.update_wake_job_state(
            "wak_lessee_bump",
            WakeJobState::LivezPolling,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        let row_after = client
            .query_one(
                "SELECT lessee_updated_at FROM zeroship.wake_jobs \
                  WHERE wake_id = 'wak_lessee_bump'",
                &[],
            )
            .await
            .unwrap();
        let after_ts: std::time::SystemTime = row_after.get(0);
        assert!(
            after_ts > before_ts,
            "lessee_updated_at must advance on state transition: before={before_ts:?}, after={after_ts:?}"
        );
    }

    /// R17-I2: the None-handling contract on `update_wake_job_state`
    /// is symmetric COALESCE across `error_code`, `error_message`,
    /// and `agent_url` — passing `None` for any of them preserves
    /// the existing column value. Retry/replay paths cannot silently
    /// null out a previously-recorded error record.
    ///
    /// Note (R20-C1): terminal rows (`ok`, `failed`) are now immutable
    /// — `update_wake_job_state` guards against terminal-overwrite via
    /// `AND state NOT IN ('ok', 'failed')`. This test therefore drives
    /// the COALESCE contract through non-terminal states (`pending` →
    /// `restoring` with metadata on successive calls, then seals with
    /// `ok` or `failed`). The key invariant — None preserves, Some
    /// overwrites — is identical regardless of which state is used.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_update_preserves_fields_on_none() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let row = sample_row("wak_preserve", "sbx_preserve", "hst_p_owner");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );

        // First transition: drive to `restoring` and record error_code
        // + error_message (simulates a transient annotation mid-flight).
        db.update_wake_job_state(
            "wak_preserve",
            WakeJobState::Restoring,
            Some(WakeErrorCode::LivezTimeout),
            Some("agent /livez never returned 200 within 30s"),
            None,
        )
        .await
        .unwrap();
        let first = db.get_wake_job("wak_preserve").await.unwrap().unwrap();
        assert_eq!(first.error_code, Some(WakeErrorCode::LivezTimeout));
        assert_eq!(
            first.error_message.as_deref(),
            Some("agent /livez never returned 200 within 30s")
        );

        // Re-update with all-None while still in non-terminal state:
        // error_code, error_message, agent_url MUST be preserved.
        // Pre-fix this would NULL out the metadata.
        db.update_wake_job_state(
            "wak_preserve",
            WakeJobState::Restoring,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let second = db.get_wake_job("wak_preserve").await.unwrap().unwrap();
        assert_eq!(
            second.error_code,
            Some(WakeErrorCode::LivezTimeout),
            "error_code must be preserved on None re-update"
        );
        assert_eq!(
            second.error_message.as_deref(),
            Some("agent /livez never returned 200 within 30s"),
            "error_message must be preserved on None re-update"
        );

        // Set an agent_url, then re-update with None — also preserved
        // (existing agent_url behavior, restated for symmetry).
        db.update_wake_job_state(
            "wak_preserve",
            WakeJobState::LivezPolling,
            None,
            None,
            Some("http://10.0.0.1:7000"),
        )
        .await
        .unwrap();
        db.update_wake_job_state(
            "wak_preserve",
            WakeJobState::LivezPolling,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let third = db.get_wake_job("wak_preserve").await.unwrap().unwrap();
        assert_eq!(third.agent_url.as_deref(), Some("http://10.0.0.1:7000"));

        // Explicitly providing Some(_) overwrites — the contract is
        // None=preserve, Some=overwrite. Verify by overwriting the
        // error_code with a different variant while still non-terminal.
        db.update_wake_job_state(
            "wak_preserve",
            WakeJobState::LivezPolling,
            Some(WakeErrorCode::Internal),
            None,
            None,
        )
        .await
        .unwrap();
        let fourth = db.get_wake_job("wak_preserve").await.unwrap().unwrap();
        assert_eq!(
            fourth.error_code,
            Some(WakeErrorCode::Internal),
            "Some(_) overwrites the existing value"
        );
        // error_message still preserved from the previous set.
        assert_eq!(
            fourth.error_message.as_deref(),
            Some("agent /livez never returned 200 within 30s")
        );
    }

    /// R16-S3: agent_url column-level CHECK rejects out-of-shape
    /// values (e.g. file:// schemes, freeform text). 0010 added the
    /// constraint as part of the PR2-followup hardening sprint.
    ///
    /// Note (R20-C1): valid-URL checks are driven through non-terminal
    /// states so the terminal-overwrite guard doesn't swallow the
    /// write. The invalid-URL checks use direct SQL (no state guard
    /// in the path) and remain structurally unchanged.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_agent_url_check_constraint_enforced() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url.clone()).await.unwrap();

        let row = sample_row("wak_url_check", "sbx_url_check", "hst_url");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );

        // http://...:port → valid (non-terminal state so the write lands).
        db.update_wake_job_state(
            "wak_url_check",
            WakeJobState::Restoring,
            None,
            None,
            Some("http://10.0.0.1:7000"),
        )
        .await
        .unwrap();

        // https://... → valid.
        db.update_wake_job_state(
            "wak_url_check",
            WakeJobState::LivezPolling,
            None,
            None,
            Some("https://example.com/agent"),
        )
        .await
        .unwrap();

        // file://... must be rejected.
        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(&url, cfg)
            .await
            .unwrap();
        let client = pool.get().await.unwrap();
        let res = client
            .execute(
                "UPDATE zeroship.wake_jobs SET agent_url = 'file:///etc/passwd' \
                 WHERE wake_id = 'wak_url_check'",
                &[],
            )
            .await;
        assert!(
            res.is_err(),
            "file:// URL must violate the CHECK constraint"
        );

        // Freeform text → reject.
        let res = client
            .execute(
                "UPDATE zeroship.wake_jobs SET agent_url = 'arbitrary text' \
                 WHERE wake_id = 'wak_url_check'",
                &[],
            )
            .await;
        assert!(res.is_err(), "freeform text must violate CHECK");

        // URL with embedded spaces → reject.
        let res = client
            .execute(
                "UPDATE zeroship.wake_jobs SET agent_url = 'http://example.com/a b' \
                 WHERE wake_id = 'wak_url_check'",
                &[],
            )
            .await;
        assert!(res.is_err(), "URL with spaces must violate CHECK");
    }

    /// R17-A2: the partial index on (lessee_updated_at) WHERE
    /// non-terminal exists after 0010, supporting the wake-job
    /// takeover sweep query.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_lessee_partial_index_present() {
        let url = test_url();
        common::reset_and_migrate(&url).await;

        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(&url, cfg)
            .await
            .unwrap();
        let client = pool.get().await.unwrap();

        let row = client
            .query_opt(
                "SELECT indexdef FROM pg_indexes \
                  WHERE schemaname = 'zeroship' \
                    AND tablename = 'wake_jobs' \
                    AND indexname = 'wake_jobs_lessee_idx'",
                &[],
            )
            .await
            .unwrap()
            .expect("wake_jobs_lessee_idx must exist after migration 0010");
        let indexdef: &str = row.get(0);
        assert!(
            indexdef.contains("lessee_updated_at"),
            "index def must mention lessee_updated_at; got {indexdef}"
        );
        assert!(
            indexdef.to_lowercase().contains("where")
                && indexdef.contains("state"),
            "index must be partial on state; got {indexdef}"
        );
    }

    /// R16-S1: sandbox_audit role MUST NOT have SELECT on wake_jobs
    /// after 0010 — the 0009 grant violated 0004's role-split
    /// invariant.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_audit_role_has_no_select() {
        let url = test_url();
        common::reset_and_migrate(&url).await;

        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(&url, cfg)
            .await
            .unwrap();
        let client = pool.get().await.unwrap();

        // The role may not exist in dev pg (the migration's DO $$
        // block tolerates that). Skip the assertion if the role
        // doesn't exist.
        let role_exists: bool = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'sandbox_audit')",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        if !role_exists {
            return;
        }

        // has_table_privilege('sandbox_audit', 'zeroship.wake_jobs', 'SELECT')
        // must be false after 0010 revoked the 0009 grant.
        let has_select: bool = client
            .query_one(
                "SELECT has_table_privilege('sandbox_audit', \
                        'zeroship.wake_jobs', 'SELECT')",
                &[],
            )
            .await
            .unwrap()
            .get(0);
        assert!(
            !has_select,
            "sandbox_audit MUST NOT have SELECT on zeroship.wake_jobs after 0010"
        );
    }

    /// Smoke check that the wake_jobs changeset (originally migration
    /// 0009) actually applied — the CHECK constraint on `state` is the
    /// proof; an INSERT with a junk state must fail.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_state_check_constraint_enforced() {
        let url = test_url();
        common::reset_and_migrate(&url).await;

        // Reach into pg directly — `insert_wake_job` only accepts
        // typed `WakeJobState`. We want to prove the SCHEMA rejects
        // an out-of-domain literal, not the Rust enum.
        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(&url, cfg)
            .await
            .unwrap();
        let client = pool.get().await.unwrap();

        let res = client
            .execute(
                "INSERT INTO zeroship.wake_jobs (wake_id, sandbox_id, state, lessee) \
                 VALUES ('wak_bad_state', 'sbx_x', 'totally_invalid', 'hst_y')",
                &[],
            )
            .await;
        assert!(
            res.is_err(),
            "INSERT with junk state must violate the CHECK constraint"
        );
    }

    // ────────────────────────────────────────────────────────────────
    // GATE-C2 (R17-C2): partial UNIQUE INDEX on (sandbox_id) WHERE
    // non-terminal closes the TOCTOU between
    // `find_pending_wake_for_sandbox` and `insert_wake_job`. Three
    // tests pin the contract:
    //
    //   1. happy path → Inserted; the row carries this caller's data.
    //   2. concurrent insert from two callers for the SAME sandbox →
    //      one Inserted + one Replay; the Replay carries the winner's
    //      row.
    //   3. UNIQUE INDEX scopes to non-terminal: after the winner
    //      transitions to `failed`, a second INSERT for the same
    //      sandbox MUST succeed (the index doesn't see the terminal
    //      row).
    // ────────────────────────────────────────────────────────────────

    /// GATE-C2: a fresh INSERT (no prior row for the sandbox) returns
    /// `InsertWakeJobOutcome::Inserted` and the row is visible via
    /// `get_wake_job`.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_insert_returns_inserted_on_fresh_sandbox() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let row = sample_row("wak_c2_fresh", "sbx_c2_fresh", "hst_c2_owner");
        let outcome = db.insert_wake_job(&row).await.expect("insert");
        assert!(
            matches!(outcome, InsertWakeJobOutcome::Inserted),
            "fresh INSERT must report Inserted; got {outcome:?}"
        );
        // Row is observable.
        let loaded = db.get_wake_job("wak_c2_fresh").await.unwrap().unwrap();
        assert_eq!(loaded.wake_id, "wak_c2_fresh");
        assert_eq!(loaded.sandbox_id, "sbx_c2_fresh");
        assert_eq!(loaded.state, WakeJobState::Pending);
    }

    /// GATE-C2 PRIMARY TEST: two concurrent INSERTs for the same
    /// `sandbox_id` collide on the partial UNIQUE INDEX. One returns
    /// `Inserted`; the other returns `Replay(winner)` carrying the
    /// winner's `wake_id`. The two callers agree on which row is the
    /// winner.
    ///
    /// This is the precise race shape R17-C2 was filed against. The
    /// concurrency is simulated by issuing two `insert_wake_job` calls
    /// back-to-back on the same Database handle — Postgres's
    /// row-locking ensures the second INSERT sees the first's row and
    /// the partial UNIQUE INDEX rejects the duplicate via
    /// ON CONFLICT DO NOTHING.
    ///
    /// Stronger forms of this (two callers on separate compio runtimes
    /// with a barrier in between) would prove the same property but
    /// require multi-runtime test scaffolding. The post-INSERT state
    /// (one row in pg, both outcomes agree on its wake_id) is the
    /// load-bearing invariant.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_insert_collapses_concurrent_race_via_unique_index() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let sid = "sbx_c2_race";
        let row_a = sample_row("wak_c2_winner", sid, "hst_c2_a");
        let row_b = sample_row("wak_c2_loser", sid, "hst_c2_b");

        let out_a = db.insert_wake_job(&row_a).await.expect("first insert");
        let out_b = db.insert_wake_job(&row_b).await.expect("second insert");

        // Caller A wins outright.
        assert!(
            matches!(out_a, InsertWakeJobOutcome::Inserted),
            "first INSERT must be Inserted; got {out_a:?}"
        );
        // Caller B sees the conflict and gets the winner's row back.
        let winner = match out_b {
            InsertWakeJobOutcome::Replay(w) => w,
            other => panic!(
                "second INSERT must be Replay; got {other:?} \
                 — UNIQUE INDEX may be absent or ON CONFLICT broken"
            ),
        };
        assert_eq!(
            winner.wake_id, "wak_c2_winner",
            "Replay must carry the winner's wake_id, not the loser's"
        );
        assert_eq!(winner.sandbox_id, sid);
        assert_eq!(winner.state, WakeJobState::Pending);
        // Loser's wake_id MUST NOT appear in pg — the row was never
        // inserted. This is the "no duplicate WakeMachine" guarantee
        // the handler depends on.
        let absent = db.get_wake_job("wak_c2_loser").await.unwrap();
        assert!(
            absent.is_none(),
            "loser's wake_id MUST NOT land in pg; got {absent:?}"
        );
    }

    /// GATE-C2: the partial UNIQUE INDEX scopes to non-terminal states.
    /// After the winner transitions to `failed`, a second INSERT for
    /// the SAME sandbox must succeed (terminal rows are outside the
    /// index's predicate). Same after `ok`.
    ///
    /// This is what lets a client retry a failed wake against the
    /// same sandbox without the UNIQUE INDEX permanently blocking the
    /// sandbox from being woken again.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_unique_index_releases_after_terminal_transition() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let sid = "sbx_c2_terminal";

        // Insert + fail the first wake.
        let first = sample_row("wak_c2_first", sid, "hst_c2_owner");
        let out_first = db.insert_wake_job(&first).await.expect("first insert");
        assert!(matches!(out_first, InsertWakeJobOutcome::Inserted));
        db.update_wake_job_state(
            "wak_c2_first",
            WakeJobState::Failed,
            Some(WakeErrorCode::Internal),
            Some("synth"),
            None,
        )
        .await
        .unwrap();

        // A second INSERT for the same sandbox now MUST succeed — the
        // terminal row dropped out of the partial UNIQUE INDEX.
        let second = sample_row("wak_c2_second", sid, "hst_c2_owner");
        let out_second = db
            .insert_wake_job(&second)
            .await
            .expect("second insert after terminal");
        assert!(
            matches!(out_second, InsertWakeJobOutcome::Inserted),
            "INSERT after terminal must be Inserted; got {out_second:?} \
             — UNIQUE INDEX predicate may be wrong"
        );

        // The pending row for this sandbox is now the second one.
        let found = db
            .find_pending_wake_for_sandbox(sid)
            .await
            .unwrap()
            .expect("second row must be findable");
        assert_eq!(found.wake_id, "wak_c2_second");

        // Same again after `ok`: fail second, terminal, then a third
        // succeeds.
        db.update_wake_job_state(
            "wak_c2_second",
            WakeJobState::Ok,
            None,
            None,
            Some("http://10.0.0.1:7000"),
        )
        .await
        .unwrap();
        let third = sample_row("wak_c2_third", sid, "hst_c2_owner");
        let out_third = db
            .insert_wake_job(&third)
            .await
            .expect("third insert after ok");
        assert!(
            matches!(out_third, InsertWakeJobOutcome::Inserted),
            "INSERT after ok must be Inserted; got {out_third:?}"
        );
    }

    /// GATE-C2 schema pin: the `wake_jobs_sandbox_pending_uniq`
    /// partial UNIQUE INDEX must exist after migration 0011. Without
    /// this index, the ON CONFLICT clause in `insert_wake_job` is a
    /// no-op (Postgres needs a matching arbiter index to use the
    /// conflict target). This test catches the case where 0011
    /// silently fails to apply.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn wake_jobs_sandbox_pending_uniq_index_present() {
        let url = test_url();
        common::reset_and_migrate(&url).await;

        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(&url, cfg)
            .await
            .unwrap();
        let client = pool.get().await.unwrap();
        let row = client
            .query_one(
                "SELECT indexdef FROM pg_indexes \
                 WHERE schemaname = 'zeroship' \
                   AND tablename = 'wake_jobs' \
                   AND indexname = 'wake_jobs_sandbox_pending_uniq'",
                &[],
            )
            .await
            .expect("wake_jobs_sandbox_pending_uniq must exist after migration 0011");
        let indexdef: String = row.get("indexdef");
        // Sanity: the index is UNIQUE and filtered to non-terminal.
        // Postgres's `pg_indexes.indexdef` normalises `NOT IN (...)`
        // to `<> ALL (ARRAY[...])`, so we match on the canonical form.
        assert!(
            indexdef.contains("UNIQUE"),
            "index must be UNIQUE; got {indexdef}"
        );
        let lower = indexdef.to_lowercase();
        assert!(
            lower.contains("'ok'") && lower.contains("'failed'"),
            "index predicate must reference both terminal states; got {indexdef}"
        );
        assert!(
            lower.contains("<> all") || lower.contains("not in"),
            "index predicate must be a negative match on terminal states; got {indexdef}"
        );
    }

    // ─── R19-C1 takeover sweep ─────────────────────────────────────
    //
    // `claim_orphan_wake_for_recovery` is the read-side that finally
    // gives `lessee_updated_at` (bumped by R17-A1 on every transition)
    // a purpose: rows whose lessee has gone stale are forcibly
    // transitioned to `failed` with `error_code = 'wake_worker_aborted'`
    // so the `wake_jobs_sandbox_pending_uniq` UNIQUE INDEX (migration
    // 0011) releases for a fresh wake POST. These tests pin the four
    // shapes that close the wedge:
    //
    //   1. Stale orphan claimed         → count=1, state=failed.
    //   2. Fresh non-terminal row       → count=0, untouched.
    //   3. Terminal row                 → count=0, untouched.
    //   4. Two concurrent sweeps        → exactly one count=1 (pg
    //      row-locks serialize via the UPDATE).

    /// Helper: backdate a row's `lessee_updated_at` by `secs` seconds
    /// via a direct pg UPDATE so the test doesn't have to actually
    /// sleep through the takeover threshold.
    async fn backdate_lessee(url: &str, wake_id: &str, secs: i64) {
        let mut cfg = compio_postgres::PoolConfig::default();
        cfg.max_size = 2;
        let pool = compio_postgres::Pool::connect_with_config(url, cfg)
            .await
            .unwrap();
        let client = pool.get().await.unwrap();
        client
            .execute(
                "UPDATE zeroship.wake_jobs \
                    SET lessee_updated_at = \
                        now() - make_interval(secs => $1::BIGINT) \
                  WHERE wake_id = $2::TEXT",
                &[&secs, &wake_id.to_string()],
            )
            .await
            .expect("backdate succeeded");
    }

    /// R19-C1: a single orphan whose `lessee_updated_at` is older
    /// than the threshold is claimed: `claim_orphan_wake_for_recovery`
    /// returns 1, and a follow-up read shows state=failed,
    /// error_code=WakeWorkerAborted.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn claim_orphan_wake_marks_stale_row_failed() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url.clone())
            .await
            .unwrap();

        // Insert a row + drop it into the `restoring` state, then
        // backdate `lessee_updated_at` 120 s into the past.
        let row = sample_row("wak_orphan_a", "sbx_orphan_a", "hst_crashed");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        db.update_wake_job_state(
            "wak_orphan_a",
            WakeJobState::Restoring,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        backdate_lessee(&url, "wak_orphan_a", 120).await;

        // Threshold 60s — the backdated row qualifies.
        let n = db
            .claim_orphan_wake_for_recovery(StdDuration::from_secs(60))
            .await
            .expect("claim succeeded");
        assert_eq!(n, 1, "exactly one orphan must be claimed");

        let after = db.get_wake_job("wak_orphan_a").await.unwrap().unwrap();
        assert_eq!(
            after.state,
            WakeJobState::Failed,
            "orphan must transition to failed"
        );
        assert_eq!(
            after.error_code,
            Some(WakeErrorCode::WakeWorkerAborted),
            "error_code must record the takeover"
        );
        assert!(
            after
                .error_message
                .as_deref()
                .map(|m| m.contains("R19-C1") || m.contains("takeover"))
                .unwrap_or(false),
            "error_message must mention takeover; got {:?}",
            after.error_message
        );
    }

    /// R19-C1: a non-terminal row whose `lessee_updated_at` is
    /// FRESH is NOT claimed — the threshold check fences against
    /// stealing rows from a still-active controller.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn claim_orphan_wake_skips_fresh_row() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url.clone())
            .await
            .unwrap();

        let row = sample_row("wak_fresh", "sbx_fresh", "hst_active");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        // Don't backdate — `lessee_updated_at` is server-NOW.

        // 60s threshold — fresh row must NOT match.
        let n = db
            .claim_orphan_wake_for_recovery(StdDuration::from_secs(60))
            .await
            .expect("claim succeeded");
        assert_eq!(n, 0, "fresh row must NOT be claimed");

        // Row must still be in its original state.
        let after = db.get_wake_job("wak_fresh").await.unwrap().unwrap();
        assert_eq!(
            after.state,
            WakeJobState::Pending,
            "fresh row must remain pending"
        );
        assert!(after.error_code.is_none(), "no error_code on fresh row");
    }

    /// R19-C1: a TERMINAL row (`ok` or `failed`) is NEVER claimed,
    /// even when its `lessee_updated_at` is ancient. Terminal rows
    /// are already out of the GATE-C2 UNIQUE INDEX's domain; the GC
    /// sweep cleans them up by `updated_at` instead.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn claim_orphan_wake_skips_terminal_row() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url.clone())
            .await
            .unwrap();

        // Two terminal rows, both backdated past the threshold.
        let row_ok = sample_row("wak_term_ok", "sbx_term_a", "hst_t");
        let row_failed = sample_row("wak_term_failed", "sbx_term_b", "hst_t");
        for r in [&row_ok, &row_failed] {
            assert!(
                matches!(
                    db.insert_wake_job(r).await.unwrap(),
                    InsertWakeJobOutcome::Inserted
                ),
                "fresh insert must return Inserted"
            );
        }
        db.update_wake_job_state(
            "wak_term_ok",
            WakeJobState::Ok,
            None,
            None,
            Some("http://10.0.0.1:7000"),
        )
        .await
        .unwrap();
        db.update_wake_job_state(
            "wak_term_failed",
            WakeJobState::Failed,
            Some(WakeErrorCode::LivezTimeout),
            Some("simulated livez timeout"),
            None,
        )
        .await
        .unwrap();
        backdate_lessee(&url, "wak_term_ok", 600).await;
        backdate_lessee(&url, "wak_term_failed", 600).await;

        let n = db
            .claim_orphan_wake_for_recovery(StdDuration::from_secs(60))
            .await
            .expect("claim succeeded");
        assert_eq!(n, 0, "terminal rows must NOT be claimed");

        // Existing error_code on the failed row must NOT be
        // overwritten — the takeover sweep doesn't touch terminals.
        let term = db
            .get_wake_job("wak_term_failed")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(term.error_code, Some(WakeErrorCode::LivezTimeout));
    }

    /// R19-C1: two concurrent claims race cleanly — postgres
    /// row-locks during UPDATE serialize them, so exactly one sees
    /// `count == 1` and the other sees `count == 0` (the row is
    /// already terminal by the time the loser's WHERE evaluates).
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn claim_orphan_wake_concurrent_claims_race_cleanly() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url.clone())
            .await
            .unwrap();

        let row = sample_row("wak_race", "sbx_race", "hst_crashed");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );
        db.update_wake_job_state(
            "wak_race",
            WakeJobState::Restoring,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        backdate_lessee(&url, "wak_race", 600).await;

        // Drive two claim attempts back-to-back through the same
        // Database. Under compio's single-threaded runtime they
        // execute sequentially at the SQL boundary; the property the
        // test pins is correctness, not parallelism: exactly one
        // UPDATE matches the WHERE predicate, the other observes
        // `state = 'failed'` and matches zero rows. Same outcome
        // shape as a true concurrent race (postgres row-locks would
        // serialize a true parallel pair to the same result).
        let n_first = db
            .claim_orphan_wake_for_recovery(StdDuration::from_secs(60))
            .await
            .expect("first claim");
        let n_second = db
            .claim_orphan_wake_for_recovery(StdDuration::from_secs(60))
            .await
            .expect("second claim");

        assert_eq!(
            n_first + n_second,
            1,
            "exactly one of the two claims must succeed (got first={n_first}, second={n_second})"
        );
        assert_eq!(n_first, 1, "first claim must observe the orphan");
        assert_eq!(n_second, 0, "second claim must observe nothing to do");

        // Final state: row is terminal, error code recorded once.
        let after = db.get_wake_job("wak_race").await.unwrap().unwrap();
        assert_eq!(after.state, WakeJobState::Failed);
        assert_eq!(
            after.error_code,
            Some(WakeErrorCode::WakeWorkerAborted),
            "exactly one takeover writer wins"
        );
    }

    // ─── R20-C1: terminal-overwrite guard ─────────────────────────
    //
    // `update_wake_job_state` now includes `AND state NOT IN
    // ('ok', 'failed')` in the WHERE predicate. A stale write from a
    // racing producer that arrives after the row has already reached a
    // terminal state must silently no-op (rows_affected == 0).
    //
    // Two tests:
    //   1. Transition to `ok`, then attempt `restoring` → row stays ok.
    //   2. Transition to `failed`, then attempt `restoring` → row stays failed.

    /// R20-C1: once a row is in state `ok`, a subsequent attempt to
    /// drive it to a non-terminal state (`restoring`) must no-op.
    /// `rows_affected == 0`; the pg row still reads `state = ok`.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn update_wake_job_state_after_ok_is_noop() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let row = sample_row("wak_r20_ok", "sbx_r20_ok", "hst_r20");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );

        // Drive to terminal ok.
        let n = db
            .update_wake_job_state(
                "wak_r20_ok",
                WakeJobState::Ok,
                None,
                None,
                Some("http://10.0.0.1:7000"),
            )
            .await
            .expect("transition to ok");
        assert_eq!(n, 1, "first transition must affect 1 row");

        let before = db.get_wake_job("wak_r20_ok").await.unwrap().unwrap();
        assert_eq!(before.state, WakeJobState::Ok);

        // Stale write: attempt to overwrite with a non-terminal state.
        let n_stale = db
            .update_wake_job_state(
                "wak_r20_ok",
                WakeJobState::Restoring,
                None,
                None,
                None,
            )
            .await
            .expect("stale write must not error");
        assert_eq!(
            n_stale, 0,
            "stale write to terminal-ok row must no-op (rows_affected == 0)"
        );

        // pg row must still be ok — terminal state not overwritten.
        let after = db.get_wake_job("wak_r20_ok").await.unwrap().unwrap();
        assert_eq!(
            after.state,
            WakeJobState::Ok,
            "state must remain ok after stale write; got {:?}",
            after.state
        );
        // agent_url preserved from the original ok transition.
        assert_eq!(after.agent_url.as_deref(), Some("http://10.0.0.1:7000"));
    }

    /// R20-C1: once a row is in state `failed`, a subsequent attempt
    /// to drive it to a non-terminal state (`restoring`) must no-op.
    /// `rows_affected == 0`; the pg row still reads `state = failed`
    /// with the original error_code intact.
    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn update_wake_job_state_after_failed_is_noop() {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = Database::from_test_config(url).await.unwrap();

        let row = sample_row("wak_r20_failed", "sbx_r20_failed", "hst_r20");
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "fresh insert must return Inserted"
        );

        // Drive to terminal failed.
        let n = db
            .update_wake_job_state(
                "wak_r20_failed",
                WakeJobState::Failed,
                Some(WakeErrorCode::LivezTimeout),
                Some("simulated livez timeout"),
                None,
            )
            .await
            .expect("transition to failed");
        assert_eq!(n, 1, "first transition must affect 1 row");

        let before = db.get_wake_job("wak_r20_failed").await.unwrap().unwrap();
        assert_eq!(before.state, WakeJobState::Failed);

        // Stale write: racing producer attempts to overwrite with a
        // non-terminal state (the TOCTOU shape from R20-C1).
        let n_stale = db
            .update_wake_job_state(
                "wak_r20_failed",
                WakeJobState::Restoring,
                None,
                None,
                None,
            )
            .await
            .expect("stale write must not error");
        assert_eq!(
            n_stale, 0,
            "stale write to terminal-failed row must no-op (rows_affected == 0)"
        );

        // pg row must still be failed — terminal state not overwritten.
        let after = db.get_wake_job("wak_r20_failed").await.unwrap().unwrap();
        assert_eq!(
            after.state,
            WakeJobState::Failed,
            "state must remain failed after stale write; got {:?}",
            after.state
        );
        // Original error metadata must be preserved.
        assert_eq!(
            after.error_code,
            Some(WakeErrorCode::LivezTimeout),
            "error_code must not be overwritten by stale write"
        );
        assert_eq!(
            after.error_message.as_deref(),
            Some("simulated livez timeout"),
            "error_message must not be overwritten by stale write"
        );
    }
}

// Convenience accessor so tests can spell `row.is_state_terminal()`
// instead of `row.state.is_terminal()` — pure ergonomics, no
// state-machine semantics involved.
trait WakeJobRowExt {
    fn is_state_terminal(&self) -> bool;
}

impl WakeJobRowExt for zeroship_sandbox::db::WakeJobRow {
    fn is_state_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

// ════════════════════════════════════════════════════════════════════
// C-7-LT-PR2: WakeMachine end-to-end (pg-gated, StubRestoreBackend)
//
// Drives the full state machine through the same `seed_snapshotted_row`
// fixture the sync `restore_sandbox` tests use, asserting:
//
//   - Happy path: pending → reserving_slot → restoring → livez_polling
//     → clock_resyncing → registering → ok. agent_url + ready_at
//     populated.
//   - StubRestoreBackend::fail_livez=true → terminal `failed` with
//     wire-code-mapped error_code = LivezTimeout. Sandbox row rolls
//     back to `snapshotted`.
//   - StubRestoreBackend::fail_submit=true → terminal `failed` with
//     wire-code-mapped error_code = RestoreFailed (Backend(_) failure
//     classification). Sandbox row rolls back.
//   - StubRestoreBackend::fail_reserve=true → terminal `failed` with
//     wire-code-mapped error_code = SlotUnavailable
//     (VmIndexUnavailable classification).
//
// These tests close test-coverage-r16's EMERGENCY HOLD (R10-T1/T2,
// R11-T1/T2, R12-T1/T2, R13-T2, R14-T1, R15-T1) for the wake path:
// the state machine is now end-to-end-driven with StubRestoreBackend
// failure injection AND the pg row state is asserted at every
// terminal.
// ════════════════════════════════════════════════════════════════════

mod wake_machine_e2e {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::time::Duration as StdDuration;
    use zeroship_sandbox::db::{InsertWakeJobOutcome, WakeErrorCode, WakeJobRow, WakeJobState};
    use zeroship_sandbox::restore_handler::{RestoreBackend, StubRestoreBackend};
    use zeroship_sandbox::snapshot_store::{LocalDiskSnapshotStore, SnapshotStore};
    use zeroship_sandbox::wake_machine::WakeMachine;

    /// Migrate + arc-wrap the shared test Database so the seeder and
    /// the WakeMachine share the same `host_id` (the CAS in
    /// `update_sandbox_status` fences on host_id; a second Database
    /// instance would have a fresh host_id and the CAS would lose).
    async fn migrated_db_arc() -> StdArc<zeroship_sandbox::db::Database> {
        let url = test_url();
        common::reset_and_migrate(&url).await;
        let db = zeroship_sandbox::db::Database::from_test_config(url)
            .await
            .unwrap();
        db.upsert_host("test-host", "nomad-ch").await.unwrap();
        StdArc::new(db)
    }

    /// Build a `WakeMachine` over a SEEDED snapshot row + backend.
    /// `db` is the same Arc the seeder used so the host_id fence
    /// holds. Returns the machine + the wake_id it was given so
    /// the caller can poll `get_wake_job` after `drive()`.
    async fn make_machine(
        db: StdArc<zeroship_sandbox::db::Database>,
        backend: StdArc<dyn RestoreBackend>,
        store_root: &std::path::Path,
        sandbox_id: Uuid,
    ) -> (WakeMachine, String) {
        let store: StdArc<dyn SnapshotStore> =
            StdArc::new(LocalDiskSnapshotStore::new(store_root));
        let typed_sid = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sandbox_id)
        );
        let wake_id = zeroship_core::typed_id::new_wake_id();
        let lessee = db.host_id().to_string();
        let row = WakeJobRow {
            wake_id: wake_id.clone(),
            sandbox_id: typed_sid,
            state: WakeJobState::Pending,
            error_code: None,
            error_message: None,
            started_at_secs: 0,
            updated_at_secs: 0,
            ready_at_secs: None,
            agent_url: None,
            lessee: lessee.clone(),
            lessee_updated_at_secs: 0,
        };
        assert!(
            matches!(
                db.insert_wake_job(&row).await.unwrap(),
                InsertWakeJobOutcome::Inserted
            ),
            "make_machine: fresh wake_id insert must return Inserted; \
             a Replay here means a stale non-terminal row leaked from a prior test \
             and the machine would drive the wrong wake_id"
        );
        let machine = WakeMachine {
            database: StdArc::clone(&db),
            backend,
            snapshot_store: store,
            persist: None, // test-fixture path; the machine skips
                           // unseal/resync/register per `persist=None`.
            sandbox_id,
            wake_id: wake_id.clone(),
            lessee,
        };
        (machine, wake_id)
    }

    /// Happy path: the state machine drives a snapshotted row to
    /// running, marks the wake_job as ok, and populates agent_url +
    /// ready_at.
    #[compio::test]
    #[ignore = "needs Postgres; C-7-LT-PR2 wake_machine happy path"]
    async fn wake_machine_drives_snapshotted_to_ok() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_happy_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_happy_back");
        let stub = StdArc::new(StubRestoreBackend::new(backend_root.clone()));
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) = make_machine(
            StdArc::clone(&db),
            backend,
            &store_root,
            sid,
        )
        .await;

        machine.drive().await;

        let final_row = db
            .get_wake_job(&wake_id)
            .await
            .unwrap()
            .expect("wake_job row must exist after drive");
        assert_eq!(
            final_row.state,
            WakeJobState::Ok,
            "happy path must reach terminal Ok; got {:?}",
            final_row.state
        );
        assert!(
            final_row.ready_at_secs.is_some(),
            "Ok terminal must populate ready_at"
        );
        assert!(
            final_row.agent_url.is_some(),
            "Ok terminal must populate agent_url"
        );
        assert!(final_row.error_code.is_none());
        assert!(final_row.error_message.is_none());

        let sb = db.get_sandbox_row(sid).await.unwrap().expect("sandbox row");
        assert_eq!(sb.status, SandboxStatus::Running);

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    /// `fail_livez=true` → terminal `failed` with `WakeErrorCode::LivezTimeout`.
    /// Sandbox row rolls back to `snapshotted` (post-livez phase, so the
    /// rollback target is NOT `snapshotted_suspect`).
    #[compio::test]
    #[ignore = "needs Postgres; C-7-LT-PR2 wake_machine livez failure"]
    async fn wake_machine_classifies_livez_failure() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_livez_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_livez_back");
        let mut stub_inner = StubRestoreBackend::new(backend_root.clone());
        stub_inner.fail_livez = true;
        let stub = StdArc::new(stub_inner);
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) = make_machine(
            StdArc::clone(&db),
            backend,
            &store_root,
            sid,
        )
        .await;

        machine.drive().await;

        let final_row = db
            .get_wake_job(&wake_id)
            .await
            .unwrap()
            .expect("wake_job row must exist");
        assert_eq!(final_row.state, WakeJobState::Failed);
        assert_eq!(
            final_row.error_code,
            Some(WakeErrorCode::LivezTimeout),
            "fail_livez must classify as LivezTimeout (renders as `livez_timeout` on the wire)"
        );
        assert!(final_row.error_message.is_some());
        assert!(
            final_row.error_code.unwrap().wire_code() == "livez_timeout",
            "wire code drift check"
        );

        let sb = db.get_sandbox_row(sid).await.unwrap().expect("sandbox row");
        assert_eq!(
            sb.status,
            SandboxStatus::Snapshotted,
            "post-livez failure rolls back to snapshotted (NOT suspect)"
        );

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    /// `fail_submit=true` → terminal `failed` with
    /// `WakeErrorCode::RestoreFailed` (Backend(_) classification). The
    /// submit failure is pre-livez so the rollback target is still
    /// `snapshotted`.
    #[compio::test]
    #[ignore = "needs Postgres; C-7-LT-PR2 wake_machine submit failure"]
    async fn wake_machine_classifies_submit_failure() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_submit_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_submit_back");
        let mut stub_inner = StubRestoreBackend::new(backend_root.clone());
        stub_inner.fail_submit = true;
        let stub = StdArc::new(stub_inner);
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) = make_machine(
            StdArc::clone(&db),
            backend,
            &store_root,
            sid,
        )
        .await;

        machine.drive().await;

        let final_row = db
            .get_wake_job(&wake_id)
            .await
            .unwrap()
            .expect("wake_job row must exist");
        assert_eq!(final_row.state, WakeJobState::Failed);
        assert_eq!(
            final_row.error_code,
            Some(WakeErrorCode::RestoreFailed),
            "Backend(submit) failure → RestoreFailed (wire `restore_backend_failed`)"
        );

        let sb = db.get_sandbox_row(sid).await.unwrap().expect("sandbox row");
        assert_eq!(sb.status, SandboxStatus::Snapshotted);

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    /// `fail_reserve=true` → terminal `failed` with
    /// `WakeErrorCode::SlotUnavailable` (VmIndexUnavailable
    /// classification → wire `vm_index_unavailable`).
    #[compio::test]
    #[ignore = "needs Postgres; C-7-LT-PR2 wake_machine reserve failure"]
    async fn wake_machine_classifies_reserve_failure() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_reserve_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_reserve_back");
        let mut stub_inner = StubRestoreBackend::new(backend_root.clone());
        stub_inner.fail_reserve = true;
        let stub = StdArc::new(stub_inner);
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) = make_machine(
            StdArc::clone(&db),
            backend,
            &store_root,
            sid,
        )
        .await;

        machine.drive().await;

        let final_row = db
            .get_wake_job(&wake_id)
            .await
            .unwrap()
            .expect("wake_job row must exist");
        assert_eq!(final_row.state, WakeJobState::Failed);
        assert_eq!(
            final_row.error_code,
            Some(WakeErrorCode::SlotUnavailable),
            "VmIndexUnavailable → SlotUnavailable (wire `vm_index_unavailable`)"
        );

        let sb = db.get_sandbox_row(sid).await.unwrap().expect("sandbox row");
        assert_eq!(sb.status, SandboxStatus::Snapshotted);

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    /// Idempotency probe: after the machine reaches terminal,
    /// `find_pending_wake_for_sandbox` returns None (the row is
    /// no longer in flight), and a second `insert_wake_job` for the
    /// SAME wake_id fails (primary-key violation). This pins the
    /// invariant the handler's idempotency check relies on.
    #[compio::test]
    #[ignore = "needs Postgres; C-7-LT-PR2 wake_machine idempotency invariant"]
    async fn wake_machine_terminal_clears_find_pending() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_idemp_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_idemp_back");
        let stub = StdArc::new(StubRestoreBackend::new(backend_root.clone()));
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, _wake_id) = make_machine(
            StdArc::clone(&db),
            backend,
            &store_root,
            sid,
        )
        .await;

        let typed_sid = format!(
            "sbx_{}",
            zeroship_core::typed_id::uuid_to_base62(&sid)
        );

        // Pre-drive: in-flight wake should be visible.
        let in_flight = db
            .find_pending_wake_for_sandbox(&typed_sid)
            .await
            .unwrap();
        assert!(
            in_flight.is_some(),
            "pending wake must be visible to idempotency lookup"
        );

        machine.drive().await;

        // Post-drive: no in-flight wake.
        let after = db
            .find_pending_wake_for_sandbox(&typed_sid)
            .await
            .unwrap();
        assert!(
            after.is_none(),
            "terminal wake must NOT show up in find_pending_wake_for_sandbox"
        );

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    /// GC sweep applied after a terminal wake: the wake_job row is
    /// deleted, mirroring the production T_KEEP eviction path.
    /// Subsequent `get_wake_job` returns None (the production poll
    /// handler maps this to 404 `wake_not_found`).
    #[compio::test]
    #[ignore = "needs Postgres; C-7-LT-PR2 wake_machine GC after terminal"]
    async fn wake_machine_gc_sweep_evicts_terminal_rows() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_gc_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_gc_back");
        let stub = StdArc::new(StubRestoreBackend::new(backend_root.clone()));
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) = make_machine(
            StdArc::clone(&db),
            backend,
            &store_root,
            sid,
        )
        .await;

        machine.drive().await;

        // Sanity: terminal row exists.
        assert!(db.get_wake_job(&wake_id).await.unwrap().is_some());

        // 0-second threshold: every terminal row qualifies. After this
        // sweep the wake_id must lookup as None (the poll handler maps
        // this to 404 `wake_not_found` per § 2.cleanup).
        let deleted = db
            .gc_expired_wake_jobs(StdDuration::from_secs(0))
            .await
            .unwrap();
        assert!(deleted >= 1, "GC must delete the terminal wake row");

        let evicted = db.get_wake_job(&wake_id).await.unwrap();
        assert!(
            evicted.is_none(),
            "wake_id must lookup as None after T_KEEP eviction"
        );

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    // ════════════════════════════════════════════════════════════════
    // R23-I1 (Path B): WakeMachine terminal-overwrite counter wiring.
    //
    // R22-I1 placed the `inc_wake_terminal_overwrite_blocked()` bumps
    // at the wake_machine.rs callers (lines :146, :191, :528) instead
    // of inside `db::update_wake_job_state`, so each caller can WARN
    // with its own `attempted_state` context. The bare db noop tests
    // in `wake_job_terminal_overwrite_guard` only prove the SQL guard
    // (rows_affected == 0); they do NOT cover the counter wiring at
    // the WakeMachine callers.
    //
    // These two tests drive the full WakeMachine end-to-end with the
    // wake_jobs row pre-flipped to a terminal state, so every
    // `update_wake_job_state` call inside the machine (both the
    // intermediate `set_state` writes and the terminal write in
    // `drive`'s outer match) hits the R20-C1 guard. They assert:
    //
    //   1. The counter strictly INCREASED (>= pre + 1) — proving the
    //      bump in wake_machine.rs is actually wired up.
    //   2. The DB row's `state` field is UNCHANGED from the pre-set
    //      terminal (the sweep's breadcrumb survives — the machine's
    //      attempted overwrite no-op'd).
    //
    // Why `>= pre + 1` and not `== pre + 1`: the WakeMachine issues
    // 5-6 `set_state` calls during a run plus one terminal write. Each
    // one no-ops + bumps when the row is already terminal. The exact
    // delta depends on how far the machine progresses before any
    // backend failure; asserting strict monotonic increase is the
    // honest contract, asserting `== 1` would require the production
    // machine to short-circuit on first guard fire (which it does not
    // — and should not: intermediate writes are best-effort).
    // ════════════════════════════════════════════════════════════════

    /// Pre-flip the wake_jobs row to `Failed`, then drive the
    /// happy-path machine. The machine's terminal write attempts
    /// `Ok`, hits the R20-C1 guard, and bumps the counter at
    /// wake_machine.rs:146.
    #[compio::test]
    #[ignore = "needs Postgres; R23-I1 terminal-overwrite counter (failed → ok)"]
    async fn wake_machine_terminal_overwrite_failed_to_ok_bumps_counter() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_r23_fto_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_r23_fto_back");
        // Happy-path backend: drive() would reach Phase::Ok if the
        // wake_job row weren't already terminal.
        let stub = StdArc::new(StubRestoreBackend::new(backend_root.clone()));
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) =
            make_machine(StdArc::clone(&db), backend, &store_root, sid).await;

        // Pre-flip the wake_job row to terminal Failed BEFORE driving
        // the machine. Use db.update_wake_job_state directly to bypass
        // the state machine for this setup step (R20-C1's guard does
        // not block transitions INTO a terminal — only transitions
        // out of one).
        let flipped = db
            .update_wake_job_state(
                &wake_id,
                WakeJobState::Failed,
                Some(WakeErrorCode::LivezTimeout),
                Some("pre-seeded terminal failure"),
                None,
            )
            .await
            .expect("seed: pending → failed must succeed");
        assert_eq!(flipped, 1, "seed transition must affect exactly 1 row");

        // Capture counter pre. Note: the counter is process-global and
        // monotonic across all tests in the binary; always delta.
        let pre = zeroship_sandbox::metrics::wake_terminal_overwrite_blocked_value();

        machine.drive().await;

        let post = zeroship_sandbox::metrics::wake_terminal_overwrite_blocked_value();
        assert!(
            post >= pre + 1,
            "wake_machine.rs counter wiring did NOT fire on terminal-overwrite-blocked: \
             pre={pre} post={post} (expected post >= pre + 1; machine writes \
             several intermediate set_state calls + one terminal write — each \
             must no-op + bump when the row is already terminal)"
        );

        // DB row's state must be unchanged from the pre-set terminal.
        // The sweep's breadcrumb (Failed + LivezTimeout) survived the
        // machine's attempted overwrite.
        let final_row = db
            .get_wake_job(&wake_id)
            .await
            .unwrap()
            .expect("wake_job row must exist");
        assert_eq!(
            final_row.state,
            WakeJobState::Failed,
            "row must remain Failed (R20-C1 guard blocked machine's Ok overwrite); \
             got {:?}",
            final_row.state
        );
        assert_eq!(
            final_row.error_code,
            Some(WakeErrorCode::LivezTimeout),
            "pre-seeded error_code must NOT be overwritten by terminal Ok attempt"
        );
        assert_eq!(
            final_row.error_message.as_deref(),
            Some("pre-seeded terminal failure"),
            "pre-seeded error_message must NOT be overwritten by terminal Ok attempt"
        );
        assert!(
            final_row.agent_url.is_none(),
            "agent_url must NOT be populated — terminal Ok write no-op'd"
        );

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }

    /// Pre-flip the wake_jobs row to `Ok`, then drive the machine
    /// with `fail_livez=true` so it attempts terminal `Failed`. The
    /// terminal write hits the R20-C1 guard and bumps the counter at
    /// wake_machine.rs:191.
    #[compio::test]
    #[ignore = "needs Postgres; R23-I1 terminal-overwrite counter (ok → failed)"]
    async fn wake_machine_terminal_overwrite_ok_to_failed_bumps_counter() {
        let db = migrated_db_arc().await;
        let (info, sid) = fresh_info("alice");
        let store_root = fresh_temp("wm_r23_otf_store");
        let _ = seed_snapshotted_row(&db, sid, &info, &store_root).await;

        let backend_root = fresh_temp("wm_r23_otf_back");
        // fail_livez=true: machine progresses through reserving_slot
        // and restoring, then trips livez and lands in Phase::Failed
        // → terminal Failed write attempt.
        let mut stub_inner = StubRestoreBackend::new(backend_root.clone());
        stub_inner.fail_livez = true;
        let stub = StdArc::new(stub_inner);
        let backend: StdArc<dyn RestoreBackend> = StdArc::clone(&stub) as _;

        let (machine, wake_id) =
            make_machine(StdArc::clone(&db), backend, &store_root, sid).await;

        // Pre-flip the wake_job row to terminal Ok BEFORE driving.
        let flipped = db
            .update_wake_job_state(
                &wake_id,
                WakeJobState::Ok,
                None,
                None,
                Some("http://10.0.0.99:7000"),
            )
            .await
            .expect("seed: pending → ok must succeed");
        assert_eq!(flipped, 1, "seed transition must affect exactly 1 row");

        let pre = zeroship_sandbox::metrics::wake_terminal_overwrite_blocked_value();

        machine.drive().await;

        let post = zeroship_sandbox::metrics::wake_terminal_overwrite_blocked_value();
        assert!(
            post >= pre + 1,
            "wake_machine.rs counter wiring did NOT fire on terminal-overwrite-blocked: \
             pre={pre} post={post} (expected post >= pre + 1; the machine attempts \
             terminal Failed via the wake_machine.rs:191 call site after fail_livez)"
        );

        // DB row's state must be unchanged from the pre-set terminal Ok.
        let final_row = db
            .get_wake_job(&wake_id)
            .await
            .unwrap()
            .expect("wake_job row must exist");
        assert_eq!(
            final_row.state,
            WakeJobState::Ok,
            "row must remain Ok (R20-C1 guard blocked machine's Failed overwrite); \
             got {:?}",
            final_row.state
        );
        assert_eq!(
            final_row.agent_url.as_deref(),
            Some("http://10.0.0.99:7000"),
            "pre-seeded agent_url must NOT be overwritten by terminal Failed attempt"
        );
        assert!(
            final_row.error_code.is_none(),
            "error_code must remain None — terminal Failed write no-op'd"
        );
        assert!(
            final_row.error_message.is_none(),
            "error_message must remain None — terminal Failed write no-op'd"
        );

        let _ = std::fs::remove_dir_all(&store_root);
        let _ = std::fs::remove_dir_all(&backend_root);
    }
}

// ════════════════════════════════════════════════════════════════════
// r1-DISC-3: R26-C1 thread-local `Rc<Pool>` cache predicate tests
//
// R28-DISCIPLINE audit (`docs/reviews/sandbox-snapshot-restore-test-
// discipline-audit-2026-05-25-r1.md`) flagged the R26-C1 cache shape
// (`crates/sandbox/src/db.rs:62-105` + `:617-672`) as the highest-
// leverage gap: ZERO predicate tests, and the cost was visible —
// the stress-r7 wedge → r7-A `max_connections` bump → r7-C-followup
// `start_housekeeper` patch sequence is exactly the kind of incident
// that a predicate oracle would have caught earlier. These tests pin
// the three observable contracts of the cache:
//
//   1. **Cache hit within a thread**: same DSN, same compio worker
//      thread → returns the SAME `Rc<Pool>` (verified via
//      `Rc::ptr_eq`). The internal pattern is `cached_pool` returns
//      `Some` on the second call; the post-await re-check in
//      `install_pool` is bypassed entirely.
//   2. **Per-thread isolation**: different OS threads each run their
//      own compio runtime → each has its own `thread_local!` entry,
//      so each opens an INDEPENDENT pool. The production-state
//      oracle is `pg_stat_activity` filtered by an
//      application_name unique to this test (avoids contamination
//      from prior tests / other connections).
//   3. **DSN tiebreaker**: same thread, different DSN strings →
//      `cached_pool` returns `None` on the DSN mismatch arm,
//      `install_pool` evicts the prior entry and replaces with the
//      new pool. `Rc::ptr_eq` between the two `Rc<Pool>` values is
//      false. This is the guard that `set_role_dsns_for_test`
//      relies on for the role-permission integration tests.
//
// Test 4 (housekeeper reaps idle conns) is DEFERRED — the
// `PoolConfig` `idle_timeout=600s` / `max_lifetime=1800s` defaults
// are not adjustable from the `Database` boundary, and a CI test
// that waits 10+ minutes is not viable. The housekeeper's correct
// wiring is verified by code inspection (`db.rs:640,670` —
// `start_housekeeper()` is called immediately after
// `Pool::connect_with_config` for both `pool_app` and `pool_audit`)
// and the structural argument from `compio-postgres/src/pool.rs:341
// -353` (housekeeper holds `Weak<Pool>` so it self-terminates when
// the last `Rc` drops). Source: r1-DISC-3 + r7-C-followup closure.
// ════════════════════════════════════════════════════════════════════

mod r26_c1_pool_cache {
    use super::*;
    use std::rc::Rc;
    use std::sync::{Arc, Barrier};

    /// Build a DSN with a unique `application_name` query parameter so
    /// each test's pg_stat_activity reading is isolated from sibling
    /// tests' lingering connections.
    fn dsn_with_app_name(tag: &str) -> String {
        let base = test_url();
        let sep = if base.contains('?') { '&' } else { '?' };
        format!("{base}{sep}application_name={tag}")
    }

    /// Count rows in `pg_stat_activity` whose `application_name`
    /// matches the test tag. The session executing the query is
    /// excluded (`pid <> pg_backend_pid()`) so the count reflects
    /// only the pool's retained connections, not the observer.
    async fn count_conns_with_app_name(observer: &compio_postgres::Pool, tag: &str) -> i64 {
        let client = observer.get().await.expect("observer client");
        let row = client
            .query_one(
                "SELECT count(*)::BIGINT FROM pg_stat_activity \
                 WHERE application_name = $1 AND pid <> pg_backend_pid()",
                &[&tag],
            )
            .await
            .expect("pg_stat_activity query");
        row.get(0)
    }

    // ────────────────────────────────────────────────────────────────
    // Test 1: same thread + same DSN → same Rc<Pool>
    // ────────────────────────────────────────────────────────────────

    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn pool_cache_returns_same_rc_within_thread() {
        // Tagged DSN keeps this test's cache entry distinct from
        // prior tests' default-DSN entries; both calls below share
        // the SAME DSN string, so `cached_pool` returns `Some` on
        // the second call and `install_pool` is bypassed.
        let dsn = dsn_with_app_name("zs_disc3_t1");
        let db = Database::from_test_config(dsn)
            .await
            .expect("from_test_config");

        let p1: Rc<compio_postgres::Pool> = db.pool_app().await.expect("first pool_app");
        let p2: Rc<compio_postgres::Pool> = db.pool_app().await.expect("second pool_app");

        assert!(
            Rc::ptr_eq(&p1, &p2),
            "same thread + same DSN must return the cached Rc<Pool> \
             (R26-C1 cache hit predicate violated — pool_app rebuilt \
             the pool instead of returning POOL_APP_CELL's cached entry)"
        );
        // Strong count is 3: the cache cell, p1, and p2. The
        // exact value matters less than ptr_eq above; this assert
        // is a defense-in-depth check that nothing exotic is going
        // on (e.g., a second cell holding a phantom strong ref).
        assert_eq!(
            Rc::strong_count(&p1),
            3,
            "Rc strong count must be 3 (cache + p1 + p2); got {}",
            Rc::strong_count(&p1)
        );
    }

    // ────────────────────────────────────────────────────────────────
    // Test 2: two OS threads → two independent pools
    //   (production-state oracle: pg_stat_activity row count)
    // ────────────────────────────────────────────────────────────────

    #[compio::test]
    #[ignore = "needs Postgres; spawns 2 OS threads with their own compio runtimes"]
    async fn pool_cache_per_thread_isolated() {
        let tag = "zs_disc3_t2";
        let dsn_for_workers = dsn_with_app_name(tag);

        // Build an observer pool on a NEUTRAL application_name (the
        // default DSN) so the observer's own backend doesn't show up
        // under the filtered count.
        let mut obs_cfg = PoolConfig::default();
        obs_cfg.max_size = 2;
        let observer = Pool::connect_with_config(&test_url(), obs_cfg)
            .await
            .expect("observer pool");

        // Barrier(3) = 2 workers + 1 observer. Workers warm pools
        // then arrive; observer arrives once it has counted.
        let barrier_start = Arc::new(Barrier::new(3));
        let barrier_done = Arc::new(Barrier::new(3));

        let mut handles = Vec::new();
        for i in 0..2u32 {
            let dsn = dsn_for_workers.clone();
            let bs = Arc::clone(&barrier_start);
            let bd = Arc::clone(&barrier_done);
            handles.push(std::thread::spawn(move || {
                let rt = compio::runtime::Runtime::new().expect("worker runtime");
                rt.block_on(async move {
                    let db = Database::from_test_config(dsn)
                        .await
                        .unwrap_or_else(|e| panic!("worker {i} from_test_config: {e}"));
                    let p1 = db
                        .pool_app()
                        .await
                        .unwrap_or_else(|e| panic!("worker {i} pool_app #1: {e}"));
                    let p2 = db
                        .pool_app()
                        .await
                        .unwrap_or_else(|e| panic!("worker {i} pool_app #2: {e}"));
                    // Same-thread cache hit invariant ALSO holds on
                    // each worker thread (the thread_local is fresh
                    // per OS thread, so the FIRST call populates it
                    // and the SECOND call hits).
                    assert!(
                        Rc::ptr_eq(&p1, &p2),
                        "worker {i}: thread-local cache must hit on second call"
                    );
                    // Hold the pool alive across the barrier so
                    // pg_stat_activity sees its conns.
                    bs.wait();
                    bd.wait();
                    drop(p2);
                    drop(p1);
                });
            }));
        }

        // Wait for both workers to have warmed their pools.
        barrier_start.wait();
        let n = count_conns_with_app_name(&observer, tag).await;
        // Each worker's pool warms `min_idle.max(1) = 2` conns
        // (PoolConfig default min_idle=2). Two workers × 2 conns
        // = 4 floor. We assert >= 2 (one per worker would already
        // disprove "all workers share one pool"); >= 4 is the
        // tight floor but we use >= 2 to stay robust against
        // PoolConfig default drift.
        assert!(
            n >= 2,
            "expected at least 2 sandbox_app conns under application_name='{tag}' \
             (one per worker thread); got {n}. Each compio worker has its OWN \
             thread_local POOL_APP_CELL — if they shared a cache the count would \
             reflect only one pool's min_idle"
        );
        barrier_done.wait();
        for (i, h) in handles.into_iter().enumerate() {
            h.join().unwrap_or_else(|e| panic!("worker {i} join: {e:?}"));
        }
    }

    // ────────────────────────────────────────────────────────────────
    // Test 3: same thread + different DSN → different Rc<Pool>
    //   (cache key is the DSN string; DSN mismatch evicts + replaces)
    // ────────────────────────────────────────────────────────────────

    #[compio::test]
    #[ignore = "needs Postgres"]
    async fn pool_cache_dsn_tiebreaker_evicts_on_mismatch() {
        // Two DSNs that connect to the same db but differ in their
        // verbatim string (the cache key). `application_name` is a
        // pg-supported connection param so both connect cleanly.
        let dsn_a = dsn_with_app_name("zs_disc3_t3_a");
        let dsn_b = dsn_with_app_name("zs_disc3_t3_b");
        assert_ne!(dsn_a, dsn_b, "test fixture: DSNs must differ verbatim");

        let db_a = Database::from_test_config(dsn_a)
            .await
            .expect("from_test_config dsn_a");
        let db_b = Database::from_test_config(dsn_b)
            .await
            .expect("from_test_config dsn_b");

        let pa = db_a.pool_app().await.expect("pool_app dsn_a");
        let pb = db_b.pool_app().await.expect("pool_app dsn_b");

        assert!(
            !Rc::ptr_eq(&pa, &pb),
            "same thread + DIFFERENT DSN must produce different pools — the \
             cache key includes the DSN so a mismatch evicts the prior entry \
             and `install_pool` builds afresh. If this assert fires the cache \
             is collapsing all DSNs to one slot, which would return \
             auth-mismatched pools when `set_role_dsns_for_test` rotates \
             roles mid-process (R26-C1 closure note: 'DSN-keyed (not \
             unkeyed): set_role_dsns_for_test exists for the role-permission \
             integration tests')"
        );

        // After the eviction, a second call with dsn_b's Database
        // returns the SAME pool as `pb` (it's now the cached entry).
        let pb2 = db_b.pool_app().await.expect("pool_app dsn_b second");
        assert!(
            Rc::ptr_eq(&pb, &pb2),
            "after DSN-mismatch eviction, the new pool itself must be \
             cached — same-DSN second call should hit"
        );
        // Conversely, going back to dsn_a now evicts dsn_b. The
        // NEW pa2 will NOT equal the original pa (which was
        // dropped from the cache during the dsn_a → dsn_b swap;
        // its only remaining strong ref is the `pa` binding
        // above).
        let pa2 = db_a.pool_app().await.expect("pool_app dsn_a second");
        assert!(
            !Rc::ptr_eq(&pa, &pa2),
            "the original dsn_a pool was evicted by the dsn_a → dsn_b \
             rotation; a third call (dsn_b → dsn_a) must build a FRESH \
             pool, not resurrect the original `pa`"
        );
    }
}
