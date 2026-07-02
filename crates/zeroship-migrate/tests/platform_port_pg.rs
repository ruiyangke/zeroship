//! Platform migration regression coverage for the live `db/migrations/` source.
//!
//! The one-time Liquibase-equivalence gate was retired after cutover completed in
//! f48c8a38: the old `db/changelog/` tree is intentionally gone, and the platform
//! schema is now sourced solely from the op/Flyway `db/migrations/` port. Ongoing
//! coverage here is the live port application test plus the Platform rollback
//! test against the real `.down.sql` files.
//!
//! This drives the REAL `command::runner::run_migrate` (not a shim, not a
//! spawned process) so the port is guarded by the same engine + guard the compose
//! `migrate` service will run. The privileged DDL in these files (CREATE ROLE /
//! GRANT / CREATE SCHEMA / ENABLE RLS / CREATE POLICY) is EXACTLY why the run must
//! be Platform: under Confined every one of those is DENIED.
//!
//! The ported tree uses the hardcoded `zeroship` schema and the global
//! `zeroship_*` roles, so this test cannot use a
//! token-suffixed throwaway schema like `cli_platform_pg.rs`. It instead resets
//! the two real schemas in the dedicated `zeroship_migrate_test` DB and uses a
//! token-suffixed META (journal) schema so concurrent runs do not collide on the
//! journal. Roles are cluster-wide; the files' `IF NOT EXISTS` / DO-block guards
//! make their creation idempotent across runs.

use std::path::PathBuf;

use compio_postgres::Client;
use zeroship_migrate::command::runner::{
    run_migrate, run_rollback, run_status, RunConfig, RunProfile, RunReport,
};
use zeroship_migrate::test_support::acquire_global_platform_resource_lock;

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";
const EXPECTED_PLATFORM_MIGRATIONS: usize = 64;
const LATEST_PLATFORM_MIGRATION_VERSION: u64 = 66;

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    pg_for_url(&dsn()).await
}

async fn pg_for_url(database_url: &str) -> Client {
    let (client, conn) = compio_postgres::connect(database_url, compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}")
}

/// The repo-root `db/migrations/` directory (the ported set under test).
fn migrations_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/zeroship-migrate
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../db/migrations")
        .canonicalize()
        .expect("db/migrations exists at repo root")
}

/// A Platform [`RunConfig`] over the REAL `zeroship` schema with `public` in the
/// allowlist, and a UNIQUE meta (journal) schema so the journal does not collide
/// with a concurrent run.
fn platform_cfg(meta: &str, yes: bool) -> RunConfig {
    platform_cfg_for_url(dsn(), meta, yes)
}

fn platform_cfg_for_url(database_url: String, meta: &str, yes: bool) -> RunConfig {
    RunConfig {
        dir: migrations_dir(),
        database_url,
        engine_override: None,
        profile: RunProfile::Platform,
        project_id: "platform".to_string(),
        project_schema: "zeroship".to_string(),
        schemas: vec!["zeroship".to_string(), "public".to_string()],
        // The changelog installs citext; the guard gates `CREATE EXTENSION`
        // against this allowlist.
        extensions: vec!["citext".to_string(), "uuid-ossp".to_string()],
        meta_schema: meta.to_string(),
        yes,
        statement_timeout: std::time::Duration::from_secs(120),
        lock_timeout: std::time::Duration::from_secs(30),
    }
}

/// Reset the platform schemas + journal so the run starts from a known-fresh DB.
/// Roles are cluster-wide and left intact (the files' guards make re-create a
/// no-op); we only reset the per-DB schema state and the journal.
async fn reset(conn: &Client, meta: &str) {
    conn.batch_execute(&format!(
        "DROP SCHEMA IF EXISTS zeroship CASCADE; \
         DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"
    ))
    .await
    .expect("reset platform schemas + journal");
}

async fn namespace_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_namespace WHERE nspname = $1", &[&name])
        .await
        .expect("query pg_namespace")
        .is_empty()
}

async fn role_exists(conn: &Client, name: &str) -> bool {
    !conn
        .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&name])
        .await
        .expect("query pg_roles")
        .is_empty()
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence")
        .is_empty()
}

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM information_schema.columns \
              WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column existence")
        .is_empty()
}

async fn index_exists(conn: &Client, schema: &str, index: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'i'",
            &[&schema, &index],
        )
        .await
        .expect("query index existence")
        .is_empty()
}

async fn policy_exists(conn: &Client, schema: &str, table: &str, policy: &str) -> bool {
    !conn
        .query(
            "SELECT 1 FROM pg_policy p \
               JOIN pg_class c ON c.oid = p.polrelid \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = $1 AND c.relname = $2 AND p.polname = $3",
            &[&schema, &table, &policy],
        )
        .await
        .expect("query pg_policy")
        .is_empty()
}

