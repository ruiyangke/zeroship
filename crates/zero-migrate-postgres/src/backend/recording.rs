//! The canned [`SqlSession`] this backend's tests drive, and the plan steps they
//! feed it.
//!
//! # Why this is a `feature`d module and not `#[cfg(test)]`
//!
//! Two suites need this one double, and they cannot be in the same crate.
//!
//! Most of what it proves is vendor-internal - which SQL this backend emits, in
//! which order, with which binds, and that the generic apply path is genuinely
//! driver-neutral - and that stays here, as unit tests beside the code. Five of them
//! additionally drive the ENGINE (`MigrationEngine::apply_plan_with_...`,
//! `AppliedPlan`, `ops::status::history_via_backend`), and those cannot live in a
//! vendor crate at all: `zero-migrate` depends on this crate, so the edge back is a
//! cycle Cargo refuses. They are integration tests OF THE ENGINE driving a
//! PostgreSQL backend, and they live in `zero-migrate/tests/pg_engine/`.
//!
//! A second copy of the recorder over there would be the real hazard: its canned
//! catalog and journal rows are the shared premise of both suites, and two copies
//! drift silently - one suite would go on asserting against a row shape the other
//! had already corrected. So there is ONE recorder, and the engine's test tree
//! reaches it through the `testing` feature, which `zero-migrate` turns on in its
//! `[dev-dependencies]` only. Resolver 3 keeps dev-dependency features out of the
//! normal build, so nothing here is compiled into a shipping
//! `zero-migrate-postgres`. `tests/dialect_matrix/a_recorder_never_ships.rs` is what
//! holds that line; this is the second recorder it covers.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use zero_migrate_backend::backfill::BackfillSpec;
use zero_migrate_backend::driver::{Bind, DbError, Row, SqlSession, Value};
use zero_migrate_backend::step::{BindValue, PlanStep};
use zero_migrate_ir::migration::{Checksum, ChecksumInput, MigrationFlags, MigrationId};

/// The host-shaped one-in-flight guard, mechanically enforced in the
/// driver rather than trusted by analogy. Every verb `compare_exchange(false,
/// true)`s on entry and clears via [`InFlightGuard`]'s `Drop` on the way out
/// (so error paths clear too). A second verb entered while the first's future
/// is still alive **panics** - turning "the engine issues one verb at a time"
/// from a claim into a checked invariant. On a real pinned host connection this
/// would otherwise deadlock (the second `tsfn.call` blocks on a socket the
/// first hasn't released); the panic surfaces the bug loudly instead.
///
/// This is the exact discipline the MySQL `JsDriverBackend` uses
/// (`transport.rs` `in_flight: bool`), lifted to `AtomicBool` because the seam
/// is `&self`, not `&mut self`.
#[derive(Debug)]
pub struct InFlightGuard<'a>(&'a AtomicBool);

impl<'a> InFlightGuard<'a> {
    /// Arm the guard on verb entry, panicking on re-entry.
    pub fn enter(flag: &'a AtomicBool) -> Self {
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            panic!(
                "SqlSession verb issued while another is in flight — the engine \
                 must be strictly one-verb-at-a-time"
            );
        }
        Self(flag)
    }
}

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        // Clear in the completion arm (RAII) so an error/early-return path also
        // releases - a leaked `true` would deadlock every later verb.
        self.0.store(false, Ordering::Release);
    }
}

/// A non-compio, host-SHAPED [`SqlSession`] that (a) records the SQL + binds of
/// every verb, (b) returns canned neutral rows for the read verbs, routed by a
/// substring match on the SQL so a full apply/introspection sweep decodes, and
/// (c) enforces the one-in-flight guard on every verb. This is NOT a
/// napi bridge - it is the in-crate host-shaped producer that
/// proves the generic PG apply path is genuinely driver-neutral, and converts
/// the one-in-flight invariant from by-analogy to mechanically-checked.
#[derive(Debug)]
pub struct RecordingSession {
    pub log: RefCell<Vec<String>>,
    pub binds: RefCell<Vec<Vec<Bind>>>,
    /// The mechanically-enforced one-verb-at-a-time guard.
    pub in_flight: AtomicBool,
    /// Canned rows the `net_applied` journal read returns (SQL-routed).
    canned_journal: RefCell<Vec<Row>>,
    /// Canned rows returned by the read-only backfill progress reader.
    canned_progress: RefCell<Vec<Row>>,
    progress_table_exists: bool,
    progress_checksum_exists: bool,
    server_version_num: i32,
}

impl Default for RecordingSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingSession {
    /// A recorder with an empty log, no canned rows, and a PostgreSQL server version
    /// new enough for every requirement gate the backend checks.
    #[must_use]
    pub fn new() -> Self {
        Self {
            log: RefCell::new(Vec::new()),
            binds: RefCell::new(Vec::new()),
            in_flight: AtomicBool::new(false),
            canned_journal: RefCell::new(Vec::new()),
            canned_progress: RefCell::new(Vec::new()),
            progress_table_exists: false,
            progress_checksum_exists: false,
            server_version_num: 180_000,
        }
    }

    pub fn with_server_version(server_version_num: i32) -> Self {
        Self {
            server_version_num,
            ..Self::new()
        }
    }

    pub fn with_canned_journal(rows: Vec<Row>) -> Self {
        let s = Self::new();
        *s.canned_journal.borrow_mut() = rows;
        s
    }

    pub fn with_canned_progress(rows: Vec<Row>, checksum_exists: bool) -> Self {
        let mut session = Self::new();
        *session.canned_progress.borrow_mut() = rows;
        session.progress_table_exists = true;
        session.progress_checksum_exists = checksum_exists;
        session
    }

