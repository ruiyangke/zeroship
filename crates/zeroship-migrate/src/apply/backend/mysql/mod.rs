//! MySQL [`MigrationBackend`](super::MigrationBackend) implementation.
//!
//! The backend talks to MySQL through the platform-owned Trusted JS driver
//! isolate. The isolate loads the vendored, unmodified `mysql2/promise` bundle
//! and bound SQL reaches the server through `connection.execute`; transaction
//! control uses mysql2's `beginTransaction`/`commit`/`rollback` on the same
//! connection.

pub mod snapshot;
pub mod transport;

use std::cell::RefCell;
use std::collections::HashMap;
#[cfg(test)]
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::capability::{BackfillSpec, OnlineSchemaChange, ShadowDryRun};
use super::{CrossDeployObligations, MigrationBackend};
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::{ChecksumDriftReport, DriftError};
use crate::apply::executor::{
    ApplyError, ApplyOutcome, LockMode, PreconditionVerdict, RollbackError,
};
use crate::apply::journal::{self, AppliedEntry, JournalError};
use crate::approval::{Approval, ApprovalScope};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Migration, MigrationId};
use crate::model::probe::GuardProbe;
use crate::model::snapshot::SchemaSnapshot;
use crate::render::existence_probe::GuardVerdict;
use crate::render::plan::SqliteRebuildSpec;
use crate::render::step::BindValue;
use snapshot::{rowsets_to_schema_snapshot, MysqlCatalogRowSets};
use sha2::{Digest, Sha256};
pub use transport::{JsDriverConn, JsDriverError, RowSet};
use zeroship_schema::query::SqlDialect;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MysqlSessionSnapshot {
    pub innodb_lock_wait_timeout: String,
    pub sql_mode: String,
}

pub struct MysqlBackend {
    conn: RefCell<JsDriverConn>,
    guarded_fragments: RefCell<HashMap<String, Vec<MysqlGuardedFragment>>>,
    #[cfg(test)]
    after_fragment_hook: RefCell<Option<Rc<dyn Fn(MysqlFragmentEvent) -> MysqlFragmentHookAction>>>,
}

impl std::fmt::Debug for MysqlBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MysqlBackend")
            .field("conn", &self.conn)
            .field("guarded_fragments", &self.guarded_fragments)
            .finish_non_exhaustive()
    }
}

