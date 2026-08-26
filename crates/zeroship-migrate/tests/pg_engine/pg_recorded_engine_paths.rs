//! The five recorder-driven tests that drive the ENGINE, not the PostgreSQL backend.
//!
//! They came out of `apply/backend/postgres/mod.rs` with the rest of the execution
//! half, and they are the ones that could not follow it into
//! `zero-migrate-postgres`: each drives `MigrationEngine`, `AppliedPlan` or
//! `ops::status::history_via_backend`, which are the engine's, and `zero-migrate`
//! depends on `zero-migrate-postgres`, so a vendor crate reaching back is a cycle
//! Cargo refuses.
//!
//! They drive the SAME recorder the eight vendor-internal ones do, reached through
//! `zero-migrate-postgres`'s `testing` feature. A second copy of that canned catalog
//! is the hazard the feature exists to avoid: its rows are the shared premise of
//! both suites and two copies drift silently.

use crate::support;

use std::sync::atomic::Ordering;
use zero_migrate::engine::{DeclarativeApplyError, EngineError, MigrationEngine};
use zero_migrate::render::plan::AppliedPlan;

use zero_migrate_backend::approval::{Approval, ApprovalScope};
use zero_migrate_backend::backend::MigrationBackend;
use zero_migrate_backend::conn::ExecutorConfig;
use zero_migrate_backend::driver::{Bind, Row, Value};
use zero_migrate_backend::executor::{ApplyError, LockMode};
use zero_migrate_backend::requirements::DatabaseFeature;
use zero_migrate_backend::step::PlanStep;
use zero_migrate_ir::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};
use zero_migrate_postgres::backend::recording::{
    canned_journal_row, plan_backfill_step, plan_dml_step, RecordingSession,
};
use zero_migrate_postgres::backend::{journal_sql, status_sql, PostgresBackend};

async fn apply_recorded_plan(
    rec: &RecordingSession,
    steps: &[PlanStep],
    approval: Approval,
    scope: &ApprovalScope,
) -> Result<zero_migrate::engine::DeclarativeDeployOutcome, DeclarativeApplyError> {
    let backend = PostgresBackend::<'_, RecordingSession>::new_generic(rec);
    MigrationEngine::new(zero_migrate::shipping_vendors())
        .apply_plan_with_touched_and_depends_scoped(
            steps,
            &["users".into()],
            &[],
            approval,
            scope,
            &backend,
            &ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x")),
            "tester",
            LockMode::Acquire,
            None,
        )
        .await
}

#[compio::test]
async fn mixed_plan_refuses_pending_delete_before_earlier_update() {
    let rec = RecordingSession::new();
    let (update, _, _) = plan_dml_step("update users", false);
    let (delete, _, _) = plan_dml_step("delete users", true);

    let result =
        apply_recorded_plan(&rec, &[update, delete], Approval::None, &ApprovalScope::All).await;

    assert!(matches!(
        result,
        Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired))
    ));
    let log = rec.log.borrow();
    assert!(
        !log.iter().any(|entry| {
            entry.contains("UPDATE users SET ready") || entry.contains("DELETE FROM users WHERE id")
        }),
        "approval preflight must run before either target mutation: {log:?}"
    );
}

#[compio::test]
async fn mixed_plan_treats_partial_backfill_as_pending_before_earlier_update() {
    let (backfill, version, checksum) = plan_backfill_step();
    let rec = RecordingSession::with_canned_progress(
        vec![Row::new(
            vec!["backfill_id".into(), "checksum".into(), "complete".into()],
            vec![
                Value::Text(version.as_str().to_string()),
                Value::Text(checksum.as_str().to_string()),
                Value::Bool(false),
            ],
        )],
        true,
    );
    let (update, _, _) = plan_dml_step("update before backfill", false);

    let result = apply_recorded_plan(
        &rec,
        &[update, backfill],
        Approval::None,
        &ApprovalScope::All,
    )
    .await;

    assert!(matches!(
        result,
        Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired))
    ));
    let log = rec.log.borrow();
    assert!(
        log.iter().any(|entry| entry.contains("schema_backfills")),
        "preflight must reconcile partial progress: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|entry| entry.contains("UPDATE users SET ready")),
        "the earlier update must not run before a pending backfill gate: {log:?}"
    );
}

