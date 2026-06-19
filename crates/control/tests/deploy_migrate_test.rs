//! Faithful integration tests for the P6 deploy-migrate step
//! (`zeroship_control::deploy_migrate::apply_bundle_migrations`).
//!
//! These drive the REAL apply path — `zeroship-migrate`'s `load_dir` +
//! `engine.plan(Confined)` + `engine.apply` against a real Postgres — exactly
//! as the control deploy handler does, NOT a shim. The handler's only extra
//! layer (reconstructing the migration files from blobs into a tmp dir) is
//! itself a thin file write; the load-bearing logic (provision schema + role,
//! apply under Confined, refuse-destructive, go-live gating) is what these
//! exercise directly with a hand-authored migration directory.
//!
//! Requires `zeroship_migrate_test` on :5440 (psql password `zeroship`). When
//! the DB is unreachable the tests are skipped silently (matching the
//! `CONTROL_TEST_DB`/`PG_TEST_URL` pattern in `deploy_test.rs`).

use std::path::{Path, PathBuf};

use compio_postgres::NoTls;
use uuid::Uuid;

use zeroship_control::deploy_migrate::{apply_bundle_migrations, DeployMigrateError};

/// Admin DSN with CREATEROLE + CREATE SCHEMA (the `postgres` superuser), the
/// same DB the migrate crate's own integration tests use.
const ADMIN_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn admin_dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| ADMIN_DSN.to_string())
}

/// Open a raw admin connection for the assert side, or `None` when the DB is
/// unreachable (⇒ the test skips).
async fn admin_conn() -> Option<compio_postgres::Client> {
    match compio_postgres::connect(&admin_dsn(), NoTls).await {
        Ok((client, conn)) => {
            compio::runtime::spawn(async move {
                let _ = conn.run().await;
            })
            .detach();
            Some(client)
        }
        Err(_) => None,
    }
}

/// A fresh per-test app id ⇒ a fresh schema `"<app_id>"` (no cross-test
/// contention; the engine's advisory lock + journal are per-app).
fn fresh_app_id() -> Uuid {
    Uuid::now_v7()
}

/// Write a set of `(filename, body)` migration files into a fresh tmp dir,
/// returning the dir path (the caller removes it).
fn migrations_dir(files: &[(&str, &str)]) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("zship-mig-test-{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).expect("mkdir tmp migrations");
    for (name, body) in files {
        std::fs::write(dir.join(name), body).expect("write migration file");
    }
    dir
}

/// Drop a per-app schema + its meta schema + migrator role so a re-run is clean.
async fn cleanup_app(conn: &compio_postgres::Client, app_id: &Uuid) {
    let schema = app_id.to_string();
    let role = zeroship_migrate::migrator_role_name(&schema).unwrap();
    // The migrator owns the schema; reassign before drop so the cascade works.
    let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS {} CASCADE; \
             DROP SCHEMA IF EXISTS {} CASCADE;",
            q(&format!("{schema}_migrations")),
            q(&schema),
        ))
        .await;
    let _ = conn
        .batch_execute(&format!(
            "DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname='{r}') THEN \
                EXECUTE 'REASSIGN OWNED BY {rq} TO current_user'; \
                EXECUTE 'DROP OWNED BY {rq}'; \
                EXECUTE 'DROP ROLE {rq}'; \
             END IF; END $$;",
            r = role.replace('\'', "''"),
            rq = q(&role),
        ))
        .await;
}

/// Does table `table_name` exist in schema `"<app_id>"`?
async fn table_exists(conn: &compio_postgres::Client, app_id: &Uuid, table: &str) -> bool {
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query information_schema");
    !rows.is_empty()
}

/// How many migrations are journaled completed for this app? Returns 0 when
/// the per-app meta schema / journal table does not exist yet (a refused plan
/// never bootstraps the journal).
async fn journaled_count(conn: &compio_postgres::Client, app_id: &Uuid) -> i64 {
    let meta = format!("{}_migrations", app_id);
    let q = format!("\"{}\".schema_migrations", meta.replace('"', "\"\""));
    let lit = q.replace('\'', "''");
    // Probe existence first (a refused plan never bootstraps the journal); a
    // direct `FROM <missing table>` would parse-error even inside a CASE arm.
    let present = conn
        .query(&format!("SELECT to_regclass('{lit}') IS NOT NULL AS p"), &[])
        .await
        .expect("regclass probe");
    if !present[0].get::<_, bool>("p") {
        return 0;
    }
    let rows = conn
        .query(
            &format!("SELECT count(*)::int8 AS n FROM {q} WHERE phase = 'completed'"),
            &[],
        )
        .await
        .expect("count journal");
    rows[0].get::<_, i64>("n")
}