const MYSQL_MIGRATOR_USER_PREFIX: &str = "zs_mig_";
const MYSQL_MIGRATOR_USER_MAX_CHARS: usize = 32;
const MYSQL_MIGRATOR_USER_HEX_CHARS: usize = 25;
const MYSQL_MIGRATOR_HOST: &str = "%";
const MYSQL_LOCK_PREFIX: &str = "zs_mig_";
const MYSQL_LOCK_HEX_CHARS: usize = 56;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlMigratorAccount {
    pub user: String,
    pub host: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlGuardedFragment {
    pub sql: String,
    pub existence_guard: GuardProbe,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MysqlFragmentDecision {
    RunBare,
    SatisfiedNoop,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlFragmentEvent {
    pub version: String,
    pub fragment_index: usize,
    pub sql: String,
    pub decision: MysqlFragmentDecision,
    pub had_inflight: bool,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MysqlFragmentHookAction {
    Continue,
    CloseConnection,
}

#[derive(Debug, thiserror::Error)]
pub enum MysqlMigratorAccountError {
    #[error("mysql migrator account db error: {0}")]
    Db(#[from] JsDriverError),
    #[error("mysql migrator account journal bootstrap failed: {0}")]
    Journal(#[from] JournalError),
    #[error("mysql migrator account identifier error: {0}")]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
}

impl MysqlBackend {
    pub fn new(conn: JsDriverConn) -> Self {
        Self {
            conn: RefCell::new(conn),
            guarded_fragments: RefCell::new(HashMap::new()),
            #[cfg(test)]
            after_fragment_hook: RefCell::new(None),
        }
    }

    pub fn echo() -> Result<Self, JsDriverError> {
        Ok(Self::new(JsDriverConn::open_echo()?))
    }

    pub fn open_mysql_dsn_json(
        dsn_json: String,
        command_timeout: Duration,
    ) -> Result<Self, JsDriverError> {
        Ok(Self::new(JsDriverConn::open_mysql_dsn_json(
            dsn_json,
            command_timeout,
        )?))
    }

    pub fn open_mysql_dsn_json_with_policy(
        dsn_json: String,
        net_policy: zeroship_runtime::NetPolicy,
        command_timeout: Duration,
    ) -> Result<Self, JsDriverError> {
        Ok(Self::new(JsDriverConn::open_mysql_dsn_json_with_policy(
            dsn_json,
            net_policy,
            command_timeout,
        )?))
    }

    pub async fn exec(&self, sql: &str) -> Result<(), JsDriverError> {
        self.conn.borrow_mut().exec(sql).await
    }

    pub async fn exec_with_binds(
        &self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<(), JsDriverError> {
        self.conn.borrow_mut().exec_with_binds(sql, binds).await
    }

    async fn begin_transaction(&self) -> Result<(), JsDriverError> {
        self.conn.borrow_mut().begin_transaction().await
    }

    async fn commit(&self) -> Result<(), JsDriverError> {
        self.conn.borrow_mut().commit().await
    }

    async fn rollback(&self) -> Result<(), JsDriverError> {
        self.conn.borrow_mut().rollback().await
    }

    pub async fn query_json(
        &self,
        sql: &str,
        binds: &[BindValue],
    ) -> Result<RowSet, JsDriverError> {
        self.conn.borrow_mut().query_json(sql, binds).await
    }

    pub fn register_guarded_fragments(
        &self,
        version: &str,
        fragments: Vec<MysqlGuardedFragment>,
    ) {
        self.guarded_fragments
            .borrow_mut()
            .insert(version.to_string(), fragments);
    }

    #[cfg(test)]
    pub fn set_after_fragment_hook(
        &self,
        hook: impl Fn(MysqlFragmentEvent) -> MysqlFragmentHookAction + 'static,
    ) {
        *self.after_fragment_hook.borrow_mut() = Some(Rc::new(hook));
    }

    #[cfg(test)]
    pub fn clear_after_fragment_hook(&self) {
        *self.after_fragment_hook.borrow_mut() = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MysqlApplyDecision {
    RunBare,
    SatisfiedNoop,
}

impl MysqlBackend {
    fn mysql_guarded_fragments(
        &self,
        m: &Migration,
    ) -> Result<Vec<MysqlGuardedFragment>, ApplyError> {
        let version = m.version.as_str();
        if let Some(fragments) = self.guarded_fragments.borrow().get(version).cloned() {
            return validate_mysql_guarded_fragments(m, fragments);
        }

        let fragments = mysql_statement_fragments(&m.up);
        if fragments.len() != 1 {
            return Err(ApplyError::NonIdempotentNonTxn {
                version: version.to_string(),
                reason: format!(
                    "MySQL non-transactional `up` contains {} SQL fragments but no \
                     per-fragment existence probes were supplied",
                    fragments.len()
                ),
            });
        }
        let Some(existence_guard) = m.existence_guard.clone() else {
            return Err(ApplyError::NonIdempotentNonTxn {
                version: version.to_string(),
                reason: "MySQL non-transactional DDL must carry an executor existence probe"
                    .to_string(),
            });
        };
        validate_mysql_guarded_fragments(
            m,
            vec![MysqlGuardedFragment {
                sql: fragments[0].clone(),
                existence_guard,
            }],
        )
    }

    async fn after_fragment_decision(
        &self,
        version: &str,
        fragment_index: usize,
        sql: &str,
        decision: MysqlApplyDecision,
        had_inflight: bool,
    ) -> Result<(), ApplyError> {
        #[cfg(test)]
        {
            let event = MysqlFragmentEvent {
                version: version.to_string(),
                fragment_index,
                sql: sql.to_string(),
                decision: match decision {
                    MysqlApplyDecision::RunBare => MysqlFragmentDecision::RunBare,
                    MysqlApplyDecision::SatisfiedNoop => MysqlFragmentDecision::SatisfiedNoop,
                },
                had_inflight,
            };
            let action = self
                .after_fragment_hook
                .borrow()
                .as_ref()
                .map_or(MysqlFragmentHookAction::Continue, |hook| hook(event));
            if action == MysqlFragmentHookAction::CloseConnection {
                let close_result = self
                    .conn
                    .borrow_mut()
                    .close_for_test(format!(
                        "test hook closed MySQL JS driver after fragment {fragment_index}"
                    ))
                    .await;
                return Err(ApplyError::Db(transport::backend_error(
                    close_result.err().unwrap_or_else(|| {
                        JsDriverError::transport(format!(
                            "test hook closed MySQL JS driver after fragment {fragment_index}"
                        ))
                    }),
                )));
            }
        }
        let _ = (version, fragment_index, sql, decision, had_inflight);
        Ok(())
    }
}

fn validate_mysql_guarded_fragments(
    m: &Migration,
    fragments: Vec<MysqlGuardedFragment>,
) -> Result<Vec<MysqlGuardedFragment>, ApplyError> {
    if fragments.is_empty() {
        return Err(ApplyError::NonIdempotentNonTxn {
            version: m.version.as_str().to_string(),
            reason: "MySQL guarded fragment list is empty".to_string(),
        });
    }
    if let Some((idx, _)) = fragments
        .iter()
        .enumerate()
        .find(|(_, fragment)| fragment.sql.trim().is_empty())
    {
        return Err(ApplyError::NonIdempotentNonTxn {
            version: m.version.as_str().to_string(),
            reason: format!("MySQL guarded fragment #{idx} is empty"),
        });
    }
    let reassembled = fragments
        .iter()
        .map(|fragment| fragment.sql.trim())
        .collect::<Vec<_>>()
        .join(";\n");
    if reassembled != m.up.trim() {
        return Err(ApplyError::NonIdempotentNonTxn {
            version: m.version.as_str().to_string(),
            reason: "MySQL guarded fragments do not reassemble to the migration `up`"
                .to_string(),
        });
    }
    Ok(fragments)
}

pub fn mysql_migration_lock_name(project_id: &str) -> String {
    let digest = Sha256::digest(project_id.as_bytes());
    let suffix = hex::encode(digest);
    format!(
        "{MYSQL_LOCK_PREFIX}{}",
        &suffix[..MYSQL_LOCK_HEX_CHARS.min(suffix.len())]
    )
}

pub fn mysql_migrator_account_user(project_id: &str) -> String {
    let digest = Sha256::digest(project_id.as_bytes());
    let suffix = hex::encode(digest);
    let user = format!(
        "{MYSQL_MIGRATOR_USER_PREFIX}{}",
        &suffix[..MYSQL_MIGRATOR_USER_HEX_CHARS.min(suffix.len())]
    );
    debug_assert!(user.len() <= MYSQL_MIGRATOR_USER_MAX_CHARS);
    user
}

fn mysql_get_lock_timeout_seconds(timeout: Duration) -> String {
    let millis = if timeout.is_zero() {
        0
    } else {
        timeout.as_millis().max(1)
    };
    format!("{}.{:03}", millis / 1000, millis % 1000)
}

fn mysql_lock_timeout_error(lock_name: &str, timeout_seconds: &str) -> ApplyError {
    ApplyError::Db(transport::backend_error(JsDriverError::Remote {
        code: 1205,
        sqlstate: "HY000".to_string(),
        message: format!(
            "timed out waiting for MySQL migration advisory lock {lock_name} after {timeout_seconds}s"
        ),
    }))
}

fn mysql_quote_string_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn mysql_account_sql(user: &str, host: &str) -> String {
    format!(
        "{}@{}",
        mysql_quote_string_lit(user),
        mysql_quote_string_lit(host)
    )
}

fn mysql_generated_password() -> String {
    format!("zs_{}", uuid::Uuid::new_v4().simple())
}

pub async fn provision_mysql_migrator_account(
    admin: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<MysqlMigratorAccount, MysqlMigratorAccountError> {
    provision_mysql_migrator_account_with_password(admin, cfg, mysql_generated_password()).await
}

pub async fn provision_mysql_migrator_account_with_password(
    admin: &MysqlBackend,
    cfg: &ExecutorConfig,
    password: impl Into<String>,
) -> Result<MysqlMigratorAccount, MysqlMigratorAccountError> {
    let password = password.into();
    let user = mysql_migrator_account_user(&cfg.project_id);
    let account = mysql_account_sql(&user, MYSQL_MIGRATOR_HOST);
    let password_lit = mysql_quote_string_lit(&password);

    admin
        .exec(&format!(
            "CREATE USER IF NOT EXISTS {account} IDENTIFIED BY {password_lit}"
        ))
        .await?;
    admin
        .exec(&format!("ALTER USER {account} IDENTIFIED BY {password_lit}"))
        .await?;
    admin
        .exec(&format!(
            "REVOKE ALL PRIVILEGES, GRANT OPTION FROM {account}"
        ))
        .await?;

    ensure_journal_mysql(admin, cfg).await?;

    let project = mysql_quote_ident(&cfg.project_schema)?;
    admin
        .exec(&format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE, CREATE, ALTER, DROP, INDEX, REFERENCES, \
                    CREATE VIEW, SHOW VIEW, TRIGGER, CREATE ROUTINE, ALTER ROUTINE, EVENT
               ON {project}.* TO {account}"
        ))
        .await?;

    for table in [
        "schema_migrations",
        "schema_migrations_inflight",
        "schema_migrations_supersedes",
    ] {
        let table = mysql_meta_table(cfg, table)?;
        admin
            .exec(&format!(
                "GRANT SELECT, INSERT, UPDATE, DELETE ON {table} TO {account}"
            ))
            .await?;
    }

    Ok(MysqlMigratorAccount {
        user,
        host: MYSQL_MIGRATOR_HOST.to_string(),
        password,
    })
}

pub async fn deprovision_mysql_migrator_account(
    admin: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<(), MysqlMigratorAccountError> {
    let user = mysql_migrator_account_user(&cfg.project_id);
    admin
        .exec(&format!(
            "DROP USER IF EXISTS {}",
            mysql_account_sql(&user, MYSQL_MIGRATOR_HOST)
        ))
        .await?;
    Ok(())
}

impl MigrationBackend for MysqlBackend {
    type SessionSnapshot = MysqlSessionSnapshot;

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mysql
    }

    fn ddl_is_transactional(&self) -> bool {
        false
    }

    async fn acquire_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        let lock_name = mysql_migration_lock_name(&cfg.project_id);
        let timeout = mysql_get_lock_timeout_seconds(cfg.pg.lock_timeout);
        let rows = self
            .query_json(
                "SELECT GET_LOCK(?, CAST(? AS DECIMAL(10,3))) AS acquired",
                &[
                    BindValue::Text(lock_name.clone()),
                    BindValue::Decimal(timeout.clone()),
                ],
            )
            .await
            .map_err(apply_db)?;
        let row = rows.rows.first().ok_or_else(|| {
            ApplyError::Backend("mysql GET_LOCK returned no row".to_string())
        })?;
        match value_to_string(row.get("acquired")).as_deref() {
            Some("1") => Ok(()),
            Some("0") => Err(mysql_lock_timeout_error(&lock_name, &timeout)),
            Some(other) => Err(ApplyError::Backend(format!(
                "mysql GET_LOCK returned unexpected value {other:?} for {lock_name}"
            ))),
            None => Err(ApplyError::Backend(format!(
                "mysql GET_LOCK returned NULL for {lock_name}"
            ))),
        }
    }

    async fn release_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        let lock_name = mysql_migration_lock_name(&cfg.project_id);
        let rows = self
            .query_json(
                "SELECT RELEASE_LOCK(?) AS released",
                &[BindValue::Text(lock_name.clone())],
            )
            .await
            .map_err(apply_db)?;
        let row = rows.rows.first().ok_or_else(|| {
            ApplyError::Backend("mysql RELEASE_LOCK returned no row".to_string())
        })?;
        match value_to_string(row.get("released")).as_deref() {
            Some("1") => Ok(()),
            Some(other) => Err(ApplyError::Backend(format!(
                "mysql RELEASE_LOCK returned {other:?} for {lock_name}; lock was not held by this session"
            ))),
            None => Err(ApplyError::Backend(format!(
                "mysql RELEASE_LOCK returned NULL for {lock_name}"
            ))),
        }
    }

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        let rows = self
            .query_json(
                "SELECT CAST(@@innodb_lock_wait_timeout AS CHAR) AS innodb_lock_wait_timeout, \
                        @@sql_mode AS sql_mode",
                &[],
            )
            .await
            .map_err(apply_db)?;
        let row = rows.rows.first().ok_or_else(|| {
            ApplyError::Backend("mysql session snapshot returned no row".to_string())
        })?;
        Ok(MysqlSessionSnapshot {
            innodb_lock_wait_timeout: value_to_string(row.get("innodb_lock_wait_timeout"))
                .unwrap_or_else(|| "50".to_string()),
            sql_mode: value_to_string(row.get("sql_mode")).unwrap_or_default(),
        })
    }

    async fn restore_session(&self, snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        let lock_wait = snap
            .innodb_lock_wait_timeout
            .parse::<i64>()
            .unwrap_or(50);
        self.exec_with_binds(
            "SET SESSION innodb_lock_wait_timeout = ?",
            &[BindValue::Int(lock_wait)],
        )
        .await
        .map_err(apply_db)?;
        self.exec_with_binds(
            "SET SESSION sql_mode = ?",
            &[BindValue::Text(snap.sql_mode.clone())],
        )
        .await
        .map_err(apply_db)?;
        Ok(())
    }

    async fn reset_role_best_effort(&self) {
        // MySQL confinement is by least-privilege account identity, not SET ROLE.
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
        let version = m.version.as_str();
        let fragments = self.mysql_guarded_fragments(m)?;
        record_started_mysql(self, cfg, version, &m.name, m.checksum.as_str(), applied_by)
            .await?;

        let started = Instant::now();
        for (idx, fragment) in fragments.iter().enumerate() {
            let live = snapshot_schema_mysql_for_schema(self, fragment.existence_guard.schema())
                .await
                .map_err(mysql_drift_to_apply)?;
            match crate::render::existence_probe::decide(
                &fragment.existence_guard,
                &live,
                SqlDialect::Mysql,
            ) {
                GuardVerdict::RunBare => {
                    self.exec(&fragment.sql).await.map_err(|e| ApplyError::MigrationFailed {
                        version: version.to_string(),
                        source: transport::backend_error(e),
                    })?;
                    self.after_fragment_decision(
                        version,
                        idx,
                        &fragment.sql,
                        MysqlApplyDecision::RunBare,
                        had_inflight,
                    )
                    .await?;
                }
                GuardVerdict::SatisfiedNoop => {
                    self.after_fragment_decision(
                        version,
                        idx,
                        &fragment.sql,
                        MysqlApplyDecision::SatisfiedNoop,
                        had_inflight,
                    )
                    .await?;
                }
                GuardVerdict::FailDrift(d) => {
                    return Err(ApplyError::ExistenceGuardDrift {
                        version: version.to_string(),
                        object: d.object,
                        field: d.field,
                        expected: d.expected,
                        actual: d.actual,
                    });
                }
            }
        }
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

        let journal_kind = if kind == "squash" { "squash" } else { kind };
        if supersedes.is_empty() {
            record_completed_mysql(
                self,
                cfg,
                journal::CompletedRecord {
                    version,
                    name: &m.name,
                    checksum: m.checksum.as_str(),
                    applied_by,
                    exec_ms,
                    kind: journal_kind,
                },
            )
            .await?;
        } else {
            self.begin_transaction().await.map_err(apply_db)?;
            let result = async {
                record_completed_mysql(
                    self,
                    cfg,
                    journal::CompletedRecord {
                        version,
                        name: &m.name,
                        checksum: m.checksum.as_str(),
                        applied_by,
                        exec_ms,
                        kind: "squash",
                    },
                )
                .await?;
                insert_supersedes_edges_mysql(self, cfg, version, supersedes).await
            }
            .await;
            if let Err(e) = result {
                let _ = self.rollback().await;
                return Err(ApplyError::Journal(e));
            }
            self.commit().await.map_err(apply_db)?;
        }

        Ok(had_inflight)
    }

    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        let Some(down) = m.down.as_deref() else {
            return Err(RollbackError::Backend(format!(
                "mysql backend: migration '{}' has no down SQL",
                m.version.as_str()
            )));
        };
        let started = Instant::now();
        for fragment in mysql_statement_fragments(down) {
            self.exec(&fragment).await.map_err(rollback_db)?;
        }
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        record_rolled_back_mysql(
            self,
            cfg,
            m.version.as_str(),
            &m.name,
            m.checksum.as_str(),
            applied_by,
            exec_ms,
        )
        .await
        .map_err(RollbackError::Journal)
    }

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        self.mysql_guarded_fragments(m).map(|_| ())
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        ensure_journal_mysql(self, cfg).await
    }

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        applied_mysql(self, cfg).await
    }

    async fn superseded_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        superseded_versions_mysql(self, cfg).await
    }

    async fn latest_completed_checksums(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<HashMap<String, String>, JournalError> {
        latest_completed_checksums_mysql(self, cfg).await
    }

    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations> {
        None
    }

    async fn check_checksum_drift(
        &self,
        cfg: &ExecutorConfig,
        migrations: &[Migration],
    ) -> Result<ChecksumDriftReport, DriftError> {
        let applied = applied_mysql(self, cfg).await?;
        Ok(crate::apply::drift::compare_applied_to_set(
            &applied,
            migrations,
        ))
    }

    async fn snapshot_schema(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<SchemaSnapshot, DriftError> {
        snapshot_schema_mysql(self, cfg).await
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
        cfg: &ExecutorConfig,
        version: &MigrationId,
        name: &str,
        template: &str,
        binds: &[BindValue],
        destructive: bool,
        owner_app: &str,
        approval: Approval,
        scope: &ApprovalScope,
        applied_by: &str,
        _lock_mode: LockMode,
    ) -> Result<bool, ApplyError> {
        if destructive && approval != Approval::Approved {
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
        run_dml_transactional_mysql(
            self,
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

fn mysql_quote_ident(ident: &str) -> Result<String, crate::render::dml::IdentQuoteError> {
    crate::render::dml::quote_ident_checked_for_dialect(ident, SqlDialect::Mysql)
}

fn mysql_meta_table(cfg: &ExecutorConfig, table: &str) -> Result<String, JournalError> {
    Ok(format!(
        "{}.{}",
        mysql_quote_ident(&cfg.pg.meta_schema)?,
        mysql_quote_ident(table)?,
    ))
}

async fn ensure_journal_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<(), JournalError> {
    let meta = mysql_quote_ident(&cfg.pg.meta_schema)?;
    if !mysql_schema_exists(backend, &cfg.pg.meta_schema).await? {
        backend
            .exec(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
            .await
            .map_err(journal_backend)?;
    }

    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
    if !mysql_table_exists(backend, &cfg.pg.meta_schema, "schema_migrations").await? {
        backend
            .exec(&format!(
                "CREATE TABLE IF NOT EXISTS {migrations} (
                event_seq BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
                event_kind VARCHAR(32) NOT NULL,
                version VARCHAR(128) NOT NULL,
                name VARCHAR(255) NOT NULL,
                checksum VARCHAR(128) NOT NULL,
                `at` DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                `by` VARCHAR(255) NOT NULL,
                exec_ms BIGINT NULL,
                phase VARCHAR(32) NULL,
                outcome VARCHAR(255) NULL,
                kind VARCHAR(32) NULL,
                CHECK (event_kind IN ('applied','rolled_back')),
                CHECK (phase IS NULL OR phase IN ('started','completed')),
                CHECK (kind IS NULL OR kind IN ('apply','baseline','squash','repeatable'))
            )"
            ))
            .await
            .map_err(journal_backend)?;
    }

    let supersedes = mysql_meta_table(cfg, "schema_migrations_supersedes")?;
    if !mysql_table_exists(backend, &cfg.pg.meta_schema, "schema_migrations_supersedes").await? {
        backend
            .exec(&format!(
                "CREATE TABLE IF NOT EXISTS {supersedes} (
                id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
                squash_version VARCHAR(128) NOT NULL,
                superseded_version VARCHAR(128) NOT NULL,
                recorded_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
            )"
            ))
            .await
            .map_err(journal_backend)?;
    }

    let inflight = mysql_meta_table(cfg, "schema_migrations_inflight")?;
    if !mysql_table_exists(backend, &cfg.pg.meta_schema, "schema_migrations_inflight").await? {
        backend
            .exec(&format!(
                "CREATE TABLE IF NOT EXISTS {inflight} (
                version VARCHAR(128) NOT NULL PRIMARY KEY,
                name VARCHAR(255) NOT NULL,
                checksum VARCHAR(128) NOT NULL,
                started_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                applied_by VARCHAR(255) NOT NULL
            )"
            ))
            .await
            .map_err(journal_backend)?;
    }

    Ok(())
}

async fn mysql_schema_exists(
    backend: &MysqlBackend,
    schema: &str,
) -> Result<bool, JournalError> {
    let rows = backend
        .query_json(
            "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
            &[BindValue::Text(schema.to_string())],
        )
        .await
        .map_err(journal_backend)?;
    Ok(!rows.rows.is_empty())
}

async fn mysql_table_exists(
    backend: &MysqlBackend,
    schema: &str,
    table: &str,
) -> Result<bool, JournalError> {
    let rows = backend
        .query_json(
            "SELECT TABLE_NAME FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            &[
                BindValue::Text(schema.to_string()),
                BindValue::Text(table.to_string()),
            ],
        )
        .await
        .map_err(journal_backend)?;
    Ok(!rows.rows.is_empty())
}

async fn record_started_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    applied_by: &str,
) -> Result<(), JournalError> {
    let inflight = mysql_meta_table(cfg, "schema_migrations_inflight")?;
    backend
        .exec_with_binds(
            &format!(
                "INSERT IGNORE INTO {inflight}
                     (version, name, checksum, applied_by)
                 VALUES (?, ?, ?, ?)"
            ),
            &[
                BindValue::Text(version.to_string()),
                BindValue::Text(name.to_string()),
                BindValue::Text(checksum.to_string()),
                BindValue::Text(applied_by.to_string()),
            ],
        )
        .await
        .map_err(journal_backend)
}

async fn record_completed_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    rec: journal::CompletedRecord<'_>,
) -> Result<(), JournalError> {
    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
    backend
        .exec_with_binds(
            &format!(
                "INSERT INTO {migrations}
                     (event_kind, version, name, checksum, `by`, exec_ms, phase, outcome, kind)
                 VALUES ('applied', ?, ?, ?, ?, ?, 'completed', 'success', ?)"
            ),
            &[
                BindValue::Text(rec.version.to_string()),
                BindValue::Text(rec.name.to_string()),
                BindValue::Text(rec.checksum.to_string()),
                BindValue::Text(rec.applied_by.to_string()),
                BindValue::Int(rec.exec_ms),
                BindValue::Text(rec.kind.to_string()),
            ],
        )
        .await
        .map_err(journal_backend)?;

    let inflight = mysql_meta_table(cfg, "schema_migrations_inflight")?;
    backend
        .exec_with_binds(
            &format!("DELETE FROM {inflight} WHERE version = ?"),
            &[BindValue::Text(rec.version.to_string())],
        )
        .await
        .map_err(journal_backend)
}

async fn run_dml_transactional_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    template: &str,
    binds: &[BindValue],
    owner_app: &str,
    applied_by: &str,
) -> Result<(), ApplyError> {
    let started = Instant::now();
    let checksum = crate::model::migration::Checksum::of(&crate::model::migration::ChecksumInput {
        up: template,
        down: None,
        flags: &crate::model::migration::MigrationFlags::default(),
        owner_app,
        depends_on: &[],
        supersedes: &[],
        preconditions: &[],
    });

    backend.begin_transaction().await.map_err(apply_db)?;
    let result = async {
        backend.exec_with_binds(template, binds).await.map_err(apply_db)?;
        let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        record_completed_mysql(
            backend,
            cfg,
            journal::CompletedRecord {
                version,
                name,
                checksum: checksum.as_str(),
                applied_by,
                exec_ms,
                kind: "apply",
            },
        )
        .await
        .map_err(ApplyError::Journal)
    }
    .await;

    match result {
        Ok(()) => {
            if let Err(e) = backend.commit().await {
                let _ = backend.rollback().await;
                return Err(apply_db(e));
            }
            Ok(())
        }
        Err(e) => {
            if let Err(rb) = backend.rollback().await {
                tracing::warn!(error = %rb, version = %version, "zeroship-migrate: MySQL ROLLBACK failed after DML transaction error");
            }
            Err(e)
        }
    }
}

async fn record_rolled_back_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    version: &str,
    name: &str,
    checksum: &str,
    rolled_back_by: &str,
    exec_ms: i64,
) -> Result<(), JournalError> {
    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
    backend
        .exec_with_binds(
            &format!(
                "INSERT INTO {migrations}
                     (event_kind, version, name, checksum, `by`, exec_ms)
                 VALUES ('rolled_back', ?, ?, ?, ?, ?)"
            ),
            &[
                BindValue::Text(version.to_string()),
                BindValue::Text(name.to_string()),
                BindValue::Text(checksum.to_string()),
                BindValue::Text(rolled_back_by.to_string()),
                BindValue::Int(exec_ms),
            ],
        )
        .await
        .map_err(journal_backend)
}

async fn insert_supersedes_edges_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
    squash_version: &str,
    supersedes: &[&str],
) -> Result<(), JournalError> {
    let table = mysql_meta_table(cfg, "schema_migrations_supersedes")?;
    for sup in supersedes {
        backend
            .exec_with_binds(
                &format!(
                    "INSERT INTO {table} (squash_version, superseded_version)
                     VALUES (?, ?)"
                ),
                &[
                    BindValue::Text(squash_version.to_string()),
                    BindValue::Text((*sup).to_string()),
                ],
            )
            .await
            .map_err(journal_backend)?;
    }
    Ok(())
}

