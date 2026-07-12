//! Postgres [`MigrationBackend`](super::MigrationBackend) implementation.
//!
//! Generic over the dialect-neutral [`SqlSession`](crate::driver::SqlSession) seam
//! (engine root `crate::driver`) — a host driver (the napi `pg` shell) supplies the
//! `SqlSession` impl. SQLite does NOT ride this seam (it is an in-process rusqlite
//! actor).

use crate::driver::SqlSession;

/// The Postgres dialect SQL leaves (session/lock/txn/journal/DML/rollback) this
/// backend drives — relocated out of the generic `apply::executor` so no
/// dialect-specific SQL lives in the shared executor (C1).
pub(crate) mod session;

use super::{
    CrossDeployObligations, JournalFuture, MigrationBackend, PgSessionSnapshot,
};
use super::capability::{BackfillSpec, OnlineSchemaChange, ShadowDryRun};
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, AppliedEntry, JournalError};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Migration, MigrationId};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::SqliteRebuildSpec;
use crate::render::step::BindValue;
use crate::schema::query::SqlDialect;

/// The generic Postgres [`MigrationBackend`] implementation on the host-pg build.
///
/// Generic over the [`SqlSession`] driver seam. It carries no compio-concrete
/// `PgOnline`/`PgShadow` harness, so `online()` / `shadow()` always report `None`
/// — the honest v1 gap (`DryRunError::ShadowUnsupported` on the host path). The
/// write/DDL/journal apply path never touches them.
#[cfg(pg_seam)]
#[derive(Debug)]
pub struct PostgresBackend<'a, D: SqlSession> {
    conn: &'a D,
}

#[cfg(pg_seam)]
impl<'a, D: SqlSession> PostgresBackend<'a, D> {
    /// Wrap any [`SqlSession`] driver as the Postgres backend (host-pg build — no
    /// compio-concrete online/shadow harness exists to attach).
    #[must_use]
    pub fn new_generic(conn: &'a D) -> Self {
        Self { conn }
    }
}

