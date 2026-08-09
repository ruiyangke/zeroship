//! Behavioral integration test: drive ONE real
//! `executor::apply` through the addon's [`NapiHostSession`] over a MOCK
//! [`VerbDispatch`] that answers with canned `driver::Row`s — NO Node host, NO DB.
//!
//! This is the addon's analogue of the in-crate `RecordingSession` proof
//! (`crates/zero-migrate/src/apply/backend/postgres/mod.rs`), but exercised through
//! the *addon's* bridge types (`NapiHostSession` + `VerbDispatch`), so it proves:
//!
//! 1. `executor::apply::<NapiHostSession<MockDispatch>>` monomorphizes and runs the
//!    whole DDL + lock + journal flow generically over the host bridge (the
//!    convergence point) — a real behavioral assertion, not just "no error";
//! 2. the recorded verb sequence contains the expected structural landmarks
//!    (advisory lock acquire → confinement SET → the migration's `up` DDL →
//!    journal write-back → advisory unlock), in order;
//! 3. the one-in-flight guard never trips across the whole apply — proving
//!    the engine is strictly one-verb-at-a-time over a pinned host connection;
//! 4. the reactor-less `futures::executor::block_on` drives the whole engine future
//!    to completion when every I/O leaf is answered inline (the executor).
//!
//! The mock answers the journal net-state read with an EMPTY rowset (nothing
//! applied yet), so the supplied migration is pending and IS applied; its version
//! then appears in the `ApplyOutcome.applied` list — the journal outcome assertion.

mod support;

use std::cell::RefCell;

use zero_migrate::apply::executor::{apply, LockMode};
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::migration::{
    Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId,
};
use zero_migrate::{BindValue, MigrationEngine, PlanStep, PostgresBackend};

use zero_migrate_node::marshal::{JsCell, JsReply, JsRequest, JsRow};
use zero_migrate_node::session::{NapiHostSession, VerbDispatch, VerbReply};

/// A recording mock host driver: logs the `{kind, sql}` of every verb and answers
/// read verbs with canned rows routed by SQL shape. This stands in for the JS
/// host `pg` driver — the addon's `TsfnDispatch` fires the real `pg` over a TSFN;
/// this mock answers inline so the whole apply runs without a Node host or DB.
struct MockDispatch {
    log: RefCell<Vec<String>>,
}

impl MockDispatch {
    const fn new() -> Self {
        Self {
            log: RefCell::new(Vec::new()),
        }
    }

    /// Route a read to canned rows by SQL shape. ONLY the journal net-state read
    /// (recognisable by the `union_all` CTE + the `schema_migrations_inflight` UNION
    /// leg) gets rows — and we return NONE (nothing applied yet), so the supplied
    /// migration is pending and gets applied. Every other read (introspection,
    /// squash, drift) gets an empty rowset — a valid empty decode.
    fn rows_for(&self, sql: &str) -> Vec<JsRow> {
        if sql.contains("current_setting('statement_timeout')") {
            return vec![JsRow {
                columns: vec!["st".into(), "lt".into(), "sp".into()],
                cells: vec![
                    text_cell("0"),
                    text_cell("0"),
                    text_cell("\"$user\", public"),
                ],
            }];
        }
        if sql.contains("performance_schema.events_transactions_current") {
            return vec![JsRow {
                columns: vec![
                    "transaction_tracking_enabled".into(),
                    "in_transaction".into(),
                ],
                cells: vec![int_cell(1), int_cell(0)],
            }];
        }
        if sql.contains("GET_LOCK(?, ?)") {
            return vec![JsRow {
                columns: vec!["got".into()],
                cells: vec![int_cell(1)],
            }];
        }
        if sql.contains("COLLATION_NAME AS collation_name")
            && sql.contains("schema_migrations_inflight")
        {
            return [
                ("schema_migrations", "version"),
                ("schema_migrations", "checksum"),
                ("schema_migrations_supersedes", "squash_version"),
                ("schema_migrations_supersedes", "superseded_version"),
                ("schema_migrations_inflight", "version"),
                ("schema_migrations_inflight", "checksum"),
                ("schema_migrations_recovery", "version"),
                ("schema_migrations_recovery", "checksum"),
            ]
            .into_iter()
            .map(|(table, column)| JsRow {
                columns: vec![
                    "table_name".into(),
                    "column_name".into(),
                    "character_set_name".into(),
                    "collation_name".into(),
                ],
                cells: vec![
                    text_cell(table),
                    text_cell(column),
                    text_cell("utf8mb4"),
                    text_cell("utf8mb4_bin"),
                ],
            })
            .collect();
        }
        Vec::new()
    }
}

