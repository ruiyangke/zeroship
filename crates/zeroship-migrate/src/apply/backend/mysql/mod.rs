//! MySQL [`MigrationBackend`](super::MigrationBackend) implementation.
//!
//! The backend talks to MySQL through the platform-owned Trusted JS driver
//! isolate. The isolate loads the vendored, unmodified `mysql2/promise` bundle
//! and all SQL reaches the server through `connection.execute`.

pub mod snapshot;
pub mod transport;

use std::cell::RefCell;
use std::collections::HashMap;
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

    pub async fn query_json(
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

    async fn acquire_project_lock(&self, _project_id: &str) -> Result<(), ApplyError> {
        // E2b wires MySQL GET_LOCK. E2a keeps the lock capability as an explicit
        // no-op so the live single-process apply path can exercise the rest of
        // the backend without pretending to have confinement/serialization.
        Ok(())
    }

    async fn release_project_lock(&self, _project_id: &str) -> Result<(), ApplyError> {
        Ok(())
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
        record_started_mysql(self, cfg, version, &m.name, m.checksum.as_str(), applied_by)
            .await?;

        let started = Instant::now();
        for fragment in mysql_statement_fragments(&m.up) {
            self.exec(&fragment).await.map_err(|e| ApplyError::MigrationFailed {
                version: version.to_string(),
                source: transport::backend_error(e),
            })?;
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
            self.exec("START TRANSACTION").await.map_err(apply_db)?;
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
                let _ = self.exec("ROLLBACK").await;
                return Err(ApplyError::Journal(e));
            }
            self.exec("COMMIT").await.map_err(apply_db)?;
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

    fn validate_non_txn(&self, _m: &Migration) -> Result<(), ApplyError> {
        Ok(())
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
        self.exec_with_binds(template, binds).await.map_err(apply_db)?;
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
    backend
        .exec(&format!("CREATE SCHEMA IF NOT EXISTS {meta}"))
        .await
        .map_err(journal_backend)?;

    let migrations = mysql_meta_table(cfg, "schema_migrations")?;
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

    let supersedes = mysql_meta_table(cfg, "schema_migrations_supersedes")?;
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

    let inflight = mysql_meta_table(cfg, "schema_migrations_inflight")?;
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

    Ok(())
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
    let schema_bind = [BindValue::Text(cfg.project_schema.clone())];
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

    rowsets_to_schema_snapshot(MysqlCatalogRowSets {
        tables,
        columns,
        statistics,
        table_constraints,
        key_column_usage,
        referential_constraints,
        views,
    })
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
    JournalError::Backend(error.to_string())
}

fn drift_backend(error: JsDriverError) -> DriftError {
    DriftError::Backend(error.to_string())
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
}