async fn applied_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<Vec<AppliedEntry>, JournalError> {
    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
    let inflight = mysql_meta_table(cfg, "schema_migrations_inflight")?;
    let rows = backend
        .query_json(
            &format!(
                "SELECT version, checksum, mig_kind, phase
                   FROM (
                     SELECT m.version, m.checksum, m.kind AS mig_kind, 'completed' AS phase
                       FROM {migrations} m
                       JOIN (
                         SELECT version, MAX(event_seq) AS event_seq
                           FROM {migrations}
                          GROUP BY version
                       ) latest
                         ON latest.version = m.version AND latest.event_seq = m.event_seq
                      WHERE m.event_kind = 'applied'
                     UNION ALL
                     SELECT i.version, i.checksum, NULL AS mig_kind, 'started' AS phase
                       FROM {inflight} i
                      WHERE NOT EXISTS (
                          SELECT 1
                            FROM {migrations} m
                            JOIN (
                              SELECT version, MAX(event_seq) AS event_seq
                                FROM {migrations}
                               GROUP BY version
                            ) latest
                              ON latest.version = m.version AND latest.event_seq = m.event_seq
                           WHERE m.event_kind = 'applied' AND m.version = i.version
                      )
                   ) net_state
                  ORDER BY BINARY version"
            ),
            &[],
        )
        .await
        .map_err(journal_backend)?;

    let mut out = Vec::with_capacity(rows.rows.len());
    for row in rows.rows {
        let version = required_row_string(&row, "version")?;
        let checksum = required_row_string(&row, "checksum")?;
        let phase_s = required_row_string(&row, "phase")?;
        let phase = journal::Phase::parse(&phase_s).ok_or(JournalError::BadPhase(phase_s))?;
        let kind = optional_row_string(&row, "mig_kind")
            .map(|s| journal::JournaledKind::parse(&s).ok_or(JournalError::BadKind(s)))
            .transpose()?;
        out.push(AppliedEntry {
            version,
            checksum,
            phase,
            kind,
        });
    }
    Ok(out)
}