impl<D: SqlSession> MigrationBackend for PostgresBackend<'_, D> {
    type SessionSnapshot = PgSessionSnapshot;

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn ddl_is_transactional(&self) -> bool {
        true
    }

    async fn acquire_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        session::acquire_project_lock(self.conn, &cfg.project_id).await
    }

    async fn release_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        session::release_project_lock(self.conn, &cfg.project_id).await
    }

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        session::snapshot_session(self.conn).await
    }

    async fn restore_session(&self, snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        session::restore_session(self.conn, snap).await
    }

    async fn reset_role_best_effort(&self) {
        if let Err(e) = self.conn.batch("RESET ROLE").await {
            tracing::warn!(error = %e, "zero-migrate: failed to RESET ROLE after apply (L1)");
        }
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
        if kind != "repeatable" && self.uses_two_phase_path(m) {
            session::configure_session_non_txn(self.conn, cfg, m).await?;
            session::apply_non_transactional(
                self.conn,
                cfg,
                m,
                applied_by,
                had_inflight,
                supersedes,
            )
            .await
        } else {
            session::apply_transactional(
                self.conn, cfg, m, applied_by, supersedes, kind,
            )
            .await?;
            Ok(false)
        }
    }

    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        session::rollback_one_transactional(self.conn, cfg, m, applied_by).await
    }

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        session::validate_non_txn_idempotent(m)
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        journal::ensure_journal(self.conn, cfg).await
    }

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal::applied(self.conn, cfg).await
    }

    async fn superseded_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal::superseded_versions(self.conn, cfg).await
    }

    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<std::collections::HashMap<String, String>, JournalError> {
        journal::latest_completed_checksums(self.conn, cfg).await
    }

    async fn check_checksum_drift(
        &self,
        cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<crate::apply::drift::ChecksumDriftReport, DriftError> {
        crate::apply::drift::check_checksum_drift(self.conn, cfg, migrations).await
    }

    async fn snapshot_schema(&self, cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        crate::apply::drift::snapshot_schema(self.conn, &cfg.project_schema).await
    }

    async fn evaluate_preconditions(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<crate::apply::executor::PreconditionVerdict, ApplyError> {
        crate::apply::precondition::evaluate_all(self.conn, cfg, m).await
    }

    async fn record_squash(
        &self,
        cfg: &ExecutorConfig,
        squash_migration: &Migration,
        applied_by: &str,
        supersedes: &[&str],
    ) -> Result<(), ApplyError> {
        crate::apply::journal::record_baseline(
            self.conn,
            cfg,
            crate::apply::journal::BaselineRecord {
                version: squash_migration.version.as_str(),
                name: &squash_migration.name,
                checksum: squash_migration.checksum.as_str(),
                applied_by,
                kind: "squash",
                supersedes,
            },
        )
        .await
        .map_err(ApplyError::Journal)
    }

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        _m: &Migration,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
    ) -> Result<(), ApplyError> {
        Err(ApplyError::Backend(format!(
            "postgres backend: SQLite table rebuild requested for '{}' — the PG differ \
             never produces rebuilds (routing bug)",
            spec.table
        )))
    }


    // Host-pg build: the online/backfill harness (`backfill::run_backfill`) is a
    // compio-concrete `native-pg`-only module, and the host addon's v1 facade does
    // not surface expand/online. A backfill step routed to the host PG backend is a
    // routing bug (the differ never emits one on the plain apply path), so refuse
    // it explicitly rather than pull the compio backfill runner into the addon.
    async fn run_backfill_step(
        &self,
        _cfg: &ExecutorConfig,
        spec: &BackfillSpec,
        _approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        _applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        Err(ApplyError::Backend(format!(
            "postgres host-pg backend: backfill step '{}' requested — the online/backfill \
             path is native-pg-only (not surfaced by the addon's v1 facade)",
            spec.name
        )))
    }

    async fn run_dml_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        name: &str,
        template: &str,
        binds: &[BindValue],
        destructive: bool,
        owner_app: &str,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        if destructive && approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if destructive && !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }
        let already = self
            .applied(cfg)
            .await
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|e| matches!(e.phase, crate::apply::journal::Phase::Completed))
            .any(|e| e.version == version.as_str());
        if already {
            return Ok(false);
        }
        session::apply_dml_transactional(
            self.conn,
            cfg,
            version.as_str(),
            name,
            template,
            binds,
            owner_app,
            applied_by,
        )
        .await?;
        Ok(true)
    }


    // Host-pg build: no compio-concrete online harness → always `None`.
    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        None
    }


    // Host-pg build: no compio-concrete shadow harness → always `None`, so
    // `dry_run`/`dry_run_declarative` return `DryRunError::ShadowUnsupported`
    // (the honest v1 gap — host-side shadow is sequenced later).
    fn shadow(&self) -> Option<&dyn ShadowDryRun> {
        None
    }

    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations> {
        Some(self)
    }

    async fn baseline_one(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError> {
        crate::apply::baseline::baseline(self.conn, cfg, m, applied_by).await
    }
}

