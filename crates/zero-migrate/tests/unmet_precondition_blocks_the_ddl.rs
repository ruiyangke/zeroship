//! SQLite refuses a precondition-bearing migration rather than ignoring it.
//!
//! Preconditions are a fail-closed gate, and the SQLite backend does not
//! implement evaluation at all: `descriptor migrations carry none`, so the engine
//! never generates one for this dialect. The safety question is what happens if a
//! migration carrying preconditions reaches it anyway — hand-authored, or
//! produced by a future path that forgets the restriction.
//!
//! Ignoring the checks and running the DDL would be the dangerous answer: the
//! operator wrote a guard, the engine reported success, and nothing was guarded.
//! It errors instead, and the DDL does not run.
//!
//! WHAT THIS FILE DOES NOT COVER, stated because the first version of it silently
//! did not cover this either: the actual EVALUATION of a precondition — unmet
//! `Halt` aborting, unmet `Skip` leaving the migration pending, a met check
//! letting the DDL through — is PostgreSQL-only and needs a live-database arm.
//!
//! HOW THE FIRST VERSION WAS WRONG, since it is the whole reason this file reads
//! the way it does. It asserted "an unmet precondition runs no DDL" against
//! SQLite and PASSED — because the apply errored on the unsupported feature, not
//! because any check was evaluated. It would have passed identically with a check
//! that was MET. The control arm below is what exposed it: a migration whose
//! precondition holds could not apply either, which is impossible if evaluation
//! were happening at all.

mod support;

use std::path::PathBuf;

use tempfile::TempDir;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::model::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
use zero_migrate::model::precondition::{OnUnmet, Precondition, PreconditionCheck};
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    model::migration::Migration, Approval, ExecutorConfig, MigrationEngine, SqliteBackend,
};

const PROJECT: &str = "prj_pc";
const OWNER: &str = "app_test";

struct Db {
    _dir: TempDir,
    backend: SqliteBackend,
}

fn open_db() -> Db {
    let dir = tempfile::tempdir().expect("tempdir");
    let app: PathBuf = dir.path().join("pc.sqlite");
    let journal: PathBuf = dir.path().join("pc.migrations.sqlite");
    let backend = SqliteBackend::open(&app, &journal).expect("open the hardened sqlite backend");
    Db { _dir: dir, backend }
}

const UP: &str = "CREATE TABLE guarded_table (id integer primary key)";

/// `create_guarded_table`, optionally gated on `table`.
fn migration(precondition_on: Option<&str>) -> Migration {
    let checks: Vec<PreconditionCheck> = precondition_on
        .map(|table| {
            vec![PreconditionCheck {
                check: Precondition::TableExists {
                    table: table.to_string(),
                },
                on_unmet: OnUnmet::Halt,
            }]
        })
        .unwrap_or_default();
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up: UP,
        down: None,
        flags: &flags,
        owner_app: OWNER,
        depends_on: &[],
        supersedes: &[],
        preconditions: &checks,
    });
    Migration {
        version: MigrationId::generate(),
        name: "create_guarded_table".to_string(),
        up: UP.to_string(),
        down: None,
        checksum,
        flags,
        owner_app: OWNER.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: checks,
        existence_guard: None,
    }
}

async fn guarded_table_exists(backend: &SqliteBackend) -> bool {
    !backend
        .actor()
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name='guarded_table'")
        .await
        .expect("read the live table list")
        .is_empty()
}

async fn apply(db: &Db, cfg: &ExecutorConfig, migration: Migration, tag: &str) -> bool {
    MigrationEngine::new()
        .apply_plan(
            &[PlanStep::Ddl(migration)],
            Approval::Approved,
            &db.backend,
            cfg,
            tag,
            LockMode::Acquire,
        )
        .await
        .is_ok()
}

#[compio::test]
async fn a_precondition_bearing_migration_is_refused_and_runs_nothing() {
    let db = open_db();
    let cfg = ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT));

    // The check is SATISFIABLE — the table exists — so a backend that evaluated
    // preconditions would apply this. SQLite must still refuse, because it cannot
    // evaluate at all, and refusing is the only safe answer.
    db.backend
        .actor()
        .query("CREATE TABLE sentinel (id integer primary key)")
        .await
        .expect("create the table the precondition names");

    assert!(
        !apply(&db, &cfg, migration(Some("sentinel")), "with-precondition").await,
        "SQLite cannot evaluate preconditions, so it must REFUSE a migration that \
         carries them. Applying them unevaluated would mean the operator wrote a \
         guard, the engine reported success, and nothing was guarded"
    );
    assert!(
        !guarded_table_exists(&db.backend).await,
        "the refusal must happen BEFORE the DDL runs"
    );
    assert!(
        db.backend
            .applied(&cfg)
            .await
            .expect("read the journal")
            .is_empty(),
        "a refused migration must not be journaled as applied"
    );
}

#[compio::test]
async fn the_same_migration_without_preconditions_applies() {
    // THE CONTROL, and it is load-bearing: the arm above asserts a table is
    // ABSENT, which is also what you see if the fixture never got far enough to
    // run anything. This proves the identical migration DOES apply once the
    // preconditions are removed, so the absence above is caused by them.
    //
    // An earlier version of this file had no such control and asserted a
    // precondition was EVALUATED. It passed while nothing was evaluated at all.
    let db = open_db();
    let cfg = ExecutorConfig::new(PROJECT, PROJECT, support::no_inject(PROJECT));

    assert!(
        apply(&db, &cfg, migration(None), "no-precondition").await,
        "the same migration without preconditions must apply"
    );
    assert!(
        guarded_table_exists(&db.backend).await,
        "the control must actually create the table, or the absence asserted above \
         proves nothing about preconditions"
    );
}