// ---------------------------------------------------------------------------
// Happy path: a CREATE TABLE migration is applied + journaled.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_applies_create_table_and_journals() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let dir = migrations_dir(&[(
        "V0001__create_widgets.sql",
        "CREATE TABLE widgets (id bigint PRIMARY KEY, name text NOT NULL);",
    )]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("deploy-migrate must succeed");
    assert_eq!(outcome.applied.len(), 1, "one migration applied");
    assert!(outcome.skipped.is_empty(), "nothing skipped on first apply");

    // The table exists in the per-app schema "<app_id>".
    assert!(
        table_exists(&conn, &app_id, "widgets").await,
        "widgets table must exist in schema {app_id}"
    );
    // The migration is journaled in the per-app meta schema.
    assert_eq!(journaled_count(&conn, &app_id).await, 1, "migration journaled");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Idempotency: a re-deploy of the same migrations is a no-op (already
// journaled), proving the deploy path is roll-forward safe.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_is_idempotent_across_redeploys() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let dir = migrations_dir(&[(
        "V0001__create_gadgets.sql",
        "CREATE TABLE gadgets (id bigint PRIMARY KEY);",
    )]);

    let first = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("first apply");
    assert_eq!(first.applied.len(), 1);

    // Second deploy of the SAME set: nothing new applied (all journaled).
    let second = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("second apply (idempotent)");
    assert!(
        second.applied.is_empty(),
        "re-deploy applies nothing new, got {:?}",
        second.applied
    );
    assert_eq!(journaled_count(&conn, &app_id).await, 1, "still one journaled");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Failure: a destructive migration is REFUSED at deploy (Approval::None) and
// the table is NOT created — the go-live gate (the caller's "on error, no
// commit") therefore holds.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_refuses_destructive_and_creates_nothing() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // A single migration that DROPs a table is destructive ⇒ refused at deploy.
    let dir = migrations_dir(&[(
        "V0001__drop_legacy.sql",
        "DROP TABLE IF EXISTS legacy;",
    )]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("destructive migration must be refused at deploy");
    match err {
        DeployMigrateError::Apply(_) => {}
        other => panic!("expected Apply(ApprovalRequired), got {other:?}"),
    }

    // Nothing was applied: the journal has no completed rows (the table the
    // migration referenced was never created either).
    assert_eq!(
        journaled_count(&conn, &app_id).await,
        0,
        "a refused destructive plan journals nothing"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Failure: a malformed migration filename is a load error (no DB touched).
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_rejects_bad_migration_filename() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    // `garbage.sql` matches no migration grammar ⇒ a hard LoaderError.
    let dir = migrations_dir(&[("garbage.sql", "CREATE TABLE t (id int);")]);

    let err = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect_err("unrecognized filename must fail the load");
    assert!(
        matches!(err, DeployMigrateError::Load(_)),
        "expected Load error, got {err:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// ---------------------------------------------------------------------------
// Two-migration set applies in order; the second references the first's table.
// ---------------------------------------------------------------------------

#[compio::test]
async fn deploy_migrate_applies_multiple_in_order() {
    let Some(conn) = admin_conn().await else {
        eprintln!("SKIP: zeroship_migrate_test :5440 unreachable");
        return;
    };
    let app_id = fresh_app_id();
    cleanup_app(&conn, &app_id).await;

    let dir = migrations_dir(&[
        (
            "V0001__create_orders.sql",
            "CREATE TABLE orders (id bigint PRIMARY KEY);",
        ),
        (
            "V0002__add_orders_total.sql",
            "ALTER TABLE orders ADD COLUMN total numeric NOT NULL DEFAULT 0;",
        ),
    ]);

    let outcome = apply_bundle_migrations(&admin_dsn(), &app_id, &dir)
        .await
        .expect("apply two migrations");
    assert_eq!(outcome.applied.len(), 2, "both migrations applied");
    assert_eq!(journaled_count(&conn, &app_id).await, 2);

    // The ALTER (V0002) succeeded only because V0001 ran first — prove the
    // column exists.
    let schema = app_id.to_string();
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema=$1 AND table_name='orders' AND column_name='total'",
            &[&schema],
        )
        .await
        .expect("columns query");
    assert!(!rows.is_empty(), "V0002's column must exist (ordering held)");

    let _ = std::fs::remove_dir_all(&dir);
    cleanup_app(&conn, &app_id).await;
}

// A path-only reference so an unused-import lint never fires if a test is
// cfg'd out in a future refactor.
#[allow(dead_code)]
fn _path_marker(_: &Path) {}