#[compio::test]
async fn completed_delete_skips_without_renewed_approval_but_drift_aborts_plan() {
    let (update, update_version, _) = plan_dml_step("update users", false);
    let (delete, delete_version, delete_checksum) = plan_dml_step("delete users", true);
    let rec = RecordingSession::with_canned_journal(vec![canned_journal_row(
        delete_version.as_str(),
        delete_checksum.as_str(),
    )]);

    let outcome = apply_recorded_plan(
        &rec,
        &[update.clone(), delete.clone()],
        Approval::None,
        &ApprovalScope::Versions(Default::default()),
    )
    .await
    .expect("a matching completed delete is an unapproved no-op");
    assert_eq!(outcome.applied.applied, vec![update_version.as_str()]);
    assert_eq!(outcome.applied.skipped, vec![delete_version.as_str()]);
    assert!(
        !rec.log
            .borrow()
            .iter()
            .any(|entry| entry.contains("DELETE FROM users WHERE id")),
        "the completed delete must not execute again"
    );

    let stale = Checksum::of(&ChecksumInput {
        up: "stale delete",
        down: None,
        flags: &MigrationFlags::default(),
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    let drift_rec = RecordingSession::with_canned_journal(vec![canned_journal_row(
        delete_version.as_str(),
        stale.as_str(),
    )]);
    let result = apply_recorded_plan(
        &drift_rec,
        &[update, delete],
        Approval::None,
        &ApprovalScope::All,
    )
    .await;
    assert!(matches!(
        result,
        Err(DeclarativeApplyError::Plain(EngineError::Apply(
            ApplyError::ChecksumDrift { .. }
        )))
    ));
    assert!(
        !drift_rec
            .log
            .borrow()
            .iter()
            .any(|entry| entry.contains("UPDATE users SET ready")),
        "drift must abort before the earlier update"
    );
}

#[compio::test]
async fn plan_requirement_refuses_before_authored_sql_runs() {
    let rec = RecordingSession::with_server_version(170_000);
    let backend = PostgresBackend::new_generic(&rec);
    let flags = MigrationFlags::default();
    let up = "CREATE TABLE authored_uuid_v7 (id uuid DEFAULT uuidv7())";
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    let migration = Migration {
        version: MigrationId::generate(),
        name: "authored UUIDv7 table".into(),
        up: up.into(),
        down: None,
        checksum,
        flags,
        owner_app: "app_test".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
        effect: None,
    };
    let mut plan = AppliedPlan::single_step(migration);
    plan.database_requirements
        .require(DatabaseFeature::UuidV7Generation);

    let result = MigrationEngine::new(zero_migrate::shipping_vendors())
        .apply_applied_plan_with_touched_and_depends(
            &plan,
            &[],
            &[],
            Approval::None,
            &backend,
            &ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x")),
            "tester",
            LockMode::Acquire,
        )
        .await;
    let error = result.expect_err("PostgreSQL 17 must refuse UUIDv7 generation");
    assert!(error.to_string().contains("PostgreSQL 18"), "{error}");
    let log = rec.log.borrow();
    assert!(
        log.iter()
            .any(|entry| entry.contains("current_setting('server_version_num')")),
        "the version preflight must run: {log:?}"
    );
    assert!(
        !log.iter()
            .any(|entry| entry.contains("CREATE TABLE authored_uuid_v7")),
        "authored DDL must not run after a capability refusal: {log:?}"
    );
}

/// One-in-flight, mechanically-proven: drive a FULL sweep over the whole DDL +
/// journal-write + journal-read + drift-read + status/history surface against the host-
/// shaped recording driver **with the `in_flight` guard armed**, and assert it
/// **never trips** (the test would panic inside the driver if any verb were
/// issued while another's future is still alive). This converts the one-in-flight
/// invariant from by-analogy (the MySQL precedent) to checked over the exact
/// generic PG apply/introspection code paths a host driver drives.
///
/// It simultaneously proves genericity end-to-end: the WRITE path records the
/// expected SQL sequence (schema/journal DDL + a journal INSERT with neutral
/// Bind params), the READ path returns driver::Rows the engine decodes
/// (`applied` → `AppliedEntry`), and `status()`/`history()` run over the same
/// driver — their decoded shapes matching what a live host driver produces.
#[compio::test]
async fn full_surface_runs_generically_with_in_flight_guard_never_tripping() {
    let rec =
        RecordingSession::with_canned_journal(vec![canned_journal_row("mig_0001", "cafef00d")]);
    let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
    let cfg = ExecutorConfig::new("prj_x", "proj_x", support::no_inject("proj_x"));

    // 1. WRITE / DDL — journal bootstrap: CREATE SCHEMA + the append-only events
    //    table + the immutability trigger, all through `batch`.
    backend
        .ensure_journal(&cfg)
        .await
        .expect("ensure_journal DDL");

    // 2. WRITE — a journal INSERT (`record_started`) through `exec` with
    //    neutral Bind params. Drives the param-side seam on a write.
    journal_sql::record_started(&rec, &cfg, "mig_0001", "create_users", "cafef00d", "tester")
        .await
        .expect("record_started journal write");

    // 3. READ (journal) — `applied()` decodes the canned journal Row into an
    //    AppliedEntry over the non-compio driver.
    let applied = backend.applied(&cfg).await.expect("applied read");
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].version, "mig_0001");
    assert_eq!(applied[0].checksum, "cafef00d");

    // 4. READ (drift/catalog) — `snapshot_schema` issues its catalog introspection
    //    queries; the empty canned rows yield an empty-but-valid snapshot, proving
    //    the whole introspection decode chain runs over Row.
    let snap = backend
        .snapshot_schema(&cfg)
        .await
        .expect("snapshot_schema");
    assert!(
        snap.tables.is_empty(),
        "empty canned catalog → empty snapshot (decode chain ran clean)"
    );

    // 5. READ — the PostgreSQL status snapshot over the SAME driver
    //    (generalized to `<D: SqlSession>`), and the neutral history verb
    //    through the backend contract.
    let st = status_sql::status(&rec, &cfg, &[])
        .await
        .expect("status over host driver");
    // The canned journal row is net-applied, so status sees it as applied.
    assert!(
        st.applied.iter().any(|e| e.version == "mig_0001"),
        "status decoded the net-applied version over Row: {:?}",
        st.applied
    );
    let hist = zero_migrate::ops::status::history_via_backend(&backend, &cfg)
        .await
        .expect("history over host driver");
    // history() over the empty canned history read returns an empty log without
    // error — the point is the decode path ran over the neutral seam.
    assert!(hist.is_empty(), "empty canned history decoded to empty log");

    // The guard was armed on every verb above and never tripped (a trip would
    // have panicked inside the driver). Assert it is cleared (RAII released) and
    // that the expected WRITE SQL sequence was recorded.
    assert!(
        !rec.in_flight.load(Ordering::Acquire),
        "in_flight guard released after the last verb (RAII clear)"
    );
    let log = rec.log.borrow();
    assert!(
        log.iter().any(|s| s.contains("CREATE SCHEMA")),
        "ensure_journal recorded the CREATE SCHEMA DDL: {log:?}"
    );
    assert!(
        log.iter().any(|s| s.contains("schema_migrations")),
        "the journal DDL/INSERT sequence touched schema_migrations: {log:?}"
    );
    assert!(
        log.iter()
            .any(|s| s.starts_with("exec:") && s.contains("INSERT INTO")),
        "record_started drove a journal INSERT through exec: {log:?}"
    );
    // The journal INSERT bound its fields as neutral Binds (param widening).
    assert!(
        rec.binds.borrow().iter().any(|b| b
            .iter()
            .any(|v| matches!(v, Bind::Text(t) if t == "mig_0001"))),
        "journal INSERT bound the version as a neutral Bind::Text"
    );
}