impl<D: SqlSession> CrossDeployObligations for PostgresBackend<'_, D> {
    fn outstanding_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::PendingContract>> {
        Box::pin(async move { journal::outstanding_pending_contracts(self.conn, cfg).await })
    }

    fn record_pending_contract_with_recovery<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        rec: journal::PendingContractRecord<'a>,
        scope: Option<journal::DeployRecoveryScope<'a>>,
    ) -> JournalFuture<'a, ()> {
        Box::pin(async move {
            journal::record_pending_contract_with_recovery(self.conn, cfg, rec, scope).await
        })
    }

    fn resolve_pending_contract<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        pc: &'a journal::PendingContract,
        resolution: journal::Resolution,
        by: &'a str,
    ) -> JournalFuture<'a, ()> {
        Box::pin(async move {
            journal::resolve_pending_contract(self.conn, cfg, pc, resolution, by).await
        })
    }

    fn mark_deploy_recovery_committed_batch<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        deploy_id: &'a str,
        pending_versions: &'a [String],
        by: &'a str,
    ) -> JournalFuture<'a, ()> {
        Box::pin(async move {
            journal::mark_deploy_recovery_committed_batch(
                self.conn,
                cfg,
                deploy_id,
                pending_versions,
                by,
            )
            .await
        })
    }

    fn mark_deploy_recovery_reconciled<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        deploy_id: &'a str,
        pending_version: &'a str,
        by: &'a str,
    ) -> JournalFuture<'a, ()> {
        Box::pin(async move {
            journal::mark_deploy_recovery_reconciled(
                self.conn,
                cfg,
                deploy_id,
                pending_version,
                by,
            )
            .await
        })
    }

    fn outstanding_deploy_recoveries<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::DeployRecovery>> {
        Box::pin(async move { journal::outstanding_deploy_recoveries(self.conn, cfg).await })
    }
}