    /// Route a read to its canned rows by SQL shape. ONLY the journal net-state
    /// read (`journal_sql::applied`, recognisable by its `union_all` CTE + the
    /// `schema_migrations_inflight` UNION leg - a shape no other query has) gets
    /// the canned (version, checksum, mig_kind, event_seq, phase) journal rows; every other
    /// read (catalog introspection in `snapshot_schema`, the `superseded_versions`
    /// squash read whose only column is `v`, drift probes) gets an EMPTY result,
    /// which yields an empty-but-valid decode - enough to drive every path
    /// end-to-end without feeding a wrong-shaped row into a decoder.
    fn rows_for(&self, sql: &str) -> Vec<Row> {
        if sql.contains("current_setting('server_version_num')") {
            vec![Row::new(
                vec!["server_version_num".into()],
                vec![Value::Text(self.server_version_num.to_string())],
            )]
        } else if sql.contains("union_all") && sql.contains("schema_migrations_inflight") {
            self.canned_journal.borrow().clone()
        } else if sql.contains("AS table_exists")
            && sql.contains("pg_catalog.pg_class")
            && sql.contains("schema_backfills")
        {
            vec![Row::new(
                vec!["table_exists".into()],
                vec![Value::Bool(self.progress_table_exists)],
            )]
        } else if sql.contains("AS table_exists") && sql.contains("pg_catalog.pg_attribute") {
            vec![Row::new(
                vec!["table_exists".into(), "checksum_exists".into()],
                vec![
                    Value::Bool(self.progress_table_exists),
                    Value::Bool(self.progress_checksum_exists),
                ],
            )]
        } else if sql.contains("schema_backfills")
            && sql.contains("backfill_id, checksum, complete")
        {
            self.canned_progress.borrow().clone()
        } else {
            Vec::new()
        }
    }
}

impl SqlSession for RecordingSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError> {
        let _g = InFlightGuard::enter(&self.in_flight);
        self.log.borrow_mut().push(format!("batch: {sql}"));
        Ok(())
    }
    async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
        let _g = InFlightGuard::enter(&self.in_flight);
        self.log.borrow_mut().push(format!("exec: {sql}"));
        self.binds.borrow_mut().push(params.to_vec());
        Ok(1)
    }
    async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
        let _g = InFlightGuard::enter(&self.in_flight);
        self.log.borrow_mut().push(format!("exec_text: {sql}"));
        Ok(1)
    }
    async fn query(&self, sql: &str, params: &[Bind]) -> Result<Vec<Row>, DbError> {
        let _g = InFlightGuard::enter(&self.in_flight);
        self.log.borrow_mut().push(format!("query: {sql}"));
        self.binds.borrow_mut().push(params.to_vec());
        Ok(self.rows_for(sql))
    }
    async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
        let _g = InFlightGuard::enter(&self.in_flight);
        self.log.borrow_mut().push(format!("query_one: {sql}"));
        self.binds.borrow_mut().push(params.to_vec());
        self.rows_for(sql)
            .into_iter()
            .next()
            .ok_or_else(|| DbError::message("query_one: no canned row"))
    }
}

/// A single completed journal event, shaped like the `applied()` CTE output:
/// (version, checksum, mig_kind, event_seq, phase) - exactly what a host `pg` driver would
/// return for that read.
pub fn canned_journal_row(version: &str, checksum: &str) -> Row {
    Row::new(
        vec![
            "version".to_string(),
            "checksum".to_string(),
            "mig_kind".to_string(),
            "event_seq".to_string(),
            "phase".to_string(),
            // The applied read now selects the stored reverse too, so a canned
            // row without the column is a row the reader cannot parse.
            "down".to_string(),
        ],
        vec![
            Value::Text(version.to_string()),
            Value::Text(checksum.to_string()),
            Value::Text("apply".to_string()),
            Value::Int(1),
            Value::Text("completed".to_string()),
            Value::Null,
        ],
    )
}

pub fn plan_dml_step(label: &str, destructive: bool) -> (PlanStep, MigrationId, Checksum) {
    let version = MigrationId::generate();
    let template = if destructive {
        "DELETE FROM users WHERE id = $1"
    } else {
        "UPDATE users SET ready = $1 WHERE id = $2"
    };
    let checksum = Checksum::of(&ChecksumInput {
        up: label,
        down: None,
        flags: &MigrationFlags::default(),
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    let binds = if destructive {
        vec![BindValue::Int(1)]
    } else {
        vec![BindValue::Bool(true), BindValue::Int(1)]
    };
    (
        PlanStep::Dml {
            version: version.clone(),
            checksum: checksum.clone(),
            name: label.to_string(),
            template: template.to_string(),
            binds,
            target_schema: "proj_x".into(),
            target_table: "users".into(),
            conflict_target: None,
            mutates_data: true,
            transactional: true,
            destructive,
            requires_approval: destructive,
            owner_app: "app_test".into(),
        },
        version,
        checksum,
    )
}

pub fn plan_backfill_step() -> (PlanStep, MigrationId, Checksum) {
    let version = MigrationId::generate();
    let checksum = Checksum::of(&ChecksumInput {
        up: "backfill users",
        down: None,
        flags: &MigrationFlags::default(),
        owner_app: "app_test",
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });
    (
        PlanStep::Backfill {
            version: version.clone(),
            checksum: checksum.clone(),
            spec: BackfillSpec {
                schema: "proj_x".into(),
                table: "users".into(),
                cursor_columns: vec!["id".into()],
                cursor_stability: zero_migrate_ir::ir::CursorStability::GuardUpdates,
                cursor_contract: None,
                batch_size: 100,
                set_clause: "ready = TRUE".into(),
                per_row: std::collections::BTreeMap::new(),
                filter: None,
                name: "backfill users".into(),
            },
        },
        version,
        checksum,
    )
}
