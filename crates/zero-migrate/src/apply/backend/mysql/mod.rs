//! MySQL [`MigrationBackend`](crate::apply::backend::MigrationBackend)
//! implementation (redesign step 4d).
//!
//! Generic over the dialect-neutral [`SqlSession`](crate::driver::SqlSession) seam
//! (engine root `crate::driver`) — a host driver (the napi `mysql2` shell) supplies
//! the `SqlSession` impl, exactly as the `pg` shell does for
//! [`PostgresBackend`](crate::apply::backend::PostgresBackend). MySQL rides the
//! SAME seam as Postgres; only the dialect SQL (lock, session, journal DDL,
//! placeholders) differs, and all of it lives here + in [`session`] / [`journal_sql`]
//! — never in the shared executor (the C1 structural fix that lets MySQL ride the
//! same seam).
//!
//! **MySQL DDL is auto-committing** (an implicit COMMIT brackets every DDL
//! statement), so a migration's `up` cannot commit atomically with its journal
//! row. Every MySQL migration therefore takes the **two-phase non-transactional
//! path** — `ddl_is_transactional()` returns `false`, which routes all versioned
//! migrations through the started-marker → run-`up` → completed-row protocol (the
//! generalization of Postgres' `CREATE INDEX CONCURRENTLY` path to every MySQL
//! migration).
//!
//! # v1 capability surface (honest gaps)
//!
//! This first MySQL backend implements the **core apply path**: the `GET_LOCK`
//! project lock, MySQL journal DDL + net-state reads + event writes, the two-phase
//! apply, rollback, session setup, and the (dialect-agnostic) checksum-drift gate
//! over the MySQL journal read. The Postgres/SQLite-specific *advanced* paths —
//! live-schema introspection (`snapshot_schema`), online expand-contract
//! (`online`), shadow dry-run (`shadow`), the SQLite table rebuild (`rebuild_one`),
//! batched backfill / parameterized-DML steps, preconditions, baseline/squash
//! journal-without-running, and the cross-deploy pending-contract partition — are
//! **fail-closed** on this v1 MySQL backend (they surface a clear
//! `ApplyError::Backend` / capability-absent `None` rather than a silent
//! mis-apply), exactly as the host-PG backend fails closed on its native-pg-only
//! online/backfill harness and SQLite fails closed on its capability gaps. A later
//! cut widens them behind live-MySQL tests.

pub(crate) mod journal_sql;
pub(crate) mod session;

use super::capability::{BackfillSpec, OnlineSchemaChange, ShadowDryRun};
use super::{CrossDeployObligations, MigrationBackend, PlaceholderStyle};
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{AppliedEntry, JournalError};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Migration, MigrationId};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::SqliteRebuildSpec;
use crate::render::step::BindValue;
use crate::schema::query::SqlDialect;
use crate::driver::SqlSession;

/// The generic MySQL [`MigrationBackend`] implementation.
///
/// Generic over the [`SqlSession`] driver seam. It carries no online/shadow
/// harness (`online()` / `shadow()` are always `None` — the honest v1 gap), and
/// the auto-committing MySQL DDL routes every migration through the two-phase
/// non-transactional apply path.
#[derive(Debug)]
pub struct MysqlBackend<'a, D: SqlSession> {
    conn: &'a D,
}

impl<'a, D: SqlSession> MysqlBackend<'a, D> {
    /// Wrap any [`SqlSession`] driver as the MySQL backend.
    #[must_use]
    pub fn new_generic(conn: &'a D) -> Self {
        Self { conn }
    }
}

