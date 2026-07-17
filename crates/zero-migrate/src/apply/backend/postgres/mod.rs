//! Postgres [`MigrationBackend`](super::MigrationBackend) implementation.
//!
//! Generic over the dialect-neutral [`SqlSession`](crate::driver::SqlSession) seam
//! (engine root `crate::driver`) — a host driver (the napi `pg` shell) supplies the
//! `SqlSession` impl. SQLite does NOT ride this seam (it is an in-process rusqlite
//! actor).

use crate::driver::SqlSession;

mod backfill_sql;
mod primary_key_sql;
/// The Postgres dialect SQL leaves (session/lock/txn/journal/DML/rollback) this
/// backend drives — relocated out of the generic `apply::executor` so no
/// dialect-specific SQL lives in the shared executor.
pub(crate) mod session;

use super::capability::{BackfillSpec, OnlineSchemaChange, ShadowDryRun};
use super::{CrossDeployObligations, JournalFuture, MigrationBackend, PgSessionSnapshot};
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{self, AppliedEntry, JournalError};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Migration, MigrationId};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::{DatabaseRequirements, SqliteRebuildSpec};
use crate::render::step::AlterPrimaryKeyStep;
use crate::render::step::BindValue;
use crate::schema::query::SqlDialect;

/// The generic Postgres [`MigrationBackend`] implementation on the host-pg build.
///
/// Generic over the [`SqlSession`] driver seam. Online expand-contract work uses
/// the same generic DDL, backfill, journal, and lock primitives as ordinary host
/// apply. Shadow-database dry runs still require a separate provisioning harness
/// and therefore remain unavailable on this backend.
#[cfg(pg_seam)]
#[derive(Debug)]
pub struct PostgresBackend<'a, D: SqlSession> {
    conn: &'a D,
}

#[cfg(pg_seam)]
impl<'a, D: SqlSession> PostgresBackend<'a, D> {
    /// Wrap any [`SqlSession`] driver as the PostgreSQL backend. Ordinary apply,
    /// schema snapshots, and online expand/backfill execution use this generic
    /// session; only shadow-database provisioning needs a separate harness.
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

    async fn verify_database_requirements(
        &self,
        requirements: &DatabaseRequirements,
    ) -> Result<(), ApplyError> {
        if requirements.is_empty() {
            return Ok(());
        }
        let actual = session::server_version_num(self.conn).await?;
        for feature in requirements.iter() {
            let minimum = feature.minimum_postgres_version_num();
            if actual < minimum {
                let minimum_major = minimum / 10_000;
                return Err(ApplyError::Backend(format!(
                    "{} requires PostgreSQL {minimum_major} or newer \
                     (server_version_num >= {minimum}); connected server reports {actual}",
                    feature.description()
                )));
            }
        }
        Ok(())
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
            session::apply_transactional(self.conn, cfg, m, applied_by, supersedes, kind).await?;
            Ok(false)
        }
    }

    async fn alter_primary_key(
        &self,
        cfg: &ExecutorConfig,
        step: &AlterPrimaryKeyStep,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        primary_key_sql::apply(self.conn, cfg, step, approval, scope, applied_by).await
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

    async fn net_rolled_back_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal::net_rolled_back(self.conn, cfg)
            .await
            .map(|entries| entries.into_iter().map(|entry| entry.version).collect())
    }

    async fn backfill_progress(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<crate::apply::backend::BackfillProgressEntry>, JournalError> {
        backfill_sql::read_progress_entries(self.conn, cfg).await
    }

    async fn superseded_versions(&self, cfg: &ExecutorConfig) -> Result<Vec<String>, JournalError> {
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

    async fn run_backfill_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        checksum: &crate::model::migration::Checksum,
        spec: &BackfillSpec,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        if let Some(entry) = self
            .applied(cfg)
            .await
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|entry| matches!(entry.phase, crate::apply::journal::Phase::Completed))
            .find(|entry| entry.version == version.as_str())
        {
            if entry.checksum != checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: checksum.as_str().to_string(),
                });
            }
            return Ok(crate::apply::executor::ApplyOutcome {
                applied: Vec::new(),
                skipped: vec![version.as_str().to_string()],
                recovered: Vec::new(),
            });
        }
        if approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }
        let outcome = backfill_sql::run_backfill(
            self.conn, cfg, version, checksum, spec, approval, None, applied_by,
        )
        .await?;
        Ok(crate::apply::executor::ApplyOutcome {
            applied: outcome
                .complete
                .then(|| version.as_str().to_string())
                .into_iter()
                .collect(),
            skipped: Vec::new(),
            recovered: Vec::new(),
        })
    }

    async fn run_dml_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        checksum: &crate::model::migration::Checksum,
        name: &str,
        template: &str,
        binds: &[BindValue],
        _target_schema: &str,
        _target_table: &str,
        _conflict_target: Option<&[String]>,
        _mutates_data: bool,
        destructive: bool,
        _owner_app: &str,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        let completed = self
            .applied(cfg)
            .await
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|e| matches!(e.phase, crate::apply::journal::Phase::Completed))
            .find(|e| e.version == version.as_str());
        if let Some(entry) = completed {
            if entry.checksum != checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: checksum.as_str().to_string(),
                });
            }
            return Ok(false);
        }
        if destructive && approval != crate::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if destructive && !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }
        session::apply_dml_transactional(
            self.conn,
            cfg,
            version.as_str(),
            checksum,
            name,
            template,
            binds,
            applied_by,
        )
        .await?;
        Ok(true)
    }

    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        Some(self)
    }

    // Host-pg build: no shadow harness → always `None`, so
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

