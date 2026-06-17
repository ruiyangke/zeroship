//! Faithful pre-apply **integrity-manifest gate** tests (v3 Plan F) against a
//! REAL Postgres (no shims).
//!
//! These exercise [`MigrationEngine::apply_verified`]: a correct expected
//! [`ManifestHash`] applies the set normally; a mismatched expected (the bundle
//! was tampered — reordered / content-edited / inserted-into / removed-from)
//! refuses with [`EngineError::Manifest`] and applies NOTHING — no project table
//! created, no completed journal rows, and (critically) the refusal happens
//! BEFORE the advisory lock / any DDL.
//!
//! Requires the dedicated database `zeroship_migrate_test` on :5440 and a
//! connecting role with `CREATEROLE` (the runbook uses `postgres`). Set
//! `MIGRATE_TEST_DB` to override the DSN. Each test uses uniquely-suffixed
//! schemas + project id (→ a unique role), so tests are isolated + re-runnable.

use compio_postgres::Client;
use zeroship_migrate::{
    author::{AuthorRequest, Column, DeterministicAuthor, MigrationAuthor},
    compute_manifest, migrator_role_name, provision_migrator, role::deprovision_migrator, Approval,
    EngineError, ExecutorConfig, GuardConfig, ManifestHash, Migration, MigrationEngine,
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
    let role = migrator_role_name(&c.project_id).unwrap();
    c.with_migrator_role(role)
}

fn guard_cfg(cfg: &ExecutorConfig) -> GuardConfig {
    GuardConfig {
        project_schema: cfg.project_schema.clone(),
        extension_allowlist: Vec::new(),
    }
}

async fn ensure_project_schema(conn: &Client, cfg: &ExecutorConfig) {
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS \"{}\"",
        cfg.project_schema
    ))
    .await
    .expect("create project schema");
}