async fn superseded_versions_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<Vec<String>, JournalError> {
    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
    let supersedes = mysql_meta_table(cfg, "schema_migrations_supersedes")?;
    let rows = backend
        .query_json(
            &format!(
                "SELECT DISTINCT s.superseded_version AS v
                   FROM {supersedes} s
                   JOIN {migrations} m ON m.version = s.squash_version
                   JOIN (
                     SELECT version, MAX(event_seq) AS event_seq
                       FROM {migrations}
                      GROUP BY version
                   ) latest
                     ON latest.version = m.version AND latest.event_seq = m.event_seq
                  WHERE m.event_kind = 'applied' AND m.kind = 'squash'
                  ORDER BY BINARY s.superseded_version"
            ),
            &[],
        )
        .await
        .map_err(journal_backend)?;
    rows.rows
        .iter()
        .map(|row| required_row_string(row, "v"))
        .collect()
}

async fn latest_completed_checksums_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<HashMap<String, String>, JournalError> {
    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
    let rows = backend
        .query_json(
            &format!(
                "SELECT m.version, m.checksum
                   FROM {migrations} m
                   JOIN (
                     SELECT version, MAX(event_seq) AS event_seq
                       FROM {migrations}
                      WHERE event_kind = 'applied' AND kind = 'repeatable'
                      GROUP BY version
                   ) latest
                     ON latest.version = m.version AND latest.event_seq = m.event_seq"
            ),
            &[],
        )
        .await
        .map_err(journal_backend)?;
    let mut out = HashMap::with_capacity(rows.rows.len());
    for row in rows.rows {
        out.insert(
            required_row_string(&row, "version")?,
            required_row_string(&row, "checksum")?,
        );
    }
    Ok(out)
}