// ---------------------------------------------------------------------------
// The port gate: the full ported set applies under Platform, the expected
// schema inventory materializes, and a re-run is an idempotent no-op.
// ---------------------------------------------------------------------------

#[compio::test]
async fn ported_changelog_applies_under_platform_and_materializes_the_schema() {
    let _global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let meta = format!("portmeta_{}", token());
    reset(&conn, &meta).await;

    let cfg = platform_cfg(&meta, /* yes */ true);

    // 1. The WHOLE ported set applies with no error under Platform.
    let report = run_migrate(&cfg)
        .await
        .expect("the ported db/migrations set applies cleanly under Platform");
    let applied = match report {
        RunReport::Migrate(outcome) => {
            assert!(!outcome.is_noop(), "a fresh DB applies migrations");
            outcome.applied.len()
        }
        other => panic!("expected Migrate report, got {other:?}"),
    };
    assert_eq!(
        applied,
        EXPECTED_PLATFORM_MIGRATIONS,
        "all current ported files applied"
    );

    // 2. Namespace the changelog provisions.
    assert!(namespace_exists(&conn, "zeroship").await, "zeroship schema");

    // 3. Roles — the five platform service roles.
    for role in [
        "zeroship_auth",
        "zeroship_control",
        "zeroship_gateway",
        "zeroship_worker",
        "zeroship_app",
    ] {
        assert!(role_exists(&conn, role).await, "role {role} created");
    }

    // 4. Key tables across the auth / control / sandbox / billing domains.
    for (schema, table) in [
        ("zeroship", "users"),
        ("zeroship", "apps"),
        ("zeroship", "app_secrets"),
        ("zeroship", "gateway_sessions"),
        ("zeroship", "app_session_anchors"),
        ("zeroship", "app_user_identities"),
        ("zeroship", "app_members"),
        ("zeroship", "oauth_clients"),
        ("zeroship", "audit_events"),
        ("zeroship", "rate_limits"),
        ("zeroship", "sandboxes"),
        ("zeroship", "plans"),
        ("zeroship", "app_net_grants"),
        ("zeroship", "net_policy_catalog"),
        ("zeroship", "device_grants"),
        ("zeroship", "invoices"),
    ] {
        assert!(
            table_exists(&conn, schema, table).await,
            "{schema}.{table} exists"
        );
    }

    // 5. RLS policies — the four tenant_isolation policies 0025 installs.
    for table in [
        "app_secrets",
        "gateway_sessions",
        "app_session_anchors",
        "app_user_identities",
    ] {
        assert!(
            policy_exists(&conn, "zeroship", table, "tenant_isolation").await,
            "RLS policy tenant_isolation on zeroship.{table}"
        );
    }

    // 6. Idempotent re-run: nothing pending, no-op.
    let report2 = run_migrate(&cfg).await.expect("idempotent re-run");
    match report2 {
        RunReport::Migrate(outcome) => assert!(outcome.is_noop(), "re-run is a no-op"),
        other => panic!("expected Migrate report, got {other:?}"),
    }

    // 7. Status: all current migrations applied, none pending.
    match run_status(&cfg).await.expect("status reads journal") {
        RunReport::Status(status) => {
            assert_eq!(
                status.applied.len(),
                EXPECTED_PLATFORM_MIGRATIONS,
                "all current migrations applied"
            );
            assert!(status.pending.is_empty(), "nothing pending");
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    // Clean up the journal (leave the schemas; the next run resets them).
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"))
        .await
        .expect("drop journal schema");
    _global_lock.release().await;
}

// ---------------------------------------------------------------------------
// The port ROLLBACK gate (design §10, finding `platform-down-rollback-untested`):
// the ported `.down.sql` set + the Platform Down/Rollback commands are shipped
// (`run_down` / `run_rollback`) but no platform test exercised them — the port
// gate above only applies-forward + asserts an idempotent no-op. Rollback was
// covered ONLY for the project profile (`rollback_pg.rs`, synthetic in-test
// migrations), so the platform path that REPLACED Liquibase's `rollback` was
// untested against the REAL ported `.down.sql` files.
//
// This applies the whole ported set, then `run_rollback`'s the SINGLE most-recent
// platform migration (V0066) via its real `.down.sql` under profile=Platform, and
// asserts: the device-grant OP bindings are removed, the journal reflects the
// rollback (RunReport::Rollback names V0066; status drops by one with V0066
// pending), and the rolled-back migration RE-APPLIES forward cleanly (the down
// was faithful).
//
// V0066 is a self-contained reversible step: its up adds OP device-grant binding
// columns/indexes, and its down removes them without disturbing the base
// device_grants table.
//
// Drives the REAL `command::runner::run_rollback` (not a shim, not a spawned
// process) so the compose `migrate` service's rollback path is guarded by the same
// engine + guard. Rollback is destructive ⇒ the run uses `yes = true` (the runner's
// own --yes gate is exercised independently in `cli_platform_pg.rs`).
// ---------------------------------------------------------------------------

#[compio::test]
async fn ported_set_rolls_back_the_last_platform_migration_via_its_down_sql() {
    let _global_lock = acquire_global_platform_resource_lock(&dsn()).await;
    let conn = pg().await;
    let meta = format!("portrbmeta_{}", token());
    reset(&conn, &meta).await;

    // Apply the WHOLE ported set forward first (the precondition for a rollback).
    let cfg = platform_cfg(&meta, /* yes */ true);
    match run_migrate(&cfg).await.expect("ported set applies under Platform") {
        RunReport::Migrate(outcome) => {
            assert_eq!(
                outcome.applied.len(),
                EXPECTED_PLATFORM_MIGRATIONS,
                "all current ported files applied"
            );
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    // V0066's objects exist on top of the base device_grants table.
    assert!(
        table_exists(&conn, "zeroship", "device_grants").await,
        "base zeroship.device_grants table exists"
    );
    assert!(
        column_exists(&conn, "zeroship", "device_grants", "client_id").await,
        "V0066 added device_grants.client_id"
    );
    assert!(
        column_exists(&conn, "zeroship", "device_grants", "sid").await,
        "V0066 added device_grants.sid"
    );
    assert!(
        column_exists(&conn, "zeroship", "device_grants", "poll_interval_secs").await,
        "V0066 added device_grants.poll_interval_secs"
    );
    assert!(
        index_exists(&conn, "zeroship", "device_grants_client_id_idx").await,
        "V0066 added device_grants_client_id_idx"
    );

    // Roll back exactly the most-recently-applied migration (V0066) via its REAL
    // `.down.sql`. `run_rollback(.., None, Some(1))` is the `down` one-step target.
    let report = run_rollback(&cfg, None, Some(1))
        .await
        .expect("the last platform migration rolls back via its .down.sql");
    match report {
        RunReport::Rollback(outcome) => {
            // V0066 → version 66 → the derived `mig_…` id. Exactly one step undone.
            let expected = zeroship_migrate::migration_id_for_version(
                LATEST_PLATFORM_MIGRATION_VERSION,
            );
            assert_eq!(
                outcome.rolled_back,
                vec![expected.as_str().to_string()],
                "exactly V0066 was rolled back, via its real .down.sql"
            );
            assert!(
                outcome.skipped_irreversible.is_empty(),
                "V0066 is reversible — nothing force-skipped"
            );
        }
        other => panic!("expected Rollback report, got {other:?}"),
    }

    // The base table remains, and V0066's down removed only its bindings.
    assert!(
        table_exists(&conn, "zeroship", "device_grants").await,
        "rolling back V0066 leaves zeroship.device_grants intact"
    );
    assert!(
        !column_exists(&conn, "zeroship", "device_grants", "client_id").await,
        "rolling back V0066 removes device_grants.client_id"
    );
    assert!(
        !column_exists(&conn, "zeroship", "device_grants", "sid").await,
        "rolling back V0066 removes device_grants.sid"
    );
    assert!(
        !column_exists(&conn, "zeroship", "device_grants", "poll_interval_secs").await,
        "rolling back V0066 removes device_grants.poll_interval_secs"
    );
    assert!(
        !index_exists(&conn, "zeroship", "device_grants_client_id_idx").await,
        "rolling back V0066 removes device_grants_client_id_idx"
    );

    // The journal reflects the rollback: one fewer applied, V0066 now pending.
    match run_status(&cfg).await.expect("status reads the journal post-rollback") {
        RunReport::Status(status) => {
            assert_eq!(
                status.applied.len(),
                EXPECTED_PLATFORM_MIGRATIONS - 1,
                "the journal shows one fewer applied after rolling back V0066"
            );
            assert_eq!(
                status.pending.len(),
                1,
                "V0066 is the single pending migration after its rollback"
            );
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    // Roll-forward heals: re-applying re-runs ONLY V0066. Proves the ported
    // down/up pair round-trips.
    match run_migrate(&cfg).await.expect("re-apply heals the rolled-back step") {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 1, "only V0066 re-applied");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert!(
        column_exists(&conn, "zeroship", "device_grants", "client_id").await,
        "re-applying V0066 restores device_grants.client_id"
    );
    assert!(
        column_exists(&conn, "zeroship", "device_grants", "sid").await,
        "re-applying V0066 restores device_grants.sid"
    );
    assert!(
        column_exists(&conn, "zeroship", "device_grants", "poll_interval_secs").await,
        "re-applying V0066 restores device_grants.poll_interval_secs"
    );
    assert!(
        index_exists(&conn, "zeroship", "device_grants_client_id_idx").await,
        "re-applying V0066 restores device_grants_client_id_idx"
    );

    // Clean up the journal (leave the schemas; the next run resets them).
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"))
        .await
        .expect("drop journal schema");
    _global_lock.release().await;
}