impl<D: SqlSession> OnlineSchemaChange for PostgresBackend<'_, D> {
    fn run_online<'a>(
        &'a self,
        intent: &'a crate::render::expand_contract::OnlineIntent,
        expand: &'a [Migration],
        backfill: &'a BackfillSpec,
        approval: crate::approval::Approval,
        scope: &'a crate::approval::ApprovalScope,
        trigger_version: &'a MigrationId,
        cfg: &'a ExecutorConfig,
        applied_by: &'a str,
        lock_mode: crate::apply::executor::LockMode,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::apply::executor::ApplyOutcome,
                        crate::engine::OnlineError,
                    >,
                > + 'a,
        >,
    > {
        Box::pin(async move {
            use crate::apply::executor::LockMode;

            if approval != crate::approval::Approval::Approved {
                return Err(crate::engine::OnlineError::Approval);
            }
            let scope_version = expand
                .first()
                .map_or_else(|| trigger_version.as_str(), |e1| e1.version.as_str());
            if !scope.admits(scope_version) {
                return Err(crate::engine::OnlineError::ApprovalNotScoped {
                    version: scope_version.to_string(),
                });
            }
            let crate::render::expand_contract::OnlineIntent::RenameColumn {
                table, from, to, ..
            } = intent;
            let allowed_engine_trigger = backfill_sql::AllowedOnlineRenameTrigger::new(
                crate::render::expand_contract::dual_write_trg_name(table, from, to),
                crate::render::expand_contract::dual_write_fn_name(table, from, to),
                from.clone(),
                to.clone(),
            );

            let own_lock = lock_mode == LockMode::Acquire;
            if own_lock {
                self.acquire_project_lock(cfg).await?;
            }
            let result = async {
                if expand.len() < 3 {
                    return Err(crate::engine::OnlineError::Apply(ApplyError::Backend(
                        "postgres online rename requires the complete expand sequence".to_string(),
                    )));
                }
                let (backfill_marker, head) = expand
                    .split_last()
                    .expect("the complete expand sequence has a backfill marker");

                let mut outcome = crate::apply::executor::apply_with_lock_backend(
                    self,
                    cfg,
                    head,
                    approval,
                    scope,
                    applied_by,
                    LockMode::AlreadyHeld,
                )
                .await?;
                crate::fault::trip(crate::fault::points::EXPAND_BETWEEN_E2_AND_BACKFILL)?;

                // E3 is the durable backfill marker. The data-step seam checks its
                // journal state first, resumes its cursor when needed, and records
                // completion only after the full cohort is mirrored.
                let completed = self
                    .applied(cfg)
                    .await
                    .map_err(ApplyError::Journal)?
                    .into_iter()
                    .filter(|entry| matches!(entry.phase, crate::apply::journal::Phase::Completed))
                    .find(|entry| entry.version == backfill_marker.version.as_str());
                if let Some(entry) = completed {
                    if entry.checksum != backfill_marker.checksum.as_str() {
                        return Err(crate::engine::OnlineError::Apply(
                            ApplyError::ChecksumDrift {
                                version: backfill_marker.version.as_str().to_string(),
                                recorded: entry.checksum,
                                expected: backfill_marker.checksum.as_str().to_string(),
                            },
                        ));
                    }
                    outcome
                        .skipped
                        .push(backfill_marker.version.as_str().to_string());
                } else {
                    let backfill_outcome = backfill_sql::run_backfill(
                        self.conn,
                        cfg,
                        &backfill_marker.version,
                        &backfill_marker.checksum,
                        backfill,
                        approval,
                        Some(&allowed_engine_trigger),
                        applied_by,
                    )
                    .await?;
                    if backfill_outcome.complete {
                        outcome
                            .applied
                            .push(backfill_marker.version.as_str().to_string());
                    }
                }
                Ok::<_, crate::engine::OnlineError>(outcome)
            }
            .await;

            if !own_lock {
                return result;
            }
            let unlock = self.release_project_lock(cfg).await;
            match result {
                Ok(outcome) => unlock
                    .map(|()| outcome)
                    .map_err(crate::engine::OnlineError::Apply),
                Err(error) => {
                    if let Err(unlock_error) = unlock {
                        tracing::warn!(
                            error = %unlock_error,
                            "zero-migrate: failed to release project lock after online rename error"
                        );
                    }
                    Err(error)
                }
            }
        })
    }
}

