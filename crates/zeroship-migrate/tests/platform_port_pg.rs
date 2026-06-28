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
//! The ported tree uses the hardcoded `zeroship` + `oauth_hydra` schemas and the
//! global `zeroship_*` / `oauth_hydra` roles, so this test cannot use a
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

/// A Platform [`RunConfig`] over the REAL `zeroship` schema with the
/// `oauth_hydra` + `public` namespaces in the allowlist (so 0027's `CREATE SCHEMA
/// oauth_hydra` is in-allowlist), and a UNIQUE meta (journal) schema so the journal
/// does not collide with a concurrent run.
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
        schemas: vec![
            "zeroship".to_string(),
            "oauth_hydra".to_string(),
            "public".to_string(),
        ],
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
         DROP SCHEMA IF EXISTS oauth_hydra CASCADE; \
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
    assert_eq!(applied, 57, "all 57 ported files applied (0045 is a gap)");

    // 2. Namespaces (the two the changelog provisions).
    assert!(namespace_exists(&conn, "zeroship").await, "zeroship schema");
    assert!(namespace_exists(&conn, "oauth_hydra").await, "oauth_hydra schema");

    // 3. Roles — the five platform service roles + the Hydra role (0025 / 0027).
    for role in [
        "zeroship_auth",
        "zeroship_control",
        "zeroship_gateway",
        "zeroship_worker",
        "zeroship_app",
        "oauth_hydra",
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

    // 7. Status: all 57 applied, none pending.
    match run_status(&cfg).await.expect("status reads journal") {
        RunReport::Status(status) => {
            assert_eq!(status.applied.len(), 57, "57 applied");
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
// platform migration (V0057) via its real `.down.sql` under profile=Platform, and
// asserts: the objects V0058 created are GONE, the journal reflects the rollback
// (RunReport::Rollback names V0058; status drops to 56 applied with V0058 pending),
// and the rolled-back migration RE-APPLIES forward cleanly (the down was faithful).
//
// V0058 is a self-contained reversible step: its up creates app net grants,
// the net-policy catalog row/table, plan net limits, and the ingress metric;
// its down removes those objects/rows without disturbing the rest of the schema.
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
            assert_eq!(outcome.applied.len(), 57, "all 57 ported files applied");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    // The objects V0058's `up` creates must exist before we roll it back.
    assert!(
        table_exists(&conn, "zeroship", "app_net_grants").await,
        "V0058 materialized zeroship.app_net_grants"
    );
    assert!(
        table_exists(&conn, "zeroship", "net_policy_catalog").await,
        "V0058 materialized zeroship.net_policy_catalog"
    );

    // Roll back exactly the most-recently-applied migration (V0058) via its REAL
    // `.down.sql`. `run_rollback(.., None, Some(1))` is the `down` one-step target.
    let report = run_rollback(&cfg, None, Some(1))
        .await
        .expect("the last platform migration rolls back via its .down.sql");
    match report {
        RunReport::Rollback(outcome) => {
            // V0058 → version 58 → the derived `mig_…` id. Exactly one step undone.
            let expected = zeroship_migrate::migration_id_for_version(58);
            assert_eq!(
                outcome.rolled_back,
                vec![expected.as_str().to_string()],
                "exactly V0058 was rolled back, via its real .down.sql"
            );
            assert!(
                outcome.skipped_irreversible.is_empty(),
                "V0058 is reversible — nothing force-skipped"
            );
        }
        other => panic!("expected Rollback report, got {other:?}"),
    }

    // The dropped objects are GONE on the REAL DB (the `.down.sql` actually ran).
    assert!(
        !table_exists(&conn, "zeroship", "app_net_grants").await,
        "rolling back V0058 dropped zeroship.app_net_grants"
    );
    assert!(
        !table_exists(&conn, "zeroship", "net_policy_catalog").await,
        "rolling back V0058 dropped zeroship.net_policy_catalog"
    );

    // The journal reflects the rollback: 56 applied, V0058 now pending again.
    match run_status(&cfg).await.expect("status reads the journal post-rollback") {
        RunReport::Status(status) => {
            assert_eq!(
                status.applied.len(),
                56,
                "the journal shows 56 applied after rolling back V0058"
            );
            assert_eq!(
                status.pending.len(),
                1,
                "V0058 is the single pending migration after its rollback"
            );
        }
        other => panic!("expected Status report, got {other:?}"),
    }

    // Roll-forward heals: re-applying re-runs ONLY V0058 (the down was faithful, so
    // its up re-creates the tables). Proves the ported down/up pair round-trips.
    match run_migrate(&cfg).await.expect("re-apply heals the rolled-back step") {
        RunReport::Migrate(outcome) => {
            assert_eq!(outcome.applied.len(), 1, "only V0058 re-applied");
        }
        other => panic!("expected Migrate report, got {other:?}"),
    }
    assert!(
        table_exists(&conn, "zeroship", "app_net_grants").await,
        "re-applying V0058 re-created zeroship.app_net_grants"
    );
    assert!(
        table_exists(&conn, "zeroship", "net_policy_catalog").await,
        "re-applying V0058 re-created zeroship.net_policy_catalog"
    );

    // Clean up the journal (leave the schemas; the next run resets them).
    conn.batch_execute(&format!("DROP SCHEMA IF EXISTS \"{meta}\" CASCADE;"))
        .await
        .expect("drop journal schema");
    _global_lock.release().await;
}
