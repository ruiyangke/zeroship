//! Faithful repeatable-migration tests (v3 Plan E — Flyway `R__` / Liquibase
//! `runOnChange`) against a REAL Postgres (no shims).
//!
//! A repeatable migration ([`MigrationFlags::repeatable`]) has a STABLE identity
//! (its `version`/name never changes per edit) and RE-APPLIES whenever its
//! definition checksum changes — for views/functions/triggers edited over time.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 (recreated by
//! the test runbook). Each test runs in its own meta + project schema.

use compio_postgres::Client;
use zeroship_migrate::migration::Checksum;
use zeroship_migrate::{
    apply, Approval, ExecutorConfig, Migration, MigrationFlags, MigrationId,
};

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{pid}_{nanos}_{n}")
}

fn cfg_for(tok: &str) -> ExecutorConfig {
    let mut c = ExecutorConfig::new(format!("prj_{tok}"), format!("proj_{tok}"));
    c.meta_schema = format!("meta_{tok}");
    c
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn drop_schemas(conn: &Client, cfg: &ExecutorConfig) {
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

/// Build an ordinary (versioned, run-once) transactional migration.
fn versioned(version: MigrationId, name: &str, up: &str) -> Migration {
    Migration {
        version,
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        checksum: Checksum::of(up, None, &[]),
        flags: MigrationFlags::default(),
        owner_app: "app_test".to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
    }
}

/// Build a REPEATABLE migration with a stable identity and a correct checksum.
fn repeatable(version: MigrationId, name: &str, up: &str) -> Migration {
    let mut m = versioned(version, name, up);
    m.flags.repeatable = true;
    m
}

/// Re-checksum a migration after editing its `up` (the author would re-derive it
/// from the new definition; here we recompute it the same way `versioned` does).
fn with_up(mut m: Migration, up: &str) -> Migration {
    m.up = up.to_string();
    m.checksum = Checksum::of(up, m.down.as_deref(), &m.preconditions);
    m
}

/// Count RAW `completed` events for a version (every re-apply appends one), as
/// opposed to net-applied state — a repeatable accrues multiple over re-applies.
async fn completed_events(conn: &Client, cfg: &ExecutorConfig, version: &str) -> i64 {
    let row = conn
        .query_one(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_migrations \
                 WHERE version = $1 AND phase = 'completed'",
                cfg.meta_schema
            ),
            &[&version],
        )
        .await
        .expect("count completed events");
    row.get("n")
}

/// Run a one-row scalar `int` (`int4`) query against the project (returns the
/// view's value widened to `i64` for convenient comparison).
async fn scalar_int(conn: &Client, sql: &str) -> i64 {
    i64::from(
        conn.query_one(sql, &[])
            .await
            .expect("scalar query")
            .get::<_, i32>(0),
    )
}

// ---------------------------------------------------------------------------
// COMMIT 1 — repeatable phase: apply, skip-unchanged, re-apply-on-change, ordering
// ---------------------------------------------------------------------------

#[compio::test]
async fn repeatable_view_applies_on_first_deploy() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let r = repeatable(
        MigrationId::generate(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    let out = apply(&conn, &cfg, std::slice::from_ref(&r), Approval::None, "actor")
        .await
        .expect("apply repeatable");

    assert_eq!(out.applied, vec![r.version.as_str().to_string()]);
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        1,
        "the view must exist and return 1"
    );
    assert_eq!(
        completed_events(&conn, &cfg, r.version.as_str()).await,
        1,
        "one completed event after first apply"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn repeatable_unchanged_redeploy_is_skipped() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let r = repeatable(
        MigrationId::generate(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    apply(&conn, &cfg, std::slice::from_ref(&r), Approval::None, "actor")
        .await
        .expect("first apply");

    // Redeploy with the SAME definition (same checksum) ⇒ SKIPPED, not re-applied.
    let out = apply(&conn, &cfg, std::slice::from_ref(&r), Approval::None, "actor")
        .await
        .expect("redeploy unchanged");

    assert!(
        out.applied.is_empty(),
        "an unchanged repeatable must not re-apply, got {:?}",
        out.applied
    );
    assert!(
        out.skipped.contains(&r.version.as_str().to_string()),
        "an unchanged repeatable must be reported skipped"
    );
    assert_eq!(
        completed_events(&conn, &cfg, r.version.as_str()).await,
        1,
        "still exactly one completed event (no new row on a skip)"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn repeatable_changed_redeploy_is_reapplied() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    let v = MigrationId::generate();
    let r1 = repeatable(
        v.clone(),
        "v_view",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 1 AS n"),
    );
    apply(&conn, &cfg, std::slice::from_ref(&r1), Approval::None, "actor")
        .await
        .expect("first apply");
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        1
    );

    // Same identity (version/name), CHANGED definition (SELECT 2) ⇒ RE-APPLIED.
    let r2 = with_up(
        r1.clone(),
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT 2 AS n"),
    );
    assert_ne!(r1.checksum, r2.checksum, "the edit must change the checksum");

    let out = apply(&conn, &cfg, std::slice::from_ref(&r2), Approval::None, "actor")
        .await
        .expect("redeploy changed");

    assert_eq!(
        out.applied,
        vec![v.as_str().to_string()],
        "a changed repeatable must re-apply"
    );
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        2,
        "the view must now return the NEW definition's value"
    );
    assert_eq!(
        completed_events(&conn, &cfg, v.as_str()).await,
        2,
        "two completed events after a re-apply (append-only)"
    );

    drop_schemas(&conn, &cfg).await;
}

#[compio::test]
async fn repeatables_run_after_versioned_pending() {
    let conn = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    drop_schemas(&conn, &cfg).await;
    ensure_project_schema(&conn, &cfg).await;
    let schema = &cfg.project_schema;

    // A versioned migration creates the table; the repeatable view reads it. If the
    // repeatable ran BEFORE the versioned migration, the CREATE VIEW would fail
    // (relation does not exist). Ordering after-versioned is what makes this pass.
    let table = versioned(
        MigrationId::generate(),
        "create_nums",
        &format!("CREATE TABLE \"{schema}\".nums (n int); INSERT INTO \"{schema}\".nums VALUES (7)"),
    );
    let view = repeatable(
        MigrationId::generate(),
        "v_over_nums",
        &format!("CREATE OR REPLACE VIEW \"{schema}\".v AS SELECT n FROM \"{schema}\".nums"),
    );

    // Supply the repeatable FIRST in the slice to prove ordering is by phase, not
    // by input order.
    let set = vec![view.clone(), table.clone()];
    let out = apply(&conn, &cfg, &set, Approval::None, "actor")
        .await
        .expect("apply mixed set");

    // The versioned migration applies first, then the repeatable.
    assert_eq!(
        out.applied,
        vec![table.version.as_str().to_string(), view.version.as_str().to_string()],
        "versioned must apply before the repeatable"
    );
    assert_eq!(
        scalar_int(&conn, &format!("SELECT n FROM \"{schema}\".v")).await,
        7
    );

    drop_schemas(&conn, &cfg).await;
}