impl VerbDispatch for MockDispatch {
    async fn dispatch(&self, req: JsRequest) -> VerbReply {
        self.log
            .borrow_mut()
            .push(format!("{}: {}", req.kind, req.sql));
        // DML verbs report an affected count; read verbs return rows. The engine's
        // `execute`/`executeTextParams` only branch on 0 vs >0, so report 1.
        let rows = self.rows_for(&req.sql);
        Ok(JsReply {
            rows,
            row_count: Some(1),
        })
    }
}

/// A single completed journal event row (unused here, since the journal read
/// returns empty so the migration is pending, but kept to document the canned
/// shape a host `pg` driver would return for that read). `event_seq` arrives as
/// an int cell because the column is `BIGINT GENERATED ALWAYS AS IDENTITY`.
#[allow(dead_code)]
fn canned_journal_row(version: &str, checksum: &str, event_seq: i64) -> JsRow {
    JsRow {
        columns: vec![
            "version".into(),
            "checksum".into(),
            "mig_kind".into(),
            "event_seq".into(),
            "phase".into(),
        ],
        cells: vec![
            text_cell(version),
            text_cell(checksum),
            text_cell("apply"),
            int_cell(event_seq),
            text_cell("completed"),
        ],
    }
}

fn text_cell(s: &str) -> JsCell {
    JsCell {
        kind: "text".into(),
        text: Some(s.to_string()),
        int: None,
        int_str: None,
        bool: None,
        text_array: None,
    }
}

fn int_cell(value: i64) -> JsCell {
    JsCell {
        kind: "int".into(),
        text: None,
        int: Some(value as f64),
        int_str: None,
        bool: None,
        text_array: None,
    }
}

/// Build a trivial additive migration (a `CREATE TABLE`) with a valid checksum.
fn trivial_migration() -> Migration {
    migration("mock_create", "CREATE TABLE mock_t (id int)")
}

