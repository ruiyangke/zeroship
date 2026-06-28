//! MySQL [`MigrationBackend`](super::MigrationBackend) skeleton for Phase E1.
//!
//! E1 proves the seam and the productionized Trusted JS-driver pump. It does
//! not wire live MySQL apply; that is E2. The transport is therefore pointed at
//! an offline echo driver by default, while the mysql2 bundle and Trusted
//! runtime construction are committed for the live path.

pub mod snapshot;
pub mod transport;

use std::cell::RefCell;
use std::collections::HashMap;

use super::capability::{BackfillSpec, OnlineSchemaChange, ShadowDryRun};
use super::{CrossDeployObligations, MigrationBackend};
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::{ChecksumDriftReport, DriftError};
use crate::apply::executor::{
    ApplyError, ApplyOutcome, LockMode, PreconditionVerdict, RollbackError,
};
use crate::apply::journal::{AppliedEntry, JournalError};
use crate::approval::{Approval, ApprovalScope};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Migration, MigrationId};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::SqliteRebuildSpec;
use crate::render::step::BindValue;
use snapshot::{rowsets_to_schema_snapshot, MysqlCatalogRowSets};
pub use transport::{JsDriverConn, JsDriverError, RowSet};
use zeroship_schema::query::SqlDialect;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MysqlSessionSnapshot {
    pub innodb_lock_wait_timeout: String,
    pub sql_mode: String,
}

#[derive(Debug)]
pub struct MysqlBackend {
    conn: RefCell<JsDriverConn>,
}

impl MysqlBackend {
    pub fn new(conn: JsDriverConn) -> Self {
        Self {
            conn: RefCell::new(conn),
        }
    }

    pub fn echo() -> Result<Self, JsDriverError> {
        Ok(Self::new(JsDriverConn::open_echo()?))
    }

    async fn exec(&self, sql: &str) -> Result<(), JsDriverError> {
        self.conn.borrow_mut().exec(sql).await
    }

    async fn query_json(
        &self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<RowSet, JsDriverError> {
        self.conn.borrow_mut().query_json(sql, binds).await
    }
}

impl MigrationBackend for MysqlBackend {
    type SessionSnapshot = MysqlSessionSnapshot;

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mysql
    }

    fn ddl_is_transactional(&self) -> bool {
        false
    }

    async fn acquire_project_lock(&self, project_id: &str) -> Result<(), ApplyError> {
        self.exec(&format!("/* E1 echo acquire_project_lock {project_id} */"))
            .await
            .map_err(apply_db)
    }

