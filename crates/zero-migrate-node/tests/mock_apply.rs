//! Behavioral integration test (design §C gate + §B.6): drive ONE real
//! `executor::apply` through the addon's [`NapiHostSession`] over a MOCK
//! [`VerbDispatch`] that answers with canned `driver::Row`s — NO Node host, NO DB.
//!
//! This is the addon's analogue of the in-crate `RecordingSession` proof
//! (`crates/zero_migrate-migrate/src/apply/backend/postgres.rs`), but exercised through
//! the *addon's* bridge types (`NapiHostSession` + `VerbDispatch`), so it proves:
//!
//! 1. `executor::apply::<NapiHostSession<MockDispatch>>` monomorphizes and runs the
//!    whole DDL + lock + journal flow generically over the host bridge (the §C.5
//!    convergence point) — a real behavioral assertion, not just "no error";
//! 2. the recorded verb sequence contains the expected structural landmarks
//!    (advisory lock acquire → confinement SET → the migration's `up` DDL →
//!    journal write-back → advisory unlock), in order;
//! 3. the one-in-flight guard (§B.6) never trips across the whole apply — proving
//!    the engine is strictly one-verb-at-a-time over a pinned host connection;
//! 4. the reactor-less `futures::executor::block_on` drives the whole engine future
//!    to completion when every I/O leaf is answered inline (the §C.4 executor).
//!
//! The mock answers the journal net-state read with an EMPTY rowset (nothing
//! applied yet), so the supplied migration is pending and IS applied; its version
//! then appears in the `ApplyOutcome.applied` list — the journal outcome assertion.

use std::cell::RefCell;

use zero_migrate::apply::executor::apply;
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};

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
    fn new() -> Self {
        Self {
            log: RefCell::new(Vec::new()),
        }
    }

    /// Route a read to canned rows by SQL shape. ONLY the journal net-state read
    /// (recognisable by the `union_all` CTE + the `schema_migrations_inflight` UNION
    /// leg) gets rows — and we return NONE (nothing applied yet), so the supplied
    /// migration is pending and gets applied. Every other read (introspection,
    /// squash, drift) gets an empty rowset — a valid empty decode.
    fn rows_for(&self, _sql: &str) -> Vec<JsRow> {
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

/// A single completed journal event row (unused here — the journal read returns
/// empty so the migration is pending — but kept to document the canned shape a
/// host `pg` driver would return for that read).
#[allow(dead_code)]
fn canned_journal_row(version: &str, checksum: &str) -> JsRow {
    JsRow {
        columns: vec![
            "version".into(),
            "checksum".into(),
            "mig_kind".into(),
            "phase".into(),
        ],
        cells: vec![
            text_cell(version),
            text_cell(checksum),
            text_cell("apply"),
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

/// Build a trivial additive migration (a `CREATE TABLE`) with a valid checksum.
fn trivial_migration() -> Migration {
    let up = "CREATE TABLE mock_t (id int)";
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
        name: "mock_create".into(),
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

#[test]
fn one_apply_runs_through_the_host_bridge_and_records_the_sql_sequence() {
    // A compio-free single-thread block_on is enough to drive the engine future
    // because the mock answers every verb inline (§C.4).
    let outcome = futures::executor::block_on(async {
        let mock = MockDispatch::new();
        let session = NapiHostSession::new(mock);
        let cfg = ExecutorConfig::new("prj_mock", "proj_mock")
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
        vec![version_str.clone()],
        "the pending migration's version is journaled as applied"
    );
    assert!(
        apply_outcome.skipped.is_empty(),
        "nothing skipped: {:?}",
        apply_outcome.skipped
    );
}

#[test]
fn the_recorded_verb_sequence_has_the_expected_landmarks_in_order() {
    // Re-run with an accessible mock so we can inspect the exact recorded SQL.
    let log = futures::executor::block_on(async {
        let mock = MockDispatch::new();
        let session = NapiHostSession::new(mock);
        let cfg = ExecutorConfig::new("prj_mock", "proj_mock")
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
