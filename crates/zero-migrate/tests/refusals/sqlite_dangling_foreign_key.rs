//! F673: SQLite must not apply a foreign key whose target table is never created.
//!
//! SQLite has no `ALTER TABLE ADD CONSTRAINT`, so a FORWARD reference — a target
//! created later in the same envelope — can only be expressed by inlining the FK
//! into `CREATE TABLE`. `declarative.rs` therefore inlines every create-time FK
//! on SQLite. That is deliberate and necessary.
//!
//! What it did not do is separate
//!
//!     created LATER in this envelope   legitimate; must inline
//!     never created ANYWHERE           dangling; must be refused
//!
//! PostgreSQL and MySQL get that distinction for free: their FKs go onto
//! `pending_foreign_keys`, entries are flushed as each target is created, and
//! anything still pending when lowering ends raises
//! `DeferredForeignKeyTargetNotCreated`. SQLite pushed nothing onto that list, so
//! it had no end-of-lowering check at all, and the dangling case reached a real
//! database:
//!
//!     CREATE TABLE "k" (..., CONSTRAINT "k_f0_fkey" FOREIGN KEY (f0) REFERENCES p(c0))
//!     tables present   ["k"]                  -- p was never created
//!     INSERT INTO k    Err("no such table: main.p")
//!
//! A migration reported successful produced a table that cannot accept one row,
//! from an envelope PostgreSQL and MySQL both refuse.
//!
//! BOTH ARMS ARE LOAD-BEARING. A test that pinned only the dangling case would be
//! satisfied by deleting the SQLite inline branch outright, which would break the
//! forward reference that branch exists to support. The control arm is what makes
//! the fix have to be the narrow one.

use crate::support;

use std::collections::BTreeMap;
use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::sqlite::Mode;
use zero_migrate::{
    apply::executor::LockMode, Approval, ExecutorConfig, GuardConfig, IrAuthor, LiveSchema,
    MigrationEngine, SqlDialect, SqliteBackend,
};

const PROJECT: &str = "prj_ir";
const APP: &str = "app_ir";

/// `createTable k` carrying a foreign key into `p`.
fn create_k_referencing_p() -> &'static str {
    r#"{"op":"createTable","name":"k","columns":[{"name":"c0","type":"bigInt","nullable":false},{"name":"f0","type":"bigInt","nullable":false}],"primaryKey":["c0"],"constraints":[{"kind":{"kind":"fk","columns":["f0"],"referencesTable":"p","referencesColumns":["c0"]}}]}"#
}

fn create_p() -> &'static str {
    r#"{"op":"createTable","name":"p","columns":[{"name":"c0","type":"bigInt","nullable":false}],"primaryKey":["c0"]}"#
}

fn envelope(ops: &[&str]) -> String {
    format!(
        r#"{{"ir_version":1,"name":"fk_case","ops":[{}]}}"#,
        ops.join(",")
    )
}

fn registry() -> BTreeMap<String, String> {
    [("k", APP), ("p", APP)]
        .iter()
        .map(|(t, o)| ((*t).to_string(), (*o).to_string()))
        .collect()
}

struct Db {
    _dir: TempDir,
    backend: SqliteBackend,
}

fn open_db(tag: &str) -> Db {
    let dir = tempfile::tempdir().expect("tempdir");
    let app: PathBuf = dir.path().join(format!("{tag}.sqlite"));
    let journal: PathBuf = dir.path().join(format!("{tag}.migrations.sqlite"));
    let backend = SqliteBackend::open(&app, &journal).expect("open hardened sqlite backend");
    Db { _dir: dir, backend }
}

fn lower_for(dialect: SqlDialect, bytes: &str) -> Result<zero_migrate::LoweredArtifact, String> {
    IrAuthor::new(PROJECT, APP, dialect, &support::confined_charter())
        .load_and_lower_guarded(
            bytes,
            APP,
            &registry(),
            &LiveSchema::default(),
            &GuardConfig::from_policy(support::no_inject(PROJECT), dialect),
        )
        .map_err(|e| format!("{e:?}"))
}

#[test]
fn a_foreign_key_target_that_is_never_created_is_refused_on_every_dialect() {
    let bytes = envelope(&[create_k_referencing_p()]);

    // PostgreSQL and MySQL already refuse this, and are quoted here so the SQLite
    // arm is measured against its own engine's behaviour rather than my opinion.
    for dialect in [SqlDialect::Postgres, SqlDialect::Mysql] {
        let refusal = lower_for(dialect, &bytes).expect_err(&format!(
            "{dialect:?} must refuse a foreign key to a table nothing creates"
        ));
        assert!(
            refusal.contains("DeferredForeignKeyTargetNotCreated"),
            "{dialect:?} refused for the wrong reason: {refusal}"
        );
    }

    let refusal = lower_for(SqlDialect::Sqlite, &bytes).expect_err(
        "SQLite must refuse a foreign key whose target no operation creates and no live \
         schema holds. Inlining it produces a table that cannot accept a row: the applied \
         schema references p(c0), and INSERT INTO k fails with `no such table: main.p`",
    );
    assert!(
        refusal.contains("ForeignKeyTargetNotCreated"),
        "SQLite must name the missing target the way the other dialects do: {refusal}"
    );
}

#[compio::test]
async fn a_forward_reference_still_lowers_and_applies_on_sqlite() {
    // CONTROL. The target is created LATER IN THE SAME ENVELOPE. SQLite cannot
    // express this as a follow-on ALTER, so inlining it is the only encoding and
    // must keep working. This arm is what stops the fix from being "stop inlining
    // on SQLite".
    let bytes = envelope(&[create_k_referencing_p(), create_p()]);

    let artifact = lower_for(SqlDialect::Sqlite, &bytes)
        .expect("a forward reference whose target IS created later must still lower on SQLite");

    let db = open_db("fk-forward");
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            &db.backend,
            &ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT)),
            "f673-forward-reference",
            LockMode::Acquire,
        )
        .await
        .expect("the forward-reference envelope applies on real SQLite");

    db.backend.actor().set_mode(Mode::CreatorUp).await.unwrap();

    let tables = db
        .backend
        .actor()
        .query(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('k','p') ORDER BY name",
        )
        .await
        .expect("list the two tables");
    assert_eq!(
        tables,
        vec![vec![Some("k".to_string())], vec![Some("p".to_string())]],
        "both tables must exist after a forward-reference envelope applies"
    );

    // And the FK is real: the row is accepted once its parent exists, which is
    // the behaviour the dangling case could never reach.
    db.backend
        .actor()
        .query("INSERT INTO p (c0) VALUES (1)")
        .await
        .expect("the parent row inserts");
    db.backend
        .actor()
        .query("INSERT INTO k (c0, f0) VALUES (1, 1)")
        .await
        .expect("a child row referencing an existing parent inserts");
}