    async fn release_project_lock(&self, project_id: &str) -> Result<(), ApplyError> {
        self.exec(&format!("/* E1 echo release_project_lock {project_id} */"))
            .await
            .map_err(apply_db)
    }

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        Ok(MysqlSessionSnapshot {
            innodb_lock_wait_timeout: "50".to_string(),
            sql_mode: String::new(),
        })
    }

    async fn restore_session(&self, _snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn reset_role_best_effort(&self) {
        // MySQL confinement is by least-privilege account identity, not SET ROLE.
    }

    async fn apply_one(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        _applied_by: &str,
        _had_inflight: bool,
        _supersedes: &[&str],
        _kind: &str,
    ) -> Result<bool, ApplyError> {
        self.exec(&m.up).await.map_err(apply_db)?;
        Ok(false)
    }

    async fn rollback_one_transactional(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        _applied_by: &str,
    ) -> Result<(), RollbackError> {
        let Some(down) = m.down.as_deref() else {
            return Err(RollbackError::Backend(format!(
                "mysql backend: migration '{}' has no down SQL",
                m.version.as_str()
            )));
        };
        self.exec(down).await.map_err(rollback_db)
    }

    fn validate_non_txn(&self, _m: &Migration) -> Result<(), ApplyError> {
        Ok(())
    }

    async fn ensure_journal(&self, _cfg: &ExecutorConfig) -> Result<(), JournalError> {
        self.exec("/* E1 echo ensure MySQL journal */")
            .await
            .map_err(journal_backend)
    }

    async fn applied(&self, _cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        Ok(Vec::new())
    }

    async fn superseded_versions(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        Ok(Vec::new())
    }

    async fn latest_completed_checksums(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<HashMap<String, String>, JournalError> {
        Ok(HashMap::new())
    }

    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations> {
        None
    }

    async fn check_checksum_drift(
        &self,
        _cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<ChecksumDriftReport, DriftError> {
        Ok(crate::apply::drift::compare_applied_to_set(&[], migrations))
    }

    async fn snapshot_schema(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<SchemaSnapshot, DriftError> {
        rowsets_to_schema_snapshot(MysqlCatalogRowSets::default())
    }

    async fn evaluate_preconditions(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<PreconditionVerdict, ApplyError> {
        if m.preconditions.is_empty() {
            Ok(PreconditionVerdict::AllMet)
        } else {
            Err(ApplyError::Backend(
                "mysql backend E1: live precondition evaluation is Phase E2".to_string(),
            ))
        }
    }

    async fn record_squash(
        &self,
        _cfg: &ExecutorConfig,
        squash_migration: &Migration,
        _applied_by: &str,
        _supersedes: &[&str],
    ) -> Result<(), ApplyError> {
        Err(ApplyError::Backend(format!(
            "mysql backend E1: squash journaling is not wired yet ({})",
            squash_migration.version.as_str()
        )))
    }

    async fn rebuild_one(
        &self,
        spec: &SqliteRebuildSpec,
        _m: &Migration,
        _scope: &ApprovalScope,
        _applied_by: &str,
    ) -> Result<(), ApplyError> {
        Err(ApplyError::Backend(format!(
            "mysql backend: SQLite table rebuild requested for '{}' (routing bug)",
            spec.table
        )))
    }

    async fn run_backfill_step(
        &self,
        _cfg: &ExecutorConfig,
        spec: &BackfillSpec,
        _approval: Approval,
        _scope: &ApprovalScope,
        _applied_by: &str,
        _lock_mode: LockMode,
    ) -> Result<ApplyOutcome, ApplyError> {
        Err(ApplyError::Backend(format!(
            "mysql backend E1: backfill step '{}' is not wired until Phase E2",
            spec.name
        )))
    }

    async fn run_dml_step(
        &self,
        _cfg: &ExecutorConfig,
        _version: &MigrationId,
        _name: &str,
        template: &str,
        binds: &[BindValue],
        destructive: bool,
        _owner_app: &str,
        approval: Approval,
        _scope: &ApprovalScope,
        _applied_by: &str,
        _lock_mode: LockMode,
    ) -> Result<bool, ApplyError> {
        if destructive && approval != Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        self.query_json(template, binds).await.map_err(apply_db)?;
        Ok(true)
    }

    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        None
    }

    fn shadow(&self) -> Option<&dyn ShadowDryRun> {
        None
    }

    async fn baseline_one(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
        _applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError> {
        Err(BaselineError::Backend(format!(
            "mysql backend E1: baseline '{}' is not wired until Phase E2",
            m.version.as_str()
        )))
    }
}

fn apply_db(error: JsDriverError) -> ApplyError {
    ApplyError::Db(transport::backend_error(error))
}

fn rollback_db(error: JsDriverError) -> RollbackError {
    RollbackError::Db(transport::backend_error(error))
}

fn journal_backend(error: JsDriverError) -> JournalError {
    JournalError::Backend(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mysql_backend_compiles_against_unchanged_trait_and_uses_echo_transport() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let backend = MysqlBackend::echo().expect("echo backend");
            assert_eq!(backend.dialect(), SqlDialect::Mysql);
            assert!(!backend.ddl_is_transactional());

            backend
                .acquire_project_lock("project")
                .await
                .expect("echo lock");
            let rows = backend
                .query_json("select echo", &[BindValue::Int(7)])
                .await
                .expect("echo query");
            assert_eq!(rows.rows[0].get("command_count"), Some(&serde_json::json!(2)));
        });
    }
}
