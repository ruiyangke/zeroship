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
//! `zeroship_*` roles, so this test cannot use a token-suffixed throwaway schema
//! like `cli_platform_pg.rs`. It instead resets the real schema in the dedicated
//! `zeroship_migrate_test` DB and uses a token-suffixed META (journal) schema so
//! concurrent runs do not collide on the journal. Roles are cluster-wide; the
//! files' `IF NOT EXISTS` / DO-block guards make their creation idempotent across
//! runs.

use std::path::PathBuf;

use compio_postgres::Client;
use zeroship_migrate::command::runner::{
    run_migrate, run_rollback, run_status, RunConfig, RunProfile, RunReport,
};
use zeroship_migrate::test_support::acquire_global_platform_resource_lock;

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

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

fn platform_up_migration_versions() -> Vec<u64> {
    let mut versions: Vec<u64> = std::fs::read_dir(migrations_dir())
        .expect("read db/migrations")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.starts_with('V') || !name.ends_with(".sql") || name.ends_with(".down.sql") {
                return None;
            }
            name[1..]
                .split_once("__")
                .and_then(|(version, _)| version.parse::<u64>().ok())
        })
        .collect();
    versions.sort_unstable();
    versions
}

fn platform_migration_count() -> usize {
    platform_up_migration_versions().len()
}

fn latest_platform_migration_version() -> u64 {
    *platform_up_migration_versions()
        .last()
        .expect("db/migrations has at least one up migration")
}

/// A Platform [`RunConfig`] over the REAL `zeroship` schema with the
/// `zeroship` + `public` namespaces in the allowlist, and a UNIQUE meta
/// (journal) schema so the journal does not collide with a concurrent run.
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
        // The changelog installs citext (V0001) + uuid-ossp (V0027); the guard
        // gates `CREATE EXTENSION` against this allowlist, so both must be named.
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