async fn snapshot_schema_mysql(
    backend: &MysqlBackend,
    cfg: &ExecutorConfig,
) -> Result<SchemaSnapshot, DriftError> {
    snapshot_schema_mysql_for_schema(backend, &cfg.project_schema).await
}

async fn snapshot_schema_mysql_for_schema(
    backend: &MysqlBackend,
    schema: &str,
) -> Result<SchemaSnapshot, DriftError> {
    let schema_bind = [BindValue::Text(schema.to_string())];
    let tables = backend
        .query_json(
            "SELECT TABLE_NAME, TABLE_COMMENT
               FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_TYPE = 'BASE TABLE'
              ORDER BY TABLE_NAME",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;
    let columns = backend
        .query_json(
            "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE, DATA_TYPE, IS_NULLABLE,
                    COLUMN_COMMENT, ORDINAL_POSITION
               FROM information_schema.COLUMNS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, ORDINAL_POSITION",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;
    let statistics = backend
        .query_json(
            "SELECT TABLE_NAME, INDEX_NAME, NON_UNIQUE, SEQ_IN_INDEX, COLUMN_NAME,
                    INDEX_TYPE, INDEX_COMMENT
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;
    let table_constraints = backend
        .query_json(
            "SELECT TABLE_NAME, CONSTRAINT_NAME, CONSTRAINT_TYPE
               FROM information_schema.TABLE_CONSTRAINTS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, CONSTRAINT_NAME",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;
    let key_column_usage = backend
        .query_json(
            "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, ORDINAL_POSITION,
                    REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
               FROM information_schema.KEY_COLUMN_USAGE
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;
    let views = backend
        .query_json(
            "SELECT TABLE_NAME, VIEW_DEFINITION
               FROM information_schema.VIEWS
              WHERE TABLE_SCHEMA = ?
              ORDER BY TABLE_NAME",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;
    let referential_constraints = backend
        .query_json(
            "SELECT CONSTRAINT_NAME, TABLE_NAME, REFERENCED_TABLE_NAME,
                    UPDATE_RULE, DELETE_RULE
               FROM information_schema.REFERENTIAL_CONSTRAINTS
              WHERE CONSTRAINT_SCHEMA = ?
              ORDER BY CONSTRAINT_NAME",
            &schema_bind,
        )
        .await
        .map_err(drift_backend)?;

    rowsets_to_schema_snapshot(
        schema,
        MysqlCatalogRowSets {
            tables,
            columns,
            statistics,
            table_constraints,
            key_column_usage,
            referential_constraints,
            views,
        },
    )
}

fn mysql_drift_to_apply(error: DriftError) -> ApplyError {
    match error {
        DriftError::Db(db) => ApplyError::Db(db),
        DriftError::Journal(journal) => ApplyError::Journal(journal),
        DriftError::Snapshot(message) | DriftError::Backend(message) => {
            ApplyError::Backend(message)
        }
    }
}

fn mysql_statement_fragments(sql: &str) -> Vec<String> {
    let mut fragments = Vec::new();
    let mut start = 0usize;
    let bytes = sql.as_bytes();
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
    let mut line_comment = false;
    let mut block_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if line_comment {
            if b == b'\n' {
                line_comment = false;
            }
            i += 1;
            continue;
        }
        if block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if let Some(q) = quote {
            if b == b'\\' {
                i = (i + 2).min(bytes.len());
                continue;
            }
            if b == q {
                if bytes.get(i + 1) == Some(&q) && q != b'`' {
                    i += 2;
                    continue;
                }
                quote = None;
            }
            i += 1;
            continue;
        }

        match b {
            b'\'' | b'"' | b'`' => {
                quote = Some(b);
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                line_comment = true;
                i += 2;
            }
            b'#' => {
                line_comment = true;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_comment = true;
                i += 2;
            }
            b';' => {
                let fragment = sql[start..i].trim();
                if !fragment.is_empty() {
                    fragments.push(fragment.to_string());
                }
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    let tail = sql[start..].trim();
    if !tail.is_empty() {
        fragments.push(tail.to_string());
    }
    fragments
}

fn required_row_string(
    row: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, JournalError> {
    optional_row_string(row, key).ok_or_else(|| {
        JournalError::Backend(format!("mysql journal row missing required field {key}"))
    })
}

fn optional_row_string(
    row: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    value_to_string(row.get(key))
}

fn value_to_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::Bool(b)) => Some(b.to_string()),
        Some(serde_json::Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn apply_db(error: JsDriverError) -> ApplyError {
    ApplyError::Db(transport::backend_error(error))
}

fn rollback_db(error: JsDriverError) -> RollbackError {
    RollbackError::Db(transport::backend_error(error))
}

fn journal_backend(error: JsDriverError) -> JournalError {
    JournalError::Db(transport::backend_error(error))
}

fn drift_backend(error: JsDriverError) -> DriftError {
    DriftError::Db(transport::backend_error(error))
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

            backend.exec("select echo lock-free").await.expect("echo exec");
            let rows = backend
                .query_json("select echo", &[BindValue::Int(7)])
                .await
                .expect("echo query");
            assert_eq!(rows.rows[0].get("command_count"), Some(&serde_json::json!(2)));
        });
    }

    #[test]
    fn mysql_lock_name_hashes_long_project_id_to_mysql_limit() {
        let long = format!("prj_{}", "tenant_with_a_very_long_project_identifier_".repeat(8));
        let name = mysql_migration_lock_name(&long);
        assert!(name.starts_with(MYSQL_LOCK_PREFIX), "lock name: {name}");
        assert!(
            name.len() <= 64,
            "MySQL GET_LOCK names are capped at 64 chars, got {}: {name}",
            name.len()
        );
        assert_eq!(
            name.len(),
            MYSQL_LOCK_PREFIX.len() + MYSQL_LOCK_HEX_CHARS,
            "lock name should be fixed-width hashed, not raw project text"
        );
        assert_ne!(
            name,
            mysql_migration_lock_name(&(long + "_other")),
            "different project ids should hash to different lock names"
        );
    }

    #[test]
    fn mysql_migrator_account_user_hashes_project_id_to_mysql_limit() {
        let long = format!("prj_{}", "tenant_with_a_very_long_project_identifier_".repeat(8));
        let cases = ["", "prj_short", "prj_symbols_!@#$%^&*()", long.as_str()];

        for project_id in cases {
            let user = mysql_migrator_account_user(project_id);
            assert!(
                user.starts_with(MYSQL_MIGRATOR_USER_PREFIX),
                "migrator user: {user}"
            );
            assert!(
                user.len() <= MYSQL_MIGRATOR_USER_MAX_CHARS,
                "MySQL user names are capped at 32 chars, got {}: {user}",
                user.len()
            );
            assert_eq!(
                user.len(),
                MYSQL_MIGRATOR_USER_PREFIX.len() + MYSQL_MIGRATOR_USER_HEX_CHARS,
                "migrator user should use its own 32-char cap, not the GET_LOCK cap"
            );
        }

        assert_ne!(
            mysql_migrator_account_user("project-a"),
            mysql_migrator_account_user("project-b"),
            "different project ids should hash to different account names"
        );
    }

    #[test]
    fn mysql_reset_role_best_effort_is_driver_noop() {
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let backend = MysqlBackend::echo().expect("echo backend");
            backend.exec("before reset").await.expect("echo exec");
            backend.reset_role_best_effort().await;
            let rows = backend
                .query_json("after reset", &[])
                .await
                .expect("echo query");
            assert_eq!(
                rows.rows[0].get("command_count"),
                Some(&serde_json::json!(2)),
                "reset_role_best_effort must not issue SET ROLE DEFAULT on MySQL"
            );
            assert_eq!(
                rows.rows[0].get("last_sql"),
                Some(&serde_json::json!("after reset"))
            );
        });
    }

    #[test]
    fn mysql_statement_fragments_ignore_semicolons_inside_literals() {
        assert_eq!(
            mysql_statement_fragments(
                "CREATE TABLE `t` (`v` VARCHAR(32) DEFAULT 'a;b');\n\
                 CREATE INDEX `idx` ON `t` (`v`);"
            ),
            vec![
                "CREATE TABLE `t` (`v` VARCHAR(32) DEFAULT 'a;b')".to_string(),
                "CREATE INDEX `idx` ON `t` (`v`)".to_string(),
            ]
        );
    }

    #[test]
    fn mysql_journal_and_drift_driver_errors_remain_downcastable() {
        let journal = journal_backend(JsDriverError::Remote {
            code: 1146,
            sqlstate: "42S02".to_string(),
            message: "table does not exist".to_string(),
        });
        let JournalError::Db(journal_db) = journal else {
            panic!("expected journal backend errors to route through JournalError::Db");
        };
        assert!(matches!(
            journal_db.downcast_ref::<JsDriverError>(),
            Some(JsDriverError::Remote {
                code: 1146,
                sqlstate,
                ..
            }) if sqlstate == "42S02"
        ));

        let drift = drift_backend(JsDriverError::Remote {
            code: 1054,
            sqlstate: "42S22".to_string(),
            message: "unknown column".to_string(),
        });
        let DriftError::Db(drift_db) = drift else {
            panic!("expected drift backend errors to route through DriftError::Db");
        };
        let apply = ApplyError::Db(drift_db);
        let ApplyError::Db(apply_db) = apply else {
            panic!("expected drift db error to map to ApplyError::Db");
        };
        assert!(matches!(
            apply_db.downcast_ref::<JsDriverError>(),
            Some(JsDriverError::Remote {
                code: 1054,
                sqlstate,
                ..
            }) if sqlstate == "42S22"
        ));
    }
}