/// Genericity proof (design §6d + §A): the apply path monomorphizes over a
/// **non-compio** [`SqlSession`] driver. An in-crate recording driver records the
/// SQL of every WRITE verb, and — now that the read side is widened to the
/// driver-neutral [`Row`]/[`DbError`] (§A) — RETURNS canned `Row`s from
/// its read verbs. This proves `PostgresBackend<'a, D>` is genuinely generic AND
/// that a host driver can build return values without a `compio_postgres::Row`,
/// closing the old `unreachable!("read verbs…")` gap.
#[cfg(test)]
mod recording_session_genericity {
    use crate::driver::{Bind, DbError, Row, Value};
    use super::*;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The host-shaped one-in-flight guard (§B.6), mechanically enforced in the
    /// driver rather than trusted by analogy. Every verb `compare_exchange(false,
    /// true)`s on entry and clears via [`InFlightGuard`]'s `Drop` on the way out
    /// (so error paths clear too). A second verb entered while the first's future
    /// is still alive **panics** — turning "the engine issues one verb at a time"
    /// from a claim into a checked invariant. On a real pinned host connection this
    /// would otherwise deadlock (the second `tsfn.call` blocks on a socket the
    /// first hasn't released); the panic surfaces the bug loudly instead.
    ///
    /// This is the exact discipline the MySQL `JsDriverBackend` uses
    /// (`transport.rs` `in_flight: bool`), lifted to `AtomicBool` because the seam
    /// is `&self`, not `&mut self`.
    struct InFlightGuard<'a>(&'a AtomicBool);

    impl<'a> InFlightGuard<'a> {
        /// Arm the guard on verb entry, panicking on re-entry.
        fn enter(flag: &'a AtomicBool) -> Self {
            if flag
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                panic!(
                    "SqlSession verb issued while another is in flight — the engine \
                     must be strictly one-verb-at-a-time (§B.6)"
                );
            }
            Self(flag)
        }
    }

    impl Drop for InFlightGuard<'_> {
        fn drop(&mut self) {
            // Clear in the completion arm (RAII) so an error/early-return path also
            // releases — a leaked `true` would deadlock every later verb.
            self.0.store(false, Ordering::Release);
        }
    }

    /// A non-compio, host-SHAPED [`SqlSession`] that (a) records the SQL + binds of
    /// every verb, (b) returns canned neutral rows for the read verbs, routed by a
    /// substring match on the SQL so a full apply/introspection sweep decodes, and
    /// (c) enforces the one-in-flight guard (§B.6) on every verb. This is NOT a
    /// napi bridge (that is Phase C) — it is the in-crate host-shaped producer that
    /// proves the generic PG apply path is genuinely driver-neutral, and converts
    /// the one-in-flight invariant from by-analogy to mechanically-checked.
    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
        /// The mechanically-enforced one-verb-at-a-time guard (§B.6).
        in_flight: AtomicBool,
        /// Canned rows the `net_applied` journal read returns (SQL-routed).
        canned_journal: RefCell<Vec<Row>>,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
                in_flight: AtomicBool::new(false),
                canned_journal: RefCell::new(Vec::new()),
            }
        }

        fn with_canned_journal(rows: Vec<Row>) -> Self {
            let s = Self::new();
            *s.canned_journal.borrow_mut() = rows;
            s
        }

        /// Route a read to its canned rows by SQL shape. ONLY the journal net-state
        /// read (`journal::applied`, recognisable by its `union_all` CTE + the
        /// `schema_migrations_inflight` UNION leg — a shape no other query has) gets
        /// the canned (version, checksum, mig_kind, phase) journal rows; every other
        /// read (catalog introspection in `snapshot_schema`, the `superseded_versions`
        /// squash read whose only column is `v`, drift probes) gets an EMPTY result,
        /// which yields an empty-but-valid decode — enough to drive every path
        /// end-to-end without feeding a wrong-shaped row into a decoder.
        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("union_all") && sql.contains("schema_migrations_inflight") {
                self.canned_journal.borrow().clone()
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
        async fn exec_text(
            &self,
            sql: &str,
            _params: &[Option<String>],
        ) -> Result<u64, DbError> {
            let _g = InFlightGuard::enter(&self.in_flight);
            self.log
                .borrow_mut()
                .push(format!("exec_text: {sql}"));
            Ok(1)
        }
        async fn query(
            &self,
            sql: &str,
            params: &[Bind],
        ) -> Result<Vec<Row>, DbError> {
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
    /// (version, checksum, mig_kind, phase) — exactly what a host `pg` driver would
    /// return for that read.
    fn canned_journal_row(version: &str, checksum: &str) -> Row {
        Row::new(
            vec![
                "version".to_string(),
                "checksum".to_string(),
                "mig_kind".to_string(),
                "phase".to_string(),
            ],
            vec![
                Value::Text(version.to_string()),
                Value::Text(checksum.to_string()),
                Value::Text("apply".to_string()),
                Value::Text("completed".to_string()),
            ],
        )
    }

    /// The flagship proof: `PostgresBackend::<'_, RecordingSession>::new_generic`
    /// monomorphizes, and the write/DDL/lock verbs run generically against a
    /// non-compio driver, recording the exact SQL the executor emits.
    #[compio::test]
    async fn write_path_runs_generically_against_a_non_compio_driver() {
        let rec = RecordingSession::new();
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);

        // A non-native `D` reports no PG-concrete online/shadow harness.
        assert!(backend.online().is_none(), "generic D has no PgOnline harness");
        assert!(backend.shadow().is_none(), "generic D has no PgShadow harness");

        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        // Lock acquire/release + RESET ROLE — all write/DDL verbs, run through the
        // generic MigrationBackend surface, recorded by the non-compio driver.
        backend.acquire_project_lock(&cfg).await.expect("acquire lock");
        backend.reset_role_best_effort().await;
        backend.release_project_lock(&cfg).await.expect("release lock");

        let log = rec.log.borrow();
        assert!(
            log.iter().any(|s| s.contains("pg_advisory_lock")),
            "advisory-lock acquire ran through the trait's exec: {log:?}"
        );
        assert!(
            log.iter().any(|s| s == "batch: RESET ROLE"),
            "RESET ROLE ran through the trait's batch: {log:?}"
        );
        assert!(
            log.iter().any(|s| s.contains("pg_advisory_unlock")),
            "advisory-unlock release ran through the trait's exec: {log:?}"
        );

        // The advisory-lock verbs bound the project id through the neutral
        // Bind path (§A.1) — the param widening ran, not just the return one.
        let binds = rec.binds.borrow();
        assert!(
            binds
                .iter()
                .any(|b| b.iter().any(|v| matches!(v, Bind::Text(t) if t == "prj_x"))),
            "project id crossed the seam as a neutral Bind::Text: {binds:?}"
        );
    }

    /// The read side is now RUN, not merely compiled: the generic journal read
    /// (`applied`) is driven against canned neutral `Row`s and its decode
    /// (`Row → AppliedEntry`) runs end-to-end over a non-compio driver — the
    /// §A closure of the old `unreachable!("read verbs…")` gap.
    #[compio::test]
    async fn read_path_runs_generically_over_canned_seam_rows() {
        let rec = RecordingSession::with_canned_journal(vec![canned_journal_row(
            "mig_0001", "deadbeef",
        )]);
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        let applied = backend.applied(&cfg).await.expect("applied read runs");
        assert_eq!(applied.len(), 1, "one canned journal row decoded");
        assert_eq!(applied[0].version, "mig_0001");
        assert_eq!(applied[0].checksum, "deadbeef");

        // The read verb issued a `query` (not `execute`) through the trait.
        assert!(
            rec.log.borrow().iter().any(|s| s.starts_with("query")),
            "applied() drove a query through the neutral read seam: {:?}",
            rec.log.borrow()
        );
    }

    /// §B.6 → mechanically-proven: drive a FULL sweep over the whole DDL + journal-
    /// write + journal-read + drift-read + status/history surface against the host-
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
    /// driver — their decoded shapes matching what `native-pg` produces.
    #[compio::test]
    async fn full_surface_runs_generically_with_in_flight_guard_never_tripping() {
        let rec = RecordingSession::with_canned_journal(vec![canned_journal_row(
            "mig_0001", "cafef00d",
        )]);
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        // 1. WRITE / DDL — journal bootstrap: CREATE SCHEMA + the append-only events
        //    table + the immutability trigger, all through `batch`.
        backend.ensure_journal(&cfg).await.expect("ensure_journal DDL");

        // 2. WRITE — a journal INSERT (`record_started`) through `exec` with
        //    neutral Bind params. Drives the param-side seam (§A.1) on a write.
        crate::apply::journal::record_started(
            &rec, &cfg, "mig_0001", "create_users", "cafef00d", "tester",
        )
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
        let snap = backend.snapshot_schema(&cfg).await.expect("snapshot_schema");
        assert!(
            snap.tables.is_empty(),
            "empty canned catalog → empty snapshot (decode chain ran clean)"
        );

        // 5. READ — the status()/history() free fns over the SAME driver (§C.5
        //    holdout 3: generalized to `<D: SqlSession>`).
        let st = crate::ops::status::status(&rec, &cfg, &[])
            .await
            .expect("status over host driver");
        // The canned journal row is net-applied, so status sees it as applied.
        assert!(
            st.applied.iter().any(|e| e.version == "mig_0001"),
            "status decoded the net-applied version over Row: {:?}",
            st.applied
        );
        let hist = crate::ops::status::history(&rec, &cfg)
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
            rec.binds
                .borrow()
                .iter()
                .any(|b| b.iter().any(|v| matches!(v, Bind::Text(t) if t == "mig_0001"))),
            "journal INSERT bound the version as a neutral Bind::Text"
        );
    }

    /// The guard is a real guard: a deliberately re-entrant driver (a verb that
    /// issues a second verb before its own future completes) **panics**. This pins
    /// the §B.6 panic behavior so a future refactor that holds a verb across a
    /// suspension point fails loudly rather than deadlocking in production.
    #[compio::test]
    #[should_panic(expected = "one-verb-at-a-time")]
    async fn in_flight_guard_panics_on_reentry() {
        let flag = AtomicBool::new(false);
        let _outer = InFlightGuard::enter(&flag);
        // A second verb entered while the first guard is still alive must panic.
        let _inner = InFlightGuard::enter(&flag);
    }
}
