//! Preconditions are actually EVALUATED on PostgreSQL, and an unmet one runs no DDL.
//!
//! `unmet_precondition_blocks_the_ddl.rs` covers the SQLite half — that dialect
//! cannot evaluate preconditions and refuses a migration carrying them — and
//! states in its own module doc that the evaluation semantics are PostgreSQL-only
//! and untested. This is that arm.
//!
//! Three outcomes, and each has to be distinguished from the others:
//!
//!     unmet + Halt   the apply ABORTS, the DDL does not run, nothing is journaled
//!     unmet + Skip   the apply SUCCEEDS, the DDL still does not run
//!     met            the apply succeeds AND the DDL runs
//!
//! THE MET ARM IS NOT OPTIONAL. The first two assert that a table is ABSENT, and
//! absence is equally what you observe when the fixture never ran — which is
//! exactly how the SQLite version of this test passed while evaluating nothing at
//! all (it errored on an unsupported feature and I read that as a working guard).
//! The met arm is the only thing proving evaluation happens rather than
//! everything being refused for some unrelated reason.
//!
//! `Skip` needs the database as its oracle for the same reason: it succeeds by
//! design, so a Skip that quietly ran its DDL is indistinguishable from a correct
//! one if you only look at the return value.
//!
//! GATE: `ZERO_MIGRATE_TEST_PG_URL`.

use crate::support;

use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::apply::executor::LockMode;
use zero_migrate::driver::SqlSession;
use zero_migrate::model::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};
use zero_migrate::model::precondition::{OnUnmet, Precondition, PreconditionCheck};
use zero_migrate::render::step::PlanStep;
use zero_migrate::{
    model::migration::Migration, Approval, ExecutorConfig, MigrationEngine, PostgresBackend,
};

const OWNER: &str = "app_precondition_pg";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "precondition_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

/// `CREATE TABLE <schema>.guarded_table`, gated on `TableExists { table }`.
fn migration(schema: &str, gate_on: &str, on_unmet: OnUnmet) -> Migration {
    let up = format!("CREATE TABLE \"{schema}\".\"guarded_table\" (id integer PRIMARY KEY)");
    let checks = vec![PreconditionCheck {
        check: Precondition::TableExists {
            table: gate_on.to_string(),
        },
        on_unmet,
    }];
    let flags = MigrationFlags::default();
    let checksum = Checksum::of(&ChecksumInput {
        up: &up,
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
        up,
        down: None,
        checksum,
        flags,
        owner_app: OWNER.to_string(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: checks,
        existence_guard: None,
        effect: None,
    }
}

async fn guarded_table_exists(session: &PgDevSession, schema: &str) -> bool {
    !session
        .query(
            &format!(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = '{schema}' AND table_name = 'guarded_table'"
            ),
            &[],
        )
        .await
        .expect("read the live table list")
        .is_empty()
}

/// `(applied_ok, table_created, journal_entries)` for one gated migration.
async fn run_case(
    session: &PgDevSession,
    gate_on: &str,
    on_unmet: OnUnmet,
    seed_sentinel: bool,
) -> (bool, bool, usize) {
    let schema = token();
    let cfg = ExecutorConfig::new(
        format!("project_{schema}"),
        &schema,
        support::no_inject(&schema),
    );
    let _guard = support::SchemaGuard::arm(
        session,
        [cfg.project_schema.clone(), cfg.pg.meta_schema.clone()],
    );
    session
        .batch(&format!("CREATE SCHEMA \"{}\"", cfg.project_schema))
        .await
        .expect("create the isolated test schema");

    if seed_sentinel {
        session
            .batch(&format!(
                "CREATE TABLE \"{}\".\"sentinel\" (id integer PRIMARY KEY)",
                cfg.project_schema
            ))
            .await
            .expect("create the table the precondition names");
    }

    let backend = PostgresBackend::new_generic(session);
    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure the migration journal");

    let applied = MigrationEngine::new()
        .apply_plan(
            &[PlanStep::Ddl(migration(
                &cfg.project_schema,
                gate_on,
                on_unmet,
            ))],
            Approval::Approved,
            &backend,
            &cfg,
            "precondition",
            LockMode::Acquire,
        )
        .await;

    let created = guarded_table_exists(session, &cfg.project_schema).await;
    let journaled = backend.applied(&cfg).await.expect("read the journal").len();
    (applied.is_ok(), created, journaled)
}

#[compio::test]
async fn a_met_precondition_lets_the_ddl_run() {
    // THE CONTROL. Without it, the two arms below are satisfied by an engine that
    // refuses everything for an unrelated reason — which is precisely how the
    // SQLite version of this test passed while evaluating nothing.
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    let (ok, created, journaled) = run_case(&session, "sentinel", OnUnmet::Halt, true).await;
    assert!(ok, "a migration whose precondition holds must apply");
    assert!(
        created,
        "a MET precondition must let the DDL run, or the absences asserted by the \
         unmet arms prove nothing about preconditions"
    );
    assert_eq!(journaled, 1, "an applied migration must be journaled");
}

#[compio::test]
async fn an_unmet_halting_precondition_aborts_and_runs_no_ddl() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    let (ok, created, journaled) =
        run_case(&session, "table_that_does_not_exist", OnUnmet::Halt, false).await;
    assert!(
        !ok,
        "OnUnmet::Halt must abort the apply when the check is unmet"
    );
    assert!(
        !created,
        "the DDL RAN despite its precondition being unmet. A guard that reports a \
         failure and applies anyway is worse than no guard: the operator believes \
         the check protected them"
    );
    assert_eq!(
        journaled, 0,
        "a halted migration must not be journaled as applied"
    );
}

#[compio::test]
async fn an_unmet_skipping_precondition_succeeds_and_still_runs_no_ddl() {
    let url = skip_if_no_pg!();
    let session = PgDevSession::connect(&url);

    // Skip SUCCEEDS by design, so the return value cannot be the oracle: a Skip
    // that quietly ran its DDL looks identical from there.
    let (_ok, created, journaled) =
        run_case(&session, "table_that_does_not_exist", OnUnmet::Skip, false).await;
    assert!(
        !created,
        "OnUnmet::Skip left the migration pending but RAN its DDL anyway. The point \
         of Skip is that the next deploy re-evaluates the check, which is meaningless \
         if the work already happened"
    );
    assert_eq!(
        journaled, 0,
        "a skipped migration must stay pending, not be recorded as applied"
    );
}