async fn metric_weight(conn: &Client, metric: &str) -> Option<(i64, i64)> {
    conn.query(
        "SELECT units_per_op, per_units FROM zeroship.metric_weights WHERE metric = $1",
        &[&metric],
    )
    .await
    .expect("query metric weight")
    .first()
    .map(|row| (row.get("units_per_op"), row.get("per_units")))
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

async fn assert_latest_platform_migration_effect(
    conn: &Client,
    latest_version: u64,
    present: bool,
) {
    let state = if present { "materialized" } else { "rolled back" };
    match latest_version {
        66 => {
            for column in ["client_id", "sid", "poll_interval_secs"] {
                assert_eq!(
                    column_exists(conn, "zeroship", "device_grants", column).await,
                    present,
                    "V0066 {state} zeroship.device_grants.{column}"
                );
            }
            for index in [
                "device_grants_client_id_idx",
                "device_grants_provider_pending_user_code_idx",
            ] {
                assert_eq!(
                    index_exists(conn, "zeroship", index).await,
                    present,
                    "V0066 {state} zeroship.{index}"
                );
            }
        }
        65 => {
            assert_eq!(
                table_exists(conn, "zeroship", "oidc_session_clients").await,
                present,
                "V0065 {state} zeroship.oidc_session_clients"
            );
            for (table, column) in [
                ("oauth_clients", "backchannel_logout_uri"),
                ("oauth_authorization_codes", "sid"),
                ("gateway_sessions", "sid"),
            ] {
                assert_eq!(
                    column_exists(conn, "zeroship", table, column).await,
                    present,
                    "V0065 {state} zeroship.{table}.{column}"
                );
            }
        }
        64 => {
            assert_eq!(
                table_exists(conn, "zeroship", "oauth_refresh_tokens").await,
                present,
                "V0064 {state} zeroship.oauth_refresh_tokens"
            );
        }
        63 => {
            assert_eq!(
                table_exists(conn, "zeroship", "oauth_authorization_codes").await,
                present,
                "V0063 {state} zeroship.oauth_authorization_codes"
            );
        }
        other => panic!("add a latest-migration effect assertion for V{other:04}"),
    }
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
    let expected_count = platform_migration_count();

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
        applied, expected_count,
        "all ported db/migrations up files applied"
    );

    // 2. Namespace.
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

    // 7. Status: all platform migrations applied, none pending.
    match run_status(&cfg).await.expect("status reads journal") {
        RunReport::Status(status) => {
            assert_eq!(
                status.applied.len(),
                expected_count,
                "all platform migrations applied"
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
// platform migration via its real `.down.sql` under profile=Platform, and
// asserts: the latest migration's known effect is removed, the journal reflects
// the rollback, and the rolled-back migration RE-APPLIES forward cleanly (the
// down was faithful).
//
// V0059 is a self-contained reversible step: its up makes net_ingress_bytes
// attribution-only (0 CU weight), and its down restores the former V0058 billable
// weight without disturbing the rest of the schema.
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
    let expected_count = platform_migration_count();
    let latest_version = latest_platform_migration_version();
    match run_migrate(&cfg).await.expect("ported set applies under Platform") {
        RunReport::Migrate(outcome) => {
            assert_eq!(
                outcome.applied.len(),
                expected_count,
                "all ported db/migrations up files applied"
            );
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    // Earlier platform billing objects still exist, V0059 has zeroed only the net
    // ingress attribution metric's billable weight, and the discovered latest
    // migration's known effect exists before rollback.
    assert!(
        table_exists(&conn, "zeroship", "app_net_grants").await,
        "V0058 materialized zeroship.app_net_grants"
    );
    assert!(
        table_exists(&conn, "zeroship", "net_policy_catalog").await,
        "V0058 materialized zeroship.net_policy_catalog"
    );
    assert_eq!(
        metric_weight(&conn, "net_ingress_bytes").await,
        Some((0, 10000)),
        "V0059 makes net_ingress_bytes attribution-only"
    );
    assert_latest_platform_migration_effect(&conn, latest_version, true).await;

    // Roll back exactly the most-recently-applied migration via its REAL
    // `.down.sql`. `run_rollback(.., None, Some(1))` is the `down` one-step target.
    let report = run_rollback(&cfg, None, Some(1))
        .await
        .expect("the last platform migration rolls back via its .down.sql");
    match report {
        RunReport::Rollback(outcome) => {
            let expected = zeroship_migrate::migration_id_for_version(latest_version);
            assert_eq!(
                outcome.rolled_back,
                vec![expected.as_str().to_string()],
                "exactly the latest platform migration was rolled back via its real .down.sql"
            );
            assert!(
                outcome.skipped_irreversible.is_empty(),
                "the latest platform migration is reversible — nothing force-skipped"
            );
        }
        other => panic!("expected Rollback report, got {other:?}"),
    }

    // The V0058 tables and V0059 billing weight remain; the latest migration's
    // known effect was removed.
    assert!(
        table_exists(&conn, "zeroship", "app_net_grants").await,
        "rolling back the latest migration leaves zeroship.app_net_grants intact"
    );
    assert!(
        table_exists(&conn, "zeroship", "net_policy_catalog").await,
        "rolling back the latest migration leaves zeroship.net_policy_catalog intact"
    );
    assert_eq!(
        metric_weight(&conn, "net_ingress_bytes").await,
        Some((0, 10000)),
        "rolling back the latest migration leaves V0059's attribution-only weight intact"
    );
    assert_latest_platform_migration_effect(&conn, latest_version, false).await;

    // The journal reflects the rollback: one fewer applied, one pending.
    match run_status(&cfg).await.expect("status reads the journal post-rollback") {
        RunReport::Status(status) => {
            assert_eq!(
                status.applied.len(),
                expected_count - 1,
                "the journal shows one fewer applied after rolling back the latest migration"
            );
            assert_eq!(
                status.pending.len(),
                1,
                "the latest migration is the single pending migration after rollback"
            );
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    // Roll-forward heals: re-applying re-runs ONLY the latest migration. Proves the
    // ported down/up pair round-trips.
    match run_migrate(&cfg).await.expect("re-apply heals the rolled-back step") {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 1, "only the latest migration re-applied");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert_eq!(
        metric_weight(&conn, "net_ingress_bytes").await,
        Some((0, 10000)),
        "re-applying the latest migration leaves V0059's attribution-only weight intact"
    );
    assert_latest_platform_migration_effect(&conn, latest_version, true).await;

    // Clean up the journal (leave the schemas; the next run resets them).
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"))
        .await
        .expect("drop journal schema");
    _global_lock.release().await;
}