fn migration(name: &str, up: &str) -> Migration {
    let flags = MigrationFlags::default();
    let version = MigrationId::generate();
    let checksum = Checksum::of(&ChecksumInput {
        up,
        down: None,
        flags: &flags,
        owner_app: "app_mock",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    Migration {
        version,
        name: name.into(),
        up: up.into(),
        down: None,
        checksum,
        flags,
        owner_app: "app_mock".into(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    }
}

fn update_step() -> (PlanStep, String) {
    let version = MigrationId::generate();
    let version_str = version.as_str().to_string();
    (
        PlanStep::Dml {
            version,
            checksum: Checksum::of(&ChecksumInput {
                up: "UPDATE mock_t SET label = $1 WHERE id = $2",
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "app_mock",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            name: "set_mock_label".into(),
            template: "UPDATE mock_t SET label = $1 WHERE id = $2".into(),
            binds: vec![BindValue::Text("ready".into()), BindValue::Int(1)],
            target_schema: "proj_mock".into(),
            target_table: "mock_t".into(),
            conflict_target: None,
            mutates_data: true,
            transactional: true,
            destructive: false,
            requires_approval: false,
            owner_app: "app_mock".into(),
        },
        version_str,
    )
}

#[test]
fn one_apply_runs_through_the_host_bridge_and_records_the_sql_sequence() {
    // A compio-free single-thread block_on is enough to drive the engine future
    // because the mock answers every verb inline.
    let outcome = futures::executor::block_on(async {
        let mock = MockDispatch::new();
        let session = NapiHostSession::new(mock);
        let cfg = ExecutorConfig::new("prj_mock", "proj_mock", support::no_inject("proj_mock"))
            .with_migrator_role("migrator_prj_mock");
        let migration = trivial_migration();
        let version_str = migration.version.as_str().to_string();

        let result = apply(
            &session,
            &cfg,
            std::slice::from_ref(&migration),
            Approval::None,
            "tester",
        )
        .await;

        // Pull the recorded log back out of the session's mock. `NapiHostSession`
        // owns the dispatch; re-borrow through it is not exposed, so we assert on
        // the outcome + re-run a fresh mock for the log inspection below.
        (result, version_str)
    });

    let (result, version_str) = outcome;
    let apply_outcome = result.expect("apply through the host bridge succeeds");

    // JOURNAL OUTCOME assertion: the pending migration WAS applied (its version is
    // in the applied list), and nothing was skipped/recovered.
    assert_eq!(
        apply_outcome.applied,
        vec![version_str],
        "the pending migration's version is journaled as applied"
    );
    assert!(
        apply_outcome.skipped.is_empty(),
        "nothing skipped: {:?}",
        apply_outcome.skipped
    );
}

#[test]
fn mysql_journal_only_status_uses_only_mysql_sql() {
    let (status, log) = futures::executor::block_on(async {
        let session = NapiHostSession::new(MockDispatch::new());
        let cfg = ExecutorConfig::new(
            "prj_mysql_status",
            "proj_mysql_status",
            support::no_inject("proj_mysql_status"),
        );
        let backend = zero_migrate::apply::backend::MysqlBackend::new_generic(&session);

        let status = zero_migrate::ops::status::status_via_backend(&backend, &cfg, &[])
            .await
            .expect("empty MySQL journal status succeeds through the host bridge")
            .expect_ready("the canned MySQL host driver grants the project lock");
        let log = session.into_dispatch().log.into_inner();
        (status, log)
    });

    assert!(status.applied.is_empty());
    assert!(status.pending.is_empty());
    assert!(
        log.iter()
            .any(|entry| entry.contains("performance_schema.events_transactions_current")),
        "MySQL status must use the MySQL journal bootstrap: {log:#?}"
    );
    assert!(
        log.iter().any(|entry| entry.contains("GET_LOCK(?, ?)")),
        "MySQL status must use MySQL named locks: {log:#?}"
    );
    assert!(
        log.iter()
            .any(|entry| entry.contains("COLLATE utf8mb4_bin")),
        "MySQL status must use the MySQL journal query: {log:#?}"
    );
    for postgres_only in [
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
        "pg_advisory",
        "pg_catalog",
        "CREATE SCHEMA IF NOT EXISTS",
        "$1",
    ] {
        assert!(
            log.iter().all(|entry| !entry.contains(postgres_only)),
            "MySQL status emitted PostgreSQL SQL {postgres_only:?}: {log:#?}"
        );
    }
}

#[test]
fn the_recorded_verb_sequence_has_the_expected_landmarks_in_order() {
    // Re-run with an accessible mock so we can inspect the exact recorded SQL.
    let log = futures::executor::block_on(async {
        let mock = MockDispatch::new();
        let session = NapiHostSession::new(mock);
        let cfg = ExecutorConfig::new("prj_mock", "proj_mock", support::no_inject("proj_mock"))
            .with_migrator_role("migrator_prj_mock");
        let migration = trivial_migration();

        apply(
            &session,
            &cfg,
            std::slice::from_ref(&migration),
            Approval::None,
            "tester",
        )
        .await
        .expect("apply succeeds");

        session.into_dispatch().log.into_inner()
    });

    // Structural landmarks — the exact SQL the executor emits, in order:
    let idx = |needle: &str| {
        log.iter()
            .position(|s| s.contains(needle))
            .unwrap_or_else(|| panic!("verb log is missing {needle:?}: {log:#?}"))
    };

    // 1. advisory lock acquire (project lock)
    let lock = idx("pg_advisory_lock");
    // 2. the confinement SET LOCAL (search_path + timeouts, the migrator bracket)
    let set_local = idx("SET LOCAL");
    // 3. the migration's own DDL
    let ddl = idx("CREATE TABLE mock_t");
    // 4. advisory unlock (release)
    let unlock = idx("pg_advisory_unlock");

    assert!(
        lock < set_local,
        "advisory lock is acquired before the confinement SET: lock@{lock} set@{set_local}"
    );
    assert!(
        set_local < ddl,
        "confinement SET precedes the migration DDL: set@{set_local} ddl@{ddl}"
    );
    assert!(
        ddl < unlock,
        "the migration DDL runs before the lock is released: ddl@{ddl} unlock@{unlock}"
    );

    // The journal write-back happened (a schema_migrations INSERT/execute).
    assert!(
        log.iter().any(|s| s.contains("schema_migrations")),
        "a journal write to schema_migrations was recorded: {log:#?}"
    );
}

#[test]
fn data_only_plan_executes_and_journals_through_the_host_bridge() {
    let (outcome, log, dml_version) = futures::executor::block_on(async {
        let mock = MockDispatch::new();
        let session = NapiHostSession::new(mock);
        let cfg = ExecutorConfig::new("prj_mock", "proj_mock", support::no_inject("proj_mock"))
            .with_migrator_role("migrator_prj_mock");
        let (step, dml_version) = update_step();
        let backend = PostgresBackend::new_generic(&session);

        let outcome = MigrationEngine::new()
            .apply_plan_with_touched_and_depends(
                &[step],
                &["mock_t".into()],
                &[],
                Approval::None,
                &backend,
                &cfg,
                "tester",
                LockMode::Acquire,
            )
            .await
            .expect("data-only plan succeeds through the host bridge");
        let log = session.into_dispatch().log.into_inner();
        (outcome, log, dml_version)
    });

    assert_eq!(outcome.applied.applied, vec![dml_version]);
    assert!(
        log.iter().any(|entry| {
            entry.starts_with("executeTextParams:")
                && entry.contains("UPDATE mock_t SET label = $1 WHERE id = $2")
        }),
        "the parameterized update must reach the host driver: {log:#?}"
    );
    assert!(
        log.iter()
            .any(|entry| entry.contains("INSERT INTO") && entry.contains("schema_migrations")),
        "the data step must be journaled: {log:#?}"
    );
}

#[test]
fn mixed_ddl_and_dml_plan_preserves_authored_execution_order() {
    let log = futures::executor::block_on(async {
        let mock = MockDispatch::new();
        let session = NapiHostSession::new(mock);
        let cfg = ExecutorConfig::new("prj_mock", "proj_mock", support::no_inject("proj_mock"))
            .with_migrator_role("migrator_prj_mock");
        let create = migration("create_mock", "CREATE TABLE mock_t (id int, label text)");
        let create_version = create.version.as_str().to_string();
        let (update, update_version) = update_step();
        let alter = migration(
            "add_mock_status",
            "ALTER TABLE mock_t ADD COLUMN status text",
        );
        let alter_version = alter.version.as_str().to_string();
        let steps = vec![PlanStep::Ddl(create), update, PlanStep::Ddl(alter)];
        let backend = PostgresBackend::new_generic(&session);

        let outcome = MigrationEngine::new()
            .apply_plan_with_touched_and_depends(
                &steps,
                &["mock_t".into()],
                &[],
                Approval::None,
                &backend,
                &cfg,
                "tester",
                LockMode::Acquire,
            )
            .await
            .expect("mixed plan succeeds through the host bridge");
        assert_eq!(
            outcome.applied.applied,
            vec![
                create_version.clone(),
                update_version.clone(),
                alter_version.clone()
            ]
        );

        session.into_dispatch().log.into_inner()
    });

    let position = |needle: &str| {
        log.iter()
            .position(|entry| entry.contains(needle))
            .unwrap_or_else(|| panic!("host log is missing {needle:?}: {log:#?}"))
    };
    let create = position("CREATE TABLE mock_t");
    let update = position("UPDATE mock_t SET label = $1 WHERE id = $2");
    let alter = position("ALTER TABLE mock_t ADD COLUMN status text");
    assert!(
        create < update && update < alter,
        "ordered plan must execute DDL, then DML, then DDL: {log:#?}"
    );
}
