//! Postgres [`MigrationBackend`](super::MigrationBackend) implementation.

pub mod backfill;
pub mod online;
pub mod session;
pub mod shadow;

use compio_postgres::Client;

pub use session::PgSession;

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
use zeroship_schema::query::SqlDialect;

/// The Postgres [`MigrationBackend`] implementation.
///
/// Generic over the [`PgSession`] driver seam (default: the native
/// `compio_postgres::Client`). The generic `D` lets a future non-compio driver
/// (a Node/napi `pg`-client shell) plug into the same apply logic. The
/// `online`/`shadow` capabilities stay PG-concrete (their lifecycle — the online
/// EXPAND `&Client` entry points, the shadow `CREATE DATABASE` + compio run-loop
/// `JoinHandle` — is compio-specific), so they are `Option` and present only on
/// the native path; a non-compio `D` reports `online() == None` / `shadow() ==
/// None`, both already permitted by the `MigrationBackend` capability seam.
#[derive(Debug)]
pub struct PostgresBackend<'a, D: PgSession = Client> {
    conn: &'a D,
    online: Option<online::PgOnline<'a>>,
    shadow: Option<shadow::PgShadow<'a>>,
}

impl<'a> PostgresBackend<'a, Client> {
    /// Wrap a migrator connection as the Postgres backend (native path).
    ///
    /// `D` is fixed to the concrete `compio_postgres::Client` here, so the
    /// PG-concrete `PgOnline`/`PgShadow` harnesses type-check and are always
    /// present — byte-for-byte the pre-seam behavior.
    #[must_use]
    pub fn new(conn: &'a Client) -> Self {
        Self {
            conn,
            online: Some(online::PgOnline::new(conn)),
            shadow: Some(shadow::PgShadow::new(conn)),
        }
    }
}

impl<'a, D: PgSession> PostgresBackend<'a, D> {
    /// Wrap any [`PgSession`] driver as the Postgres backend, WITHOUT the
    /// PG-concrete online/shadow harnesses (both `None`).
    ///
    /// Used to monomorphize the backend over a non-compio driver — the recording
    /// `PgSession` used to prove genericity, and any future Node/napi driver. The
    /// online/shadow capabilities report `None` for such a `D`; the write/DDL/
    /// journal apply path (which never touches them) runs unchanged.
    #[must_use]
    pub fn new_generic(conn: &'a D) -> Self {
        Self {
            conn,
            online: None,
            shadow: None,
        }
    }
}