async fn setup(conn: &Client, cfg: &ExecutorConfig) {
    ensure_project_schema(conn, cfg).await;
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

/// Does the meta schema's journal table exist at all? (Created by `ensure_journal`
/// on the FIRST apply that takes the lock. If the manifest gate refuses BEFORE the
/// lock, the journal table is never created.)
async fn journal_table_exists(conn: &Client, cfg: &ExecutorConfig) -> bool {
    let rows = conn
        .query(
            "SELECT 1 FROM information_schema.tables \
             WHERE table_schema = $1 AND table_name = 'schema_migrations'",
            &[&cfg.meta_schema],
        )
        .await
        .expect("query journal table existence");
    !rows.is_empty()
}

fn det(cfg: &ExecutorConfig) -> DeterministicAuthor {
    DeterministicAuthor::new(cfg.project_schema.clone(), "app_test")
}

/// A safe two-migration additive set (create + add column) authored cleanly.
fn additive_set(cfg: &ExecutorConfig) -> Vec<Migration> {
    let create = det(cfg)
        .author(&AuthorRequest::CreateTable {
            name: "orders".into(),
            columns: vec![Column {
                name: "id".into(),
                ty: "bigint".into(),
                nullable: false,
            }],
        })
        .unwrap();
    let add = det(cfg)
        .author(&AuthorRequest::AddColumn {
            table: "orders".into(),
            column: Column {
                name: "note".into(),
                ty: "text".into(),
                nullable: true,
            },
        })
        .unwrap();
    create.into_iter().chain(add).collect()
}

// ---------------------------------------------------------------------------
// Happy path — a correct expected manifest applies normally
// ---------------------------------------------------------------------------

#[compio::test]
async fn correct_manifest_applies_the_set_normally() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    // The control plane stamps this at review time.
    let expected = compute_manifest(&set);

    let engine = MigrationEngine::new();
    let plan = engine.plan(&set, &guard_cfg(&cfg));
    let outcome = engine
        .apply_verified(&plan, Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("a matching manifest must apply the set");

    assert_eq!(outcome.applied.len(), 2, "both migrations applied");
    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn none_expected_applies_unverified() {
    // expected = None ⇒ apply unverified (internal-caller back-compat-free path).
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let engine = MigrationEngine::new();
    let plan = engine.plan(&set, &guard_cfg(&cfg));
    let outcome = engine
        .apply_verified(&plan, None, Approval::None, &conn, &cfg, "app_test")
        .await
        .expect("None expected applies unverified");
    assert_eq!(outcome.applied.len(), 2);
    assert!(table_exists(&conn, &cfg.project_schema, "orders").await);
    teardown(&conn, &cfg).await;
}

// ---------------------------------------------------------------------------
// Tamper rejection — every mutation refuses, applying NOTHING, before the lock
// ---------------------------------------------------------------------------

/// Assert the refusal applied nothing AND happened before any DDL/journal work:
/// no project table, and the journal table itself was never created (it is
/// created by `ensure_journal` only AFTER the advisory lock is taken — i.e.
/// strictly after the manifest gate would have passed).
async fn assert_refused_before_apply(conn: &Client, cfg: &ExecutorConfig, err: EngineError) {
    assert!(
        matches!(err, EngineError::Manifest(_)),
        "expected a manifest mismatch, got {err:?}"
    );
    assert!(
        !table_exists(conn, &cfg.project_schema, "orders").await,
        "no project table may exist after a refused apply"
    );
    assert!(
        !journal_table_exists(conn, cfg).await,
        "the journal table must NOT exist — the gate refused BEFORE the lock / ensure_journal / any DDL"
    );
}

#[compio::test]
async fn reordered_set_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker reorders the SAME two migrations (each migration's own checksum is
    // unchanged — only the set order differs).
    let tampered = vec![set[1].clone(), set[0].clone()];

    let engine = MigrationEngine::new();
    let plan = engine.plan(&tampered, &guard_cfg(&cfg));
    let err = engine
        .apply_verified(&plan, Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a reordered set must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn inserted_migration_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker inserts an extra (even guard-safe) migration into the bundle.
    let extra = det(&cfg)
        .author(&AuthorRequest::CreateTable {
            name: "smuggled".into(),
            columns: vec![Column {
                name: "id".into(),
                ty: "bigint".into(),
                nullable: false,
            }],
        })
        .unwrap();
    let mut tampered = set.clone();
    tampered.extend(extra);

    let engine = MigrationEngine::new();
    let plan = engine.plan(&tampered, &guard_cfg(&cfg));
    let err = engine
        .apply_verified(&plan, Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("an inserted migration must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    // The smuggled table must NOT exist either.
    assert!(!table_exists(&conn, &cfg.project_schema, "smuggled").await);
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn removed_migration_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker drops a migration from the reviewed bundle.
    let tampered = vec![set[0].clone()];

    let engine = MigrationEngine::new();
    let plan = engine.plan(&tampered, &guard_cfg(&cfg));
    let err = engine
        .apply_verified(&plan, Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a removed migration must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn content_edited_migration_is_refused_before_apply() {
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let expected = compute_manifest(&set);
    // Attacker edits the FIRST migration's `up` in place (same version), so its
    // per-migration checksum changes ⇒ the manifest changes. Keep the version,
    // recompute the checksum to mirror a tampered-but-self-consistent bundle.
    let mut edited = set[0].clone();
    edited.up = format!(
        "CREATE TABLE \"{}\".\"orders\" (id bigint NOT NULL, evil text)",
        cfg.project_schema
    );
    edited.checksum =
        zeroship_migrate::Checksum::of(&zeroship_migrate::ChecksumInput::from_migration(&edited));
    let tampered = vec![edited, set[1].clone()];

    let engine = MigrationEngine::new();
    let plan = engine.plan(&tampered, &guard_cfg(&cfg));
    let err = engine
        .apply_verified(&plan, Some(&expected), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a content-edited migration must be refused");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}

#[compio::test]
async fn garbage_expected_hash_fails_closed() {
    // An expected hash that cannot match (wrong/short) refuses every set.
    let conn = pg().await;
    let cfg = cfg_for(&token());
    setup(&conn, &cfg).await;

    let set = additive_set(&cfg);
    let garbage = ManifestHash::from_hex("deadbeef");

    let engine = MigrationEngine::new();
    let plan = engine.plan(&set, &guard_cfg(&cfg));
    let err = engine
        .apply_verified(&plan, Some(&garbage), Approval::None, &conn, &cfg, "app_test")
        .await
        .expect_err("a garbage expected hash must fail closed");
    assert_refused_before_apply(&conn, &cfg, err).await;
    teardown(&conn, &cfg).await;
}