impl<D: SqlSession> CrossDeployObligations for PostgresBackend<'_, D> {
    fn outstanding_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::PendingContract>> {
        Box::pin(async move { journal::outstanding_pending_contracts(self.conn, cfg).await })
    }

    fn resolved_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::ResolvedPendingContract>> {
        Box::pin(async move { journal::resolved_pending_contracts(self.conn, cfg).await })
    }

    fn pending_contract_shape<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        contract: &'a journal::PendingContract,
    ) -> JournalFuture<'a, journal::PendingContractShape> {
        Box::pin(async move { journal::pending_contract_shape(self.conn, cfg, contract).await })
    }

    fn record_pending_contract_with_recovery<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        rec: journal::PendingContractRecord<'a>,
        scope: Option<journal::DeployRecoveryScope<'a>>,
    ) -> JournalFuture<'a, bool> {
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
            journal::mark_deploy_recovery_reconciled(self.conn, cfg, deploy_id, pending_version, by)
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

/// Genericity proof: the apply path monomorphizes over a
/// **non-compio** [`SqlSession`] driver. An in-crate recording driver records the
/// SQL of every WRITE verb, and — now that the read side is widened to the
/// driver-neutral [`Row`]/[`DbError`] — RETURNS canned `Row`s from
/// its read verbs. This proves `PostgresBackend<'a, D>` is genuinely generic AND
/// that a host driver can build return values without a `compio_postgres::Row`,
/// closing the old `unreachable!("read verbs…")` gap.
#[cfg(test)]
mod recording_session_genericity {
    use super::*;
    use crate::apply::executor::LockMode;
    use crate::approval::{Approval, ApprovalScope};
    use crate::driver::{Bind, DbError, Row, Value};
    use crate::engine::{DeclarativeApplyError, EngineError, MigrationEngine};
    use crate::model::migration::{Checksum, ChecksumInput, Migration, MigrationFlags};
    use crate::render::plan::{AppliedPlan, DatabaseFeature, DatabaseRequirements};
    use crate::render::step::PlanStep;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// The host-shaped one-in-flight guard, mechanically enforced in the
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
                     must be strictly one-verb-at-a-time"
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
    /// (c) enforces the one-in-flight guard on every verb. This is NOT a
    /// napi bridge — it is the in-crate host-shaped producer that
    /// proves the generic PG apply path is genuinely driver-neutral, and converts
    /// the one-in-flight invariant from by-analogy to mechanically-checked.
    struct RecordingSession {
        log: RefCell<Vec<String>>,
        binds: RefCell<Vec<Vec<Bind>>>,
        /// The mechanically-enforced one-verb-at-a-time guard.
        in_flight: AtomicBool,
        /// Canned rows the `net_applied` journal read returns (SQL-routed).
        canned_journal: RefCell<Vec<Row>>,
        /// Canned rows returned by the read-only backfill progress reader.
        canned_progress: RefCell<Vec<Row>>,
        progress_table_exists: bool,
        progress_checksum_exists: bool,
        server_version_num: i32,
    }