impl<D: PgSession> MigrationBackend for PostgresBackend<'_, D> {
    type SessionSnapshot = PgSessionSnapshot;

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn ddl_is_transactional(&self) -> bool {
        true
    }

    async fn acquire_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        crate::apply::executor::pg::acquire_project_lock(self.conn, &cfg.project_id).await
    }

    async fn release_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        crate::apply::executor::pg::release_project_lock(self.conn, &cfg.project_id).await
    }

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        crate::apply::executor::pg::snapshot_session(self.conn).await
    }

    async fn restore_session(&self, snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        crate::apply::executor::pg::restore_session(self.conn, snap).await
    }

    async fn reset_role_best_effort(&self) {
        if let Err(e) = self.conn.batch_execute("RESET ROLE").await {
            tracing::warn!(error = %e, "zeroship-migrate: failed to RESET ROLE after apply (L1)");
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
            crate::apply::executor::pg::configure_session_non_txn(self.conn, cfg, m).await?;
            crate::apply::executor::pg::apply_non_transactional(
                self.conn,
                cfg,
                m,
                applied_by,
                had_inflight,
                supersedes,
            )
            .await
        } else {
            crate::apply::executor::pg::apply_transactional(
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
        crate::apply::executor::pg::rollback_one_transactional(self.conn, cfg, m, applied_by).await
    }

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        crate::apply::executor::pg::validate_non_txn_idempotent(m)
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

    async fn run_backfill_step(
        &self,
        cfg: &ExecutorConfig,
        spec: &BackfillSpec,
        approval: crate::approval::Approval,
        _scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        let outcome = backfill::run_backfill(self.conn, cfg, spec, approval, applied_by)
            .await
            .map_err(|e| ApplyError::Backend(format!("backfill step failed: {e}")))?;
        let applied = if outcome.complete {
            vec![spec.name.clone()]
        } else {
            Vec::new()
        };
        Ok(crate::apply::executor::ApplyOutcome {
            applied,
            skipped: Vec::new(),
            recovered: Vec::new(),
        })
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
        crate::apply::executor::apply_dml_transactional(
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

    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        self.online.as_ref().map(|o| o as &dyn OnlineSchemaChange)
    }

    fn shadow(&self) -> Option<&dyn ShadowDryRun> {
        self.shadow.as_ref().map(|s| s as &dyn ShadowDryRun)
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

impl<D: PgSession> CrossDeployObligations for PostgresBackend<'_, D> {
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

/// Genericity proof (design §6d): the apply path monomorphizes over a **non-compio**
/// [`PgSession`] driver. An in-crate recording driver records the SQL each verb
/// receives (and returns canned/`Err` for the read verbs, since a `compio_postgres::Row`
/// cannot be constructed outside its crate). This proves `PostgresBackend<'a, D>` is
/// genuinely generic — the seam is swappable, not merely compio behind a trait.
#[cfg(test)]
mod recording_session_genericity {
    use super::*;
    use std::cell::RefCell;

    /// A non-compio [`PgSession`] that records the SQL of every verb.
    struct RecordingSession {
        log: RefCell<Vec<String>>,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self { log: RefCell::new(Vec::new()) }
        }
    }

    impl PgSession for RecordingSession {
        async fn batch_execute(&self, sql: &str) -> Result<(), compio_postgres::Error> {
            self.log.borrow_mut().push(format!("batch_execute: {sql}"));
            Ok(())
        }
        async fn execute(
            &self,
            sql: &str,
            _params: &[&(dyn compio_postgres::types::ToSql + Sync)],
        ) -> Result<u64, compio_postgres::Error> {
            self.log.borrow_mut().push(format!("execute: {sql}"));
            Ok(1)
        }
        async fn execute_text_params(
            &self,
            sql: &str,
            _params: &[Option<String>],
        ) -> Result<u64, compio_postgres::Error> {
            self.log.borrow_mut().push(format!("execute_text_params: {sql}"));
            Ok(1)
        }
        async fn query(
            &self,
            _sql: &str,
            _params: &[&(dyn compio_postgres::types::ToSql + Sync)],
        ) -> Result<Vec<compio_postgres::Row>, compio_postgres::Error> {
            // A non-compio driver can construct NEITHER a `compio_postgres::Row`
            // (private fields) NOR a `compio_postgres::Error` (no public
            // constructor) — this is exactly the read-path coupling the design's
            // §3.2 `SeamRow` + §9.1 P1/P2 widening removes. So the read verbs are
            // unreachable in this proof; their VALUE is that the impl COMPILES,
            // proving the `query`/`query_one` generic signatures monomorphize over
            // a non-compio `D` (the write path below is what runs generically).
            unreachable!("read verbs are not exercised by the write-path genericity proof");
        }
        async fn query_one(
            &self,
            _sql: &str,
            _params: &[&(dyn compio_postgres::types::ToSql + Sync)],
        ) -> Result<compio_postgres::Row, compio_postgres::Error> {
            unreachable!("read verbs are not exercised by the write-path genericity proof");
        }
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
            "advisory-lock acquire ran through the trait's execute: {log:?}"
        );
        assert!(
            log.iter().any(|s| s == "batch_execute: RESET ROLE"),
            "RESET ROLE ran through the trait's batch_execute: {log:?}"
        );
        assert!(
            log.iter().any(|s| s.contains("pg_advisory_unlock")),
            "advisory-unlock release ran through the trait's execute: {log:?}"
        );
    }

    /// Compile-only proof that the READ path monomorphizes over a non-compio `D`.
    ///
    /// Building a future from `PostgresBackend::<'_, RecordingSession>::applied` /
    /// `::superseded_versions` (both `query`-issuing journal reads) forces the
    /// compiler to instantiate their generic bodies against `RecordingSession` —
    /// a non-compio `D`. The futures are dropped un-awaited: a recording driver
    /// cannot construct a `compio_postgres::Row`/`Error` to return (§6d), so the
    /// read path type-checks generically but is not run-proven this milestone
    /// (deferred to the §3.2 `SeamRow` widening). This code compiling IS the
    /// monomorphization proof.
    #[test]
    fn read_path_signatures_monomorphize_over_a_non_compio_driver() {
        let rec = RecordingSession::new();
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x");
        // Constructing these futures instantiates the generic read fn bodies over
        // `RecordingSession`; we never poll them (the read verbs `unreachable!`).
        let applied_fut = backend.applied(&cfg);
        let superseded_fut = backend.superseded_versions(&cfg);
        drop(applied_fut);
        drop(superseded_fut);
    }
}
