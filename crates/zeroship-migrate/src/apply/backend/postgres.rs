//! Postgres [`MigrationBackend`](super::MigrationBackend) implementation.

pub mod backfill;
pub mod online;
pub mod shadow;

use compio_postgres::Client;

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
#[derive(Debug)]
pub struct PostgresBackend<'a> {
    conn: &'a Client,
    online: online::PgOnline<'a>,
    shadow: shadow::PgShadow<'a>,
}

impl<'a> PostgresBackend<'a> {
    /// Wrap a migrator connection as the Postgres backend.
    #[must_use]
    pub fn new(conn: &'a Client) -> Self {
        Self {
            conn,
            online: online::PgOnline::new(conn),
            shadow: shadow::PgShadow::new(conn),
        }
    }
}

impl MigrationBackend for PostgresBackend<'_> {
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
        Some(&self.online)
    }

    fn shadow(&self) -> Option<&dyn ShadowDryRun> {
        Some(&self.shadow)
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

impl CrossDeployObligations for PostgresBackend<'_> {
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