    impl RecordingSession {
        fn new() -> Self {
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

        fn with_server_version(server_version_num: i32) -> Self {
            Self {
                server_version_num,
                ..Self::new()
            }
        }

        fn with_canned_journal(rows: Vec<Row>) -> Self {
            let s = Self::new();
            *s.canned_journal.borrow_mut() = rows;
            s
        }

        fn with_canned_progress(rows: Vec<Row>, checksum_exists: bool) -> Self {
            let mut session = Self::new();
            *session.canned_progress.borrow_mut() = rows;
            session.progress_table_exists = true;
            session.progress_checksum_exists = checksum_exists;
            session
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

    fn plan_dml_step(label: &str, destructive: bool) -> (PlanStep, MigrationId, Checksum) {
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

    fn plan_backfill_step() -> (PlanStep, MigrationId, Checksum) {
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
                    cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
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

    async fn apply_recorded_plan(
        rec: &RecordingSession,
        steps: &[PlanStep],
        approval: Approval,
        scope: &ApprovalScope,
    ) -> Result<crate::engine::DeclarativeDeployOutcome, DeclarativeApplyError> {
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(rec);
        MigrationEngine::new()
            .apply_plan_with_touched_and_depends_scoped(
                steps,
                &["users".into()],
                &[],
                approval,
                scope,
                &backend,
                &ExecutorConfig::new("prj_x", "proj_x"),
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
                entry.contains("UPDATE users SET ready")
                    || entry.contains("DELETE FROM users WHERE id")
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

    fn requirements(feature: DatabaseFeature) -> DatabaseRequirements {
        let mut requirements = DatabaseRequirements::default();
        requirements.require(feature);
        requirements
    }

    #[compio::test]
    async fn uuid_generation_requirements_gate_postgres_server_versions() {
        let empty = RecordingSession::with_server_version(120_000);
        let empty_backend = PostgresBackend::new_generic(&empty);
        empty_backend
            .verify_database_requirements(&DatabaseRequirements::default())
            .await
            .expect("an empty requirement set performs no version gate");
        assert!(
            empty.log.borrow().is_empty(),
            "empty requirements must not query the server"
        );

        for (feature, rejected, accepted, expected) in [
            (
                DatabaseFeature::UuidV4Generation,
                120_000,
                130_000,
                "PostgreSQL 13",
            ),
            (
                DatabaseFeature::UuidV7Generation,
                170_000,
                180_000,
                "PostgreSQL 18",
            ),
        ] {
            let old = RecordingSession::with_server_version(rejected);
            let old_backend = PostgresBackend::new_generic(&old);
            let error = old_backend
                .verify_database_requirements(&requirements(feature))
                .await
                .expect_err("an older PostgreSQL server must fail closed");
            let message = error.to_string();
            assert!(message.contains(expected), "got: {message}");
            assert!(
                message.contains(&rejected.to_string()),
                "actual server version must be reported: {message}"
            );

            let current = RecordingSession::with_server_version(accepted);
            PostgresBackend::new_generic(&current)
                .verify_database_requirements(&requirements(feature))
                .await
                .expect("the minimum supported PostgreSQL version must pass");
            assert!(current
                .log
                .borrow()
                .iter()
                .any(|entry| { entry.contains("current_setting('server_version_num')") }));
        }
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
        };
        let mut plan = AppliedPlan::single_step(migration);
        plan.database_requirements
            .require(DatabaseFeature::UuidV7Generation);

        let result = MigrationEngine::new()
            .apply_applied_plan_with_touched_and_depends(
                &plan,
                &[],
                &[],
                Approval::None,
                &backend,
                &ExecutorConfig::new("prj_x", "proj_x"),
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

    /// The flagship proof: `PostgresBackend::<'_, RecordingSession>::new_generic`
    /// monomorphizes, and the write/DDL/lock verbs run generically against a
    /// non-compio driver, recording the exact SQL the executor emits.
    #[compio::test]
    async fn write_path_runs_generically_against_a_non_compio_driver() {
        let rec = RecordingSession::new();
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);

        // Online expand-contract work rides the same generic SqlSession seam.
        assert!(
            backend.online().is_some(),
            "generic D must expose the host-capable online runner"
        );
        assert!(
            backend.shadow().is_none(),
            "generic D has no PgShadow harness"
        );

        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        // Lock acquire/release + RESET ROLE — all write/DDL verbs, run through the
        // generic MigrationBackend surface, recorded by the non-compio driver.
        backend
            .acquire_project_lock(&cfg)
            .await
            .expect("acquire lock");
        backend.reset_role_best_effort().await;
        backend
            .release_project_lock(&cfg)
            .await
            .expect("release lock");

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
        // Bind path — the param widening ran, not just the return one.
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
    /// closure of the old `unreachable!("read verbs…")` gap.
    #[compio::test]
    async fn read_path_runs_generically_over_canned_seam_rows() {
        let rec =
            RecordingSession::with_canned_journal(vec![canned_journal_row("mig_0001", "deadbeef")]);
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

    #[compio::test]
    async fn backfill_progress_reader_decodes_existing_rows_without_bootstrap() {
        let rec = RecordingSession::with_canned_progress(
            vec![Row::new(
                vec!["backfill_id".into(), "checksum".into(), "complete".into()],
                vec![
                    Value::Text("mig_progress".into()),
                    Value::Text("checksum_a".into()),
                    Value::Bool(false),
                ],
            )],
            true,
        );
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        let progress = backend
            .backfill_progress(&cfg)
            .await
            .expect("progress read runs");

        assert_eq!(
            progress,
            vec![crate::apply::backend::BackfillProgressEntry {
                version: "mig_progress".into(),
                checksum: Some("checksum_a".into()),
                complete: false,
            }]
        );
        assert!(
            !rec.log
                .borrow()
                .iter()
                .any(|entry| entry.contains("CREATE TABLE") && entry.contains("schema_backfills")),
            "status must not bootstrap progress state"
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
        let cfg = ExecutorConfig::new("prj_x", "proj_x");

        // 1. WRITE / DDL — journal bootstrap: CREATE SCHEMA + the append-only events
        //    table + the immutability trigger, all through `batch`.
        backend
            .ensure_journal(&cfg)
            .await
            .expect("ensure_journal DDL");

        // 2. WRITE — a journal INSERT (`record_started`) through `exec` with
        //    neutral Bind params. Drives the param-side seam on a write.
        crate::apply::journal::record_started(
            &rec,
            &cfg,
            "mig_0001",
            "create_users",
            "cafef00d",
            "tester",
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
        let snap = backend
            .snapshot_schema(&cfg)
            .await
            .expect("snapshot_schema");
        assert!(
            snap.tables.is_empty(),
            "empty canned catalog → empty snapshot (decode chain ran clean)"
        );

        // 5. READ — the status()/history() free fns over the SAME driver
        //    (generalized to `<D: SqlSession>`).
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
            rec.binds.borrow().iter().any(|b| b
                .iter()
                .any(|v| matches!(v, Bind::Text(t) if t == "mig_0001"))),
            "journal INSERT bound the version as a neutral Bind::Text"
        );
    }

    /// The guard is a real guard: a deliberately re-entrant driver (a verb that
    /// issues a second verb before its own future completes) **panics**. This pins
    /// the one-in-flight panic behavior so a future refactor that holds a verb across a
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