impl<D: SqlSession> MigrationBackend for MysqlBackend<'_, D> {
    type SessionSnapshot = ();

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mysql
    }

    fn placeholder_style(&self) -> PlaceholderStyle {
        PlaceholderStyle::Question
    }

    /// MySQL DDL is auto-committing: an implicit COMMIT brackets every DDL
    /// statement, so a migration's `up` can NEVER commit atomically with its
    /// journal row. This forces every MySQL migration onto the two-phase
    /// non-transactional apply path (`uses_two_phase_path` returns true for every
    /// versioned migration).
    fn ddl_is_transactional(&self) -> bool {
        false
    }

    async fn acquire_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        session::acquire_project_lock(self.conn, &cfg.project_id).await
    }

    async fn release_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        session::release_project_lock(self.conn, &cfg.project_id).await
    }

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        // MySQL session budgets are re-applied per migration in `apply_one`
        // (`configure_session`); there is no session-wide GUC to snapshot/restore
        // the way Postgres restores `search_path` / `statement_timeout`. The
        // budgets are session-scoped SETs that are harmless to leave, and every
        // apply re-sets them, so the snapshot is the unit value.
        Ok(())
    }

    async fn restore_session(&self, _snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn reset_role_best_effort(&self) {
        // MySQL confinement is the connecting user's grants, not a `SET ROLE`
        // switch bracketing the `up` (the PG least-privilege migrator-role model),
        // so there is no per-apply role to reset. No-op.
    }

    async fn apply_one(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
        had_inflight: bool,
        supersedes: &[&str],
        kind: &str,
    ) -> Result<bool, ApplyError> {
        // A repeatable rides the same two-phase path on MySQL (there is no
        // transactional fast path — DDL auto-commits), so `kind` only informs the
        // journaled stamp, which `apply_two_phase` derives from `supersedes` for
        // the squash case; a plain apply/repeatable stamps `apply`. Configure the
        // per-migration session budgets first.
        let _ = kind;
        session::configure_session(self.conn, cfg, m).await?;
        session::apply_two_phase(self.conn, cfg, m, applied_by, had_inflight, supersedes).await
    }

    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        // "Transactional" is the trait's name for the rollback entry; on MySQL the
        // `down` auto-commits and the `rolled_back` append is ordered after it (the
        // same two-phase reality as apply). The trait method name is kept for a
        // single generic rollback caller.
        session::rollback_one(self.conn, cfg, m, applied_by).await
    }

    fn validate_non_txn(&self, _m: &Migration) -> Result<(), ApplyError> {
        // The PG non-txn idempotency scan (forbid bare DML, require IF NOT EXISTS on
        // CONCURRENTLY) is Postgres-specific (`pg_query`-parsed). On MySQL every
        // migration is two-phase by construction (auto-commit DDL), and MySQL has no
        // `CREATE INDEX CONCURRENTLY` INVALID-residue hazard; author-side idempotence
        // (`IF (NOT) EXISTS`) is the crash-recovery contract. A dedicated MySQL
        // non-txn idempotency scan is a later cut; v1 accepts the `up` here.
        Ok(())
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        journal_sql::ensure_journal(self.conn, cfg).await
    }

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal_sql::applied(self.conn, cfg).await
    }

    async fn superseded_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal_sql::superseded_versions(self.conn, cfg).await
    }

    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<std::collections::HashMap<String, String>, JournalError> {
        journal_sql::latest_completed_checksums(self.conn, cfg).await
    }

    async fn check_checksum_drift(
        &self,
        cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<crate::apply::drift::ChecksumDriftReport, DriftError> {
        // The drift/tamper comparison is dialect-agnostic
        // (`compare_applied_to_set`); only the journal read underneath is
        // dialect-coupled. Read the net-applied state through the MySQL journal and
        // run the SAME shared comparison the PG/SQLite backends use, so the
        // repeatable-exemption / kind-mismatch / tamper rules never diverge across
        // dialects.
        let applied = journal_sql::applied(self.conn, cfg).await?;
        Ok(crate::apply::drift::compare_applied_to_set(&applied, migrations))
    }

    async fn snapshot_schema(&self, _cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        // Live-schema introspection (`information_schema` catalog read → a neutral
        // `SchemaSnapshot`) is the deepest MySQL surface; the declarative diff /
        // structural-drift consumers are not wired for MySQL in v1. Fail closed with
        // a clear message rather than return a fake empty snapshot that a differ
        // would misread as "no tables exist" (a silent destructive plan). A later
        // cut adds the MySQL catalog reader behind live-MySQL tests.
        Err(DriftError::Backend(
            "mysql backend: live-schema introspection (snapshot_schema) is not yet \
             implemented — structural drift / declarative diff is unsupported on MySQL in v1"
                .to_string(),
        ))
    }

    async fn evaluate_preconditions(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<crate::apply::executor::PreconditionVerdict, ApplyError> {
        // The executor calls this for EVERY migration, precondition-bearing or not.
        // A migration with NO preconditions needs no evaluator at all — evaluating an
        // empty list is `AllMet` by construction (exactly what the PG evaluator's
        // `evaluate_all` returns for an empty `m.preconditions`), so it must apply
        // normally on MySQL rather than trip the v1 capability gap.
        if m.preconditions.is_empty() {
            return Ok(crate::apply::executor::PreconditionVerdict::AllMet);
        }
        // A GENUINE precondition (boolean-SELECT probes gated by the `pg_query`
        // parser + PG-flavoured catalog reads) has no MySQL-native evaluator yet, so
        // a MySQL migration that DECLARES preconditions is refused (fail closed)
        // rather than silently treated as satisfied. A later cut adds the MySQL
        // evaluator behind live-MySQL tests.
        Err(ApplyError::Backend(
            "mysql backend: precondition evaluation is not yet implemented on MySQL in v1"
                .to_string(),
        ))
    }

    async fn record_squash(
        &self,
        _cfg: &ExecutorConfig,
        _squash_migration: &Migration,
        _applied_by: &str,
        _supersedes: &[&str],
    ) -> Result<(), ApplyError> {
        // The existing-DB squash path journals a `squash` event WITHOUT running its
        // `up` (baseline-style). That records-not-run primitive is not wired for
        // MySQL in v1 (it shares the baseline machinery below). The FRESH-path
        // squash — where the squash's `up` DOES run — is handled inline by
        // `apply_two_phase` (non-empty `supersedes`), so this refusal is only the
        // existing-DB record-not-run variant.
        Err(ApplyError::Backend(
            "mysql backend: existing-DB squash (record-not-run) is not yet implemented on \
             MySQL in v1"
                .to_string(),
        ))
    }

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        _m: &Migration,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
    ) -> Result<(), ApplyError> {
        // A SQLite 12-step table rebuild reaching the MySQL backend is a routing bug
        // (only the SQLite differ produces rebuilds), surfaced as a clear error.
        Err(ApplyError::Backend(format!(
            "mysql backend: SQLite table rebuild requested for '{}' — only the SQLite \
             differ produces rebuilds (routing bug)",
            spec.table
        )))
    }

    async fn run_backfill_step(
        &self,
        _cfg: &ExecutorConfig,
        spec: &BackfillSpec,
        _approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        // The batched-backfill executor (windowed UPDATE, resumable cursor) is
        // PG/SQLite-specific; the MySQL batched backfill is a later cut. Fail closed
        // rather than silently skip the data transform.
        Err(ApplyError::Backend(format!(
            "mysql backend: batched backfill step '{}' is not yet implemented on MySQL in v1",
            spec.name
        )))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_dml_step(
        &self,
        _cfg: &ExecutorConfig,
        _version: &MigrationId,
        name: &str,
        _template: &str,
        _binds: &[BindValue],
        _destructive: bool,
        _owner_app: &str,
        _approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        // The parameterized-DML step executor (op.* §2.3.2) is a later MySQL cut.
        // Fail closed rather than silently skip the DML.
        Err(ApplyError::Backend(format!(
            "mysql backend: parameterized DML step '{name}' is not yet implemented on \
             MySQL in v1"
        )))
    }

    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        // No online expand-contract harness on MySQL in v1 (the differ emits no
        // renames for MySQL, so `renames` is empty — no online path is reached).
        None
    }

    fn shadow(&self) -> Option<&dyn ShadowDryRun> {
        // No shadow dry-run harness on MySQL in v1 — `dry_run` surfaces
        // `DryRunError::ShadowUnsupported` (the honest gap).
        None
    }

    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations> {
        // No cross-deploy pending-contract partition on MySQL in v1 (the online
        // rename that opens obligations is unsupported here). `None` ⇒ reads are
        // empty and writes are no-ops by construction.
        None
    }

    async fn baseline_one(
        &self,
        _cfg: &ExecutorConfig,
        _m: &Migration,
        _applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError> {
        // Adoption baseline (record the live schema as the project baseline WITHOUT
        // running the `up`) shares the guard + advisory-lock + record-not-run
        // machinery that is PG-specific; the MySQL baseline is a later cut.
        Err(BaselineError::Backend(
            "mysql backend: schema baseline/adoption is not yet implemented on MySQL in v1"
                .to_string(),
        ))
    }
}

/// UNIT / render tests for the MySQL backend (redesign step 4d).
///
/// These assert the **generated MySQL SQL** — `GET_LOCK`/`RELEASE_LOCK` for the
/// project lock, MySQL journal DDL (`AUTO_INCREMENT`, `CURRENT_TIMESTAMP(6)`,
/// `CREATE DATABASE`, InnoDB, SIGNAL-based immutability triggers), and the `?`
/// placeholder style on every journaled write — **WITHOUT a live MySQL server**.
/// A host-shaped [`RecordingSession`] records the SQL + binds of every verb and
/// returns canned rows for the reads (`GET_LOCK → 1`, trigger-existence → empty),
/// so a full lock + `ensure_journal` + apply sweep runs generically over a
/// non-compio driver and every emitted statement is inspected. The live-MySQL e2e
/// is the separate cut 4e.
#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::model::migration::{Checksum, MigrationFlags};
    use crate::driver::{Bind, DbError, Row, Value};
    use std::cell::RefCell;

    /// A non-compio, host-shaped [`SqlSession`] that records the SQL + binds of
    /// every verb and returns canned rows for the reads the MySQL apply path issues:
    /// `GET_LOCK(...)` → a single `got=1` row (lock acquired); the
    /// `information_schema.triggers` existence probe → empty (so `ensure_journal`
    /// creates every trigger); the journal net-state reads → empty. This is the
    /// MySQL analogue of the PG backend's in-crate `RecordingSession` genericity
    /// proof.
    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
            }
        }

        /// Route a read to its canned rows by SQL shape. `GET_LOCK` returns a
        /// single `got=1` row; everything else (trigger-existence probe, journal
        /// net-state reads) returns empty — enough to drive the whole apply/journal
        /// sweep end-to-end without a live server.
        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("GET_LOCK") {
                vec![Row::new(vec!["got".to_string()], vec![Value::Int(1)])]
            } else {
                Vec::new()
            }
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            Ok(())
        }
        async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            Ok(1)
        }
        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(1)
        }
        async fn query(&self, sql: &str, params: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            Ok(self.rows_for(sql))
        }
        async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            self.rows_for(sql)
                .into_iter()
                .next()
                .ok_or_else(|| DbError::message("query_one: no canned row"))
        }
    }

    fn trivial_migration() -> Migration {
        let flags = MigrationFlags::default();
        let version = MigrationId::generate();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: "CREATE TABLE t (id INT)",
            down: Some("DROP TABLE t"),
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "create_t".into(),
            up: "CREATE TABLE t (id INT)".into(),
            down: Some("DROP TABLE t".into()),
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }

    /// The backend reports the MySQL dialect, the `?` placeholder style, and
    /// non-transactional DDL (auto-commit ⇒ two-phase path for every migration).
    #[test]
    fn backend_reports_mysql_dialect_and_question_placeholders() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        assert_eq!(backend.dialect(), SqlDialect::Mysql);
        assert_eq!(backend.placeholder_style(), PlaceholderStyle::Question);
        assert!(
            !backend.ddl_is_transactional(),
            "MySQL DDL auto-commits — must report non-transactional (two-phase path)"
        );
        // The `?` placeholder renders positionally regardless of index.
        assert_eq!(backend.placeholder(1), "?");
        assert_eq!(backend.placeholder(7), "?");
    }

    /// `project_lock_name` derives a `zero_migrate:`-prefixed lock name and folds a
    /// too-long id to a deterministic 64-char SHA-256 hex (MySQL's lock-name cap).
    #[test]
    fn project_lock_name_prefixes_and_folds_to_64_chars() {
        // Short id → prefixed verbatim.
        assert_eq!(
            session::project_lock_name("prj_abc"),
            "zero_migrate:prj_abc"
        );
        // A >64-char id folds to a stable 64-hex-char name; deterministic.
        let long = "prj_".to_string() + &"x".repeat(200);
        let folded = session::project_lock_name(&long);
        assert_eq!(folded.len(), 64, "folded lock name is exactly 64 chars");
        assert_eq!(
            folded,
            session::project_lock_name(&long),
            "the fold is deterministic"
        );
        assert!(
            folded.bytes().all(|b| b.is_ascii_hexdigit()),
            "the folded name is hex"
        );
    }

    /// `quote_ident_mysql` backtick-quotes, doubles embedded backticks, and
    /// fail-closes on empty / NUL identifiers.
    #[test]
    fn quote_ident_mysql_backticks_and_fails_closed() {
        assert_eq!(journal_sql::quote_ident_mysql("t").unwrap(), "`t`");
        assert_eq!(
            journal_sql::quote_ident_mysql("we`ird").unwrap(),
            "`we``ird`"
        );
        assert!(journal_sql::quote_ident_mysql("").is_err());
        assert!(journal_sql::quote_ident_mysql("a\0b").is_err());
    }

    /// The project lock rides `GET_LOCK(?, ?)` with the derived name bound (not
    /// interpolated); release rides `RELEASE_LOCK(?)`.
    #[compio::test]
    async fn project_lock_uses_get_lock_release_lock_with_bound_name() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        backend.acquire_project_lock(&cfg).await.expect("acquire GET_LOCK");
        backend.release_project_lock(&cfg).await.expect("release RELEASE_LOCK");

        let log = rec.log.borrow();
        assert!(
            log.iter().any(|s| s.contains("GET_LOCK(?, ?)")),
            "acquire issues GET_LOCK with ? placeholders (not pg_advisory_lock): {log:?}"
        );
        assert!(
            log.iter().any(|s| s.contains("RELEASE_LOCK(?)")),
            "release issues RELEASE_LOCK with a ? placeholder: {log:?}"
        );
        assert!(
            !log.iter().any(|s| s.contains("pg_advisory")),
            "no Postgres advisory-lock SQL leaks into the MySQL backend: {log:?}"
        );
        // The derived lock name crossed the seam as a bound Text, never interpolated.
        assert!(
            rec.binds
                .borrow()
                .iter()
                .any(|b| b.iter().any(|v| matches!(v, Bind::Text(t) if t == "zero_migrate:prj_x"))),
            "the project lock name is bound, not interpolated: {:?}",
            rec.binds.borrow()
        );
    }

    /// `ensure_journal` emits the MySQL journal DDL: `CREATE DATABASE IF NOT
    /// EXISTS`, the `schema_migrations` table with `BIGINT AUTO_INCREMENT PRIMARY
    /// KEY` + `TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6)` + `ENGINE=InnoDB`, the
    /// `_supersedes` + `_inflight` tables, and `SIGNAL SQLSTATE '45000'`
    /// immutability triggers — never any Postgres-flavoured `GENERATED ALWAYS AS
    /// IDENTITY` / `TIMESTAMPTZ` / plpgsql.
    #[compio::test]
    async fn ensure_journal_emits_mysql_dialect_ddl() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        backend.ensure_journal(&cfg).await.expect("ensure_journal DDL");

        let log = rec.log.borrow();
        let all = log.join("\n");
        assert!(
            all.contains("CREATE DATABASE IF NOT EXISTS `proj_x_migrations`"),
            "meta database DDL is CREATE DATABASE (MySQL), backtick-quoted: {all}"
        );
        assert!(
            all.contains("schema_migrations")
                && all.contains("BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY"),
            "the journal PK is BIGINT AUTO_INCREMENT (MySQL native total order): {all}"
        );
        assert!(
            all.contains("TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)"),
            "timestamps are MySQL TIMESTAMP(6)/CURRENT_TIMESTAMP(6): {all}"
        );
        assert!(all.contains("ENGINE=InnoDB"), "tables are InnoDB: {all}");
        assert!(
            all.contains("schema_migrations_supersedes")
                && all.contains("schema_migrations_inflight"),
            "the supersedes + inflight side-tables are created: {all}"
        );
        assert!(
            all.contains("SIGNAL SQLSTATE '45000'"),
            "immutability triggers SIGNAL SQLSTATE '45000' (MySQL), not plpgsql RAISE: {all}"
        );
        assert!(
            all.contains("BEFORE UPDATE") && all.contains("BEFORE DELETE"),
            "both BEFORE UPDATE and BEFORE DELETE immutability triggers are created: {all}"
        );
        // No Postgres dialect leaks.
        assert!(
            !all.contains("GENERATED ALWAYS AS IDENTITY")
                && !all.contains("TIMESTAMPTZ")
                && !all.contains("plpgsql")
                && !all.contains("CREATE SCHEMA"),
            "no Postgres-flavoured DDL leaks into the MySQL journal: {all}"
        );
        // The trigger-existence probe ran (guarding re-bootstrap idempotency).
        assert!(
            all.contains("information_schema.triggers"),
            "trigger creation is guarded on information_schema.triggers: {all}"
        );
    }

    /// A full two-phase apply of one migration through the backend records the
    /// journal writes with `?` placeholders (never `$N`): the `started`-marker
    /// `INSERT IGNORE ... VALUES (?, ?, ?, ?)`, the creator `up` as a `batch`, the
    /// `completed` `INSERT ... VALUES ('applied', ?, ?, ?, ?, ?, 'completed',
    /// 'success', ?)`, and the inflight `DELETE ... WHERE version = ?`.
    #[compio::test]
    async fn two_phase_apply_writes_journal_with_question_placeholders() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");
        let m = trivial_migration();

        let recovered = backend
            .apply_one(&cfg, &m, "tester", false, &[], "apply")
            .await
            .expect("two-phase apply runs");
        assert!(!recovered, "a fresh apply recovered no inflight marker");

        let log = rec.log.borrow();
        let all = log.join("\n");
        // Session budgets set with MySQL session vars (no search_path / SET ROLE).
        assert!(
            all.contains("SET SESSION max_execution_time")
                && all.contains("innodb_lock_wait_timeout"),
            "MySQL session budgets are max_execution_time + innodb_lock_wait_timeout: {all}"
        );
        assert!(
            !all.contains("search_path") && !all.contains("SET LOCAL ROLE"),
            "no Postgres search_path / SET ROLE on the MySQL apply path: {all}"
        );
        // The started marker uses INSERT IGNORE + ? placeholders.
        assert!(
            all.contains("INSERT IGNORE INTO `proj_x_migrations`.schema_migrations_inflight")
                && all.contains("VALUES (?, ?, ?, ?)"),
            "started marker is INSERT IGNORE with ? placeholders: {all}"
        );
        // The creator up ran as a batch.
        assert!(
            log.iter().any(|s| s == "batch: CREATE TABLE t (id INT)"),
            "the creator up ran through batch: {log:?}"
        );
        // The completed row is an INSERT with the applied/completed/success literals
        // and ? placeholders.
        assert!(
            all.contains("INSERT INTO `proj_x_migrations`.schema_migrations")
                && all.contains("'applied', ?, ?, ?, ?, ?, 'completed', 'success', ?"),
            "completed row is an INSERT with ? placeholders: {all}"
        );
        // The inflight marker is cleared with a ? placeholder.
        assert!(
            all.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight WHERE version = ?"),
            "inflight clear is a DELETE ... WHERE version = ?: {all}"
        );
        // No Postgres `$N` placeholders anywhere on the MySQL write path.
        assert!(
            !all.contains("$1") && !all.contains("$2") && !all.contains("$3"),
            "no Postgres $N placeholders leak onto the MySQL apply path: {all}"
        );
    }

    /// Rollback runs the `down` as a batch then appends a `rolled_back` event with
    /// `?` placeholders and the NULL applied-only columns (only the 5 event fields).
    #[compio::test]
    async fn rollback_runs_down_then_appends_rolled_back_event() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");
        let m = trivial_migration();

        backend
            .rollback_one_transactional(&cfg, &m, "tester")
            .await
            .expect("rollback runs");

        let log = rec.log.borrow();
        let all = log.join("\n");
        assert!(
            log.iter().any(|s| s == "batch: DROP TABLE t"),
            "the down ran through batch: {log:?}"
        );
        assert!(
            all.contains("INSERT INTO `proj_x_migrations`.schema_migrations")
                && all.contains("'rolled_back', ?, ?, ?, ?, ?")
                && all.contains("(event_kind, version, name, checksum, `by`, exec_ms)"),
            "rolled_back event appends only the 5 event fields (applied-only cols NULL): {all}"
        );
    }

    /// The capability gaps are fail-closed, not silent: introspection, preconditions,
    /// backfill, DML-step, rebuild, baseline, and squash-record-not-run all surface a
    /// clear error; online/shadow/pending-contracts report the capability absent.
    #[compio::test]
    async fn v1_capability_gaps_fail_closed() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");
        let m = trivial_migration();

        assert!(
            backend.snapshot_schema(&cfg).await.is_err(),
            "snapshot_schema fails closed (no fake empty snapshot)"
        );
        // A migration with NO preconditions applies normally (empty list ⇒ AllMet,
        // no evaluator needed — the executor calls this for every migration).
        assert_eq!(
            backend.evaluate_preconditions(&cfg, &m).await.unwrap(),
            crate::apply::executor::PreconditionVerdict::AllMet,
            "an empty precondition list is AllMet, not the v1 capability gap",
        );
        // A migration that DECLARES a precondition fails closed (no MySQL evaluator).
        let mut m_pc = trivial_migration();
        m_pc.preconditions = vec![crate::model::precondition::PreconditionCheck::halt(
            crate::model::precondition::Precondition::TableNotExists { table: "t".into() },
        )];
        assert!(
            backend.evaluate_preconditions(&cfg, &m_pc).await.is_err(),
            "a DECLARED precondition fails closed on MySQL v1",
        );
        assert!(backend.baseline_one(&cfg, &m, "t").await.is_err());
        assert!(backend.record_squash(&cfg, &m, "t", &["v1"]).await.is_err());
        assert!(backend.online().is_none(), "no online harness in v1");
        assert!(backend.shadow().is_none(), "no shadow harness in v1");
        assert!(
            backend.pending_contracts().is_none(),
            "no cross-deploy pending-contract partition in v1"
        );
    }

    /// The checksum-drift gate runs the SAME dialect-agnostic comparison the PG /
    /// SQLite backends use, over the MySQL journal read: an empty journal yields no
    /// drift and no orphans for a fresh set.
    #[compio::test]
    async fn checksum_drift_gate_runs_over_mysql_journal_read() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");
        let m = trivial_migration();

        let report = backend
            .check_checksum_drift(&cfg, std::slice::from_ref(&m))
            .await
            .expect("drift check runs over the MySQL journal read");
        assert!(
            report.checksum_drift.is_empty() && report.orphan_journal.is_empty(),
            "empty MySQL journal ⇒ no drift, no orphans for a fresh set: {report:?}"
        );
        // The read used a window-function net-state query (MySQL 8), not PG DISTINCT ON.
        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("ROW_NUMBER() OVER (PARTITION BY version ORDER BY event_seq DESC)"),
            "the MySQL net-state read uses a window function, not DISTINCT ON: {all}"
        );
    }
}
