//! MySQL [`MigrationBackend`](zero_migrate_backend::backend::MigrationBackend)
//! implementation.
//!
//! Generic over the dialect-neutral [`SqlSession`](zero_migrate_backend::driver::SqlSession) seam
//! (engine root `zero_migrate_backend::driver`) - a host driver (the napi `mysql2` shell) supplies
//! the `SqlSession` impl, exactly as the `pg` shell does for the PostgreSQL
//! backend, which lives in its own vendor crate. MySQL rides the
//! SAME seam as Postgres; only the dialect SQL (lock, session, journal DDL,
//! placeholders) differs, and all of it lives here + in `session` / `journal_sql`
//! - never in the shared executor (the structural fix that lets MySQL ride the
//! same seam).
//!
//! **MySQL DDL is auto-committing** (an implicit COMMIT brackets every DDL
//! statement), so a migration's `up` cannot commit atomically with its journal
//! row. Every MySQL migration therefore takes the **two-phase non-transactional
//! path** - `ddl_is_transactional()` returns `false`, which routes all versioned
//! migrations through the started-marker -> run-`up` -> completed-row protocol (the
//! generalization of Postgres' `CREATE INDEX CONCURRENTLY` path to every MySQL
//! migration).
//!
//! # Capability surface
//!
//! This first MySQL backend implements the **core apply path**: the `GET_LOCK`
//! project lock, MySQL journal DDL + net-state reads + event writes, the two-phase
//! apply, rollback, session setup, structured one-shot DML, resumable batched
//! backfills, and the (dialect-agnostic) checksum-drift gate over the MySQL
//! journal read, and authoritative table/column/index catalog introspection. It
//! does not implement online expand-contract (`online`), shadow dry-run
//! (`shadow`), the SQLite table rebuild (`rebuild_one`), preconditions,
//! baseline/squash journal-without-running, or the cross-deploy pending-contract
//! partition. These gaps **fail closed** on the MySQL backend and surface a clear
//! `ApplyError::Backend` / capability-absent `None` rather than a silent
//! mis-apply, exactly as the host-PG backend fails closed on its
//! online harness and SQLite fails closed on its capability gaps.

pub(crate) mod alter_column_type_sql;
pub(crate) mod backfill_sql;
pub(crate) mod drift_sql;
pub(crate) mod identity_sql;
pub(crate) mod journal_sql;

/// The journal identity columns that must compare byte-for-byte, re-exported so a
/// host mocking this journal answers the collation probe from the same list the
/// bootstrap checks. Two hand-copied mirrors of it already drifted the moment a
/// table was added.
/// The canned `SqlSession` this backend's tests drive, shared with the engine's
/// integration tests through the `testing` feature. See its own header for why it
/// is not simply `#[cfg(test)]`.
#[cfg(any(test, feature = "testing"))]
pub mod recording;

pub use journal_sql::BINARY_IDENTITY_COLUMNS;
pub(crate) mod primary_key_sql;
pub(crate) mod session;

use zero_migrate_backend::backend::{
    CrossDeployObligations, MigrationBackend, PlaceholderStyle, ProjectLockAcquisition,
    PROJECT_LOCK_TRY_ATTEMPTS, PROJECT_LOCK_TRY_BACKOFF,
};
use zero_migrate_backend::backfill::BackfillSpec;
use zero_migrate_backend::baseline::{BaselineError, BaselineOutcome};
use zero_migrate_backend::capability::OnlineSchemaChange;
use zero_migrate_backend::conn::ExecutorConfig;
use zero_migrate_backend::drift::DriftError;
use zero_migrate_backend::driver::{Row, SqlSession};
use zero_migrate_backend::executor::{ApplyError, RollbackError};
use zero_migrate_backend::journal::{AppliedEntry, JournalError};
use zero_migrate_backend::requirements::{DatabaseFeature, DatabaseRequirements};
use zero_migrate_backend::snapshot::SchemaSnapshot;
use zero_migrate_backend::step::{
    AlterColumnTypeStep, AlterPrimaryKeyStep, BindValue, SynchronizeIdentityStep,
};
use zero_migrate_backend::table_rebuild::TableRebuildSpec;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::migration::{Checksum, Migration, MigrationId};

use crate::DIALECT;

/// The generic MySQL [`MigrationBackend`] implementation.
///
/// Generic over the [`SqlSession`] driver seam. It carries no online/shadow
/// harness (`online()` / `shadow()` are always `None` - the honest v1 gap), and
/// the auto-committing MySQL DDL routes every migration through the two-phase
/// non-transactional apply path.
///
/// The wrapped session must be dedicated to zero-migrate and idle when an
/// operation starts. MySQL's apply envelope pins `autocommit=1`; the snapshot
/// guard rejects an active transaction before that setting or any author SQL can
/// run, preventing an implicit commit of caller-owned work.
#[derive(Debug)]
pub struct MysqlBackend<'a, D: SqlSession> {
    conn: &'a D,
}

/// MySQL session settings overridden while author SQL runs. The backend restores
/// these values on every exit so integrity enforcement, autocommit, `sql_mode`,
/// and timeout budgets never leak onto a pooled or otherwise long-lived caller
/// connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MysqlSessionSnapshot {
    sql_mode: String,
    time_zone: String,
    max_execution_time: i64,
    innodb_lock_wait_timeout: i64,
    information_schema_stats_expiry: i64,
    autocommit: i64,
    foreign_key_checks: i64,
    unique_checks: i64,
}

/// An unmatched MySQL DDL marker, exposed for operator recovery tooling without
/// requiring direct access to zero-migrate's journal tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MysqlInflightDdlMarker {
    /// Stable migration identity.
    pub version: String,
    /// Migration name recorded before author DDL started.
    pub name: String,
    /// Exact reviewed migration checksum.
    pub checksum: String,
    /// Actor that started the interrupted apply.
    pub applied_by: String,
    /// Database-rendered marker timestamp.
    pub started_at: String,
}

/// The two explicit, operator-asserted resolutions for ambiguous auto-committing
/// MySQL DDL. Neither variant executes the migration SQL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MysqlInflightResolution {
    /// The operator verified that the full intended schema shape landed. Append
    /// the normal completion event and clear the marker atomically.
    MarkAppliedAfterVerification,
    /// The operator restored and verified the complete pre-migration shape.
    /// Audit the decision and clear the marker so a later normal apply may retry.
    ClearForRetryAfterRollback,
}

impl MysqlInflightResolution {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MarkAppliedAfterVerification => "mark_applied",
            Self::ClearForRetryAfterRollback => "clear_for_retry",
        }
    }
}

/// Successful, audited resolution of one MySQL inflight DDL marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MysqlInflightRecoveryOutcome {
    /// The exact marker that was locked and verified against the migration.
    pub marker: MysqlInflightDdlMarker,
    /// The committed operator resolution.
    pub resolution: MysqlInflightResolution,
}

/// Error from [`MysqlBackend::recover_inflight_ddl`].
#[derive(Debug, thiserror::Error)]
pub enum MysqlInflightRecoveryError {
    /// Journal bootstrap/read/write failure.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// Project-lock or backend session failure.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// No marker exists for the supplied migration.
    #[error("mysql migration {version} has no inflight DDL marker to recover")]
    NotFound { version: String },
    /// The marker does not identify the exact reviewed migration supplied to the
    /// recovery call.
    #[error(
        "mysql inflight marker for {version} has {field} {recorded:?}, but the reviewed migration has {expected:?}"
    )]
    MarkerMismatch {
        version: String,
        field: &'static str,
        recorded: String,
        expected: String,
    },
    /// Recovery decisions must leave a useful immutable audit record.
    #[error("mysql inflight DDL recovery requires non-empty recovered_by and reason values")]
    MissingAuditContext,
    /// The migration shape cannot map to a valid journal completion kind.
    #[error("mysql inflight DDL recovery received an invalid migration shape: {0}")]
    InvalidMigration(String),
}

/// Decide whether a stable plan-step version is already net-applied. A matching
/// completed checksum is an idempotent skip; a same-version mismatch is tamper
/// drift and must abort before either a DML statement or a backfill resume.
fn completed_step_matches(
    entries: Vec<AppliedEntry>,
    version: &MigrationId,
    checksum: &Checksum,
) -> Result<bool, ApplyError> {
    let completed = entries.into_iter().find(|entry| {
        entry.version == version.as_str()
            && matches!(entry.phase, zero_migrate_backend::journal::Phase::Completed)
    });
    let Some(entry) = completed else {
        return Ok(false);
    };
    if entry.checksum != checksum.as_str() {
        return Err(ApplyError::ChecksumDrift {
            version: version.as_str().to_string(),
            recorded: entry.checksum,
            expected: checksum.as_str().to_string(),
        });
    }
    Ok(true)
}

async fn ensure_transactional_dml_target<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            "SELECT ENGINE AS table_engine
               FROM information_schema.TABLES
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
            &[schema.into(), table.into()],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Err(ApplyError::Backend(format!(
            "mysql DML: target table {schema:?}.{table:?} was not found"
        )));
    };
    let engine: Option<String> = row.try_get("table_engine")?;
    if !engine
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case("InnoDB"))
    {
        return Err(ApplyError::Backend(format!(
            "mysql DML: target table {schema:?}.{table:?} uses nontransactional or unsupported engine {engine:?}; atomic data migration journaling requires InnoDB"
        )));
    }
    ensure_no_user_triggers(conn, schema, table).await?;
    Ok(())
}

/// Return whether the catalog rows describe one full-column unique index whose
/// columns are exactly the authored conflict target. Conflict-target column
/// order is intentionally ignored, matching PostgreSQL's unique-index inference.
/// Prefix indexes and functional index entries cannot prove the authored target.
fn has_exact_unique_conflict_target(
    rows: &[Row],
    target_columns: &[String],
) -> Result<bool, ApplyError> {
    if target_columns.is_empty() {
        return Ok(false);
    }

    let target = target_columns
        .iter()
        .map(|column| column.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    if target.len() != target_columns.len() {
        return Ok(false);
    }

    let mut indexes =
        std::collections::BTreeMap::<String, Vec<(Option<String>, Option<i64>)>>::new();
    for row in rows {
        let index_name: String = row.try_get("index_name")?;
        let column_name: Option<String> = row.try_get("column_name")?;
        let sub_part: Option<i64> = row.try_get("sub_part")?;
        indexes
            .entry(index_name)
            .or_default()
            .push((column_name, sub_part));
    }

    Ok(indexes.into_values().any(|parts| {
        if parts.len() != target.len()
            || parts
                .iter()
                .any(|(column, prefix_length)| column.is_none() || prefix_length.is_some())
        {
            return false;
        }
        let columns = parts
            .into_iter()
            .filter_map(|(column, _)| column)
            .map(|column| column.to_ascii_lowercase())
            .collect::<std::collections::BTreeSet<_>>();
        columns.len() == target.len() && columns == target
    }))
}

/// Prove the MySQL `onConflict` target before its native duplicate-key statement
/// runs. `NON_UNIQUE = 0` includes both PRIMARY and UNIQUE indexes. Reading every
/// part lets the executor reject strict supersets, prefix indexes, and functional
/// entries rather than accepting a partial catalog match.
pub(crate) async fn ensure_exact_unique_conflict_target<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
    target_columns: &[String],
) -> Result<(), ApplyError> {
    let rows = conn
        .query(
            "SELECT INDEX_NAME AS index_name,
                    COLUMN_NAME AS column_name,
                    SUB_PART AS sub_part
               FROM information_schema.STATISTICS
              WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND NON_UNIQUE = 0
              ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            &[schema.into(), table.into()],
        )
        .await?;
    if has_exact_unique_conflict_target(&rows, target_columns)? {
        return Ok(());
    }
    Err(ApplyError::Backend(format!(
        "mysql DML: onConflict target {target_columns:?} on {schema:?}.{table:?} does not exactly match the full columns of one UNIQUE or PRIMARY index"
    )))
}

/// MySQL cannot prove that a user trigger only writes to transactional tables.
/// Fail closed instead of claiming atomic data+journal behavior while a trigger
/// can leave nontransactional side effects behind after rollback.
pub(super) async fn ensure_no_user_triggers<D: SqlSession>(
    conn: &D,
    schema: &str,
    table: &str,
) -> Result<(), ApplyError> {
    let triggers = conn
        .query(
            "SELECT TRIGGER_NAME AS trigger_name
               FROM information_schema.TRIGGERS
              WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ?
              LIMIT 1",
            &[schema.into(), table.into()],
        )
        .await?;
    if let Some(trigger) = triggers.first() {
        let name: String = trigger.try_get("trigger_name")?;
        return Err(ApplyError::Backend(format!(
            "mysql DML: target table {schema:?}.{table:?} has trigger {name:?}; zero-migrate cannot prove transactional side effects, so structured data migrations fail closed"
        )));
    }
    Ok(())
}

async fn restore_after_data_step<D: SqlSession, T>(
    conn: &D,
    snapshot: &MysqlSessionSnapshot,
    result: Result<T, ApplyError>,
) -> Result<T, ApplyError> {
    let restored = session::restore_session(conn, snapshot).await;
    match (result, restored) {
        (Err(error), Err(restore)) => {
            tracing::warn!(
                error = %restore,
                "zero-migrate: failed to restore MySQL session after data-step error"
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
    }
}

impl<'a, D: SqlSession> MysqlBackend<'a, D> {
    /// Wrap any [`SqlSession`] driver as the MySQL backend.
    #[must_use]
    pub fn new_generic(conn: &'a D) -> Self {
        Self { conn }
    }

    /// Resolve one ambiguous MySQL DDL marker through a locked, identity-checked,
    /// append-only operator workflow.
    ///
    /// The caller must first inspect the live schema outside zero-migrate. Supply
    /// the exact reviewed [`Migration`], an explicit resolution whose assertion is
    /// true, the operator identity, and a durable reason. This method then:
    ///
    /// 1. serializes against apply with the project lock;
    /// 2. locks the marker and verifies its version, name, and checksum against
    ///    the supplied migration;
    /// 3. appends an immutable recovery audit row; and
    /// 4. atomically either records normal completion or clears the marker for a
    ///    later normal retry.
    ///
    /// It never executes `migration.up`. A same-version marker for different
    /// content fails closed.
    pub async fn recover_inflight_ddl(
        &self,
        cfg: &ExecutorConfig,
        migration: &Migration,
        resolution: MysqlInflightResolution,
        recovered_by: &str,
        reason: &str,
    ) -> Result<MysqlInflightRecoveryOutcome, MysqlInflightRecoveryError> {
        if recovered_by.trim().is_empty() || reason.trim().is_empty() {
            return Err(MysqlInflightRecoveryError::MissingAuditContext);
        }

        <Self as MigrationBackend>::ensure_journal(self, cfg).await?;
        session::acquire_project_lock(self.conn, cfg, &cfg.project_id).await?;
        let result =
            recover_inflight_locked(self.conn, cfg, migration, resolution, recovered_by, reason)
                .await;
        let unlock = session::release_project_lock(self.conn, &cfg.project_id).await;
        match (result, unlock) {
            (Err(error), Err(unlock)) => {
                tracing::warn!(
                    error = %unlock,
                    "zero-migrate: failed to release MySQL project lock after inflight recovery error"
                );
                Err(error)
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error.into()),
            (Ok(outcome), Ok(())) => Ok(outcome),
        }
    }
}

fn recovery_db(error: zero_migrate_backend::driver::DbError) -> MysqlInflightRecoveryError {
    MysqlInflightRecoveryError::Journal(JournalError::Db(error.into()))
}

async fn recover_inflight_locked<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migration: &Migration,
    resolution: MysqlInflightResolution,
    recovered_by: &str,
    reason: &str,
) -> Result<MysqlInflightRecoveryOutcome, MysqlInflightRecoveryError> {
    conn.batch("START TRANSACTION").await.map_err(recovery_db)?;
    let result = async {
        let version = migration.version.as_str();
        let marker = journal_sql::inflight_for_update(conn, cfg, version)
            .await?
            .ok_or_else(|| MysqlInflightRecoveryError::NotFound {
                version: version.to_string(),
            })?;
        for (field, recorded, expected) in [
            ("version", marker.version.as_str(), version),
            ("name", marker.name.as_str(), migration.name.as_str()),
            (
                "checksum",
                marker.checksum.as_str(),
                migration.checksum.as_str(),
            ),
        ] {
            if recorded != expected {
                return Err(MysqlInflightRecoveryError::MarkerMismatch {
                    version: version.to_string(),
                    field,
                    recorded: recorded.to_string(),
                    expected: expected.to_string(),
                });
            }
        }

        let (kind, supersedes) = if migration.flags.repeatable {
            if !migration.supersedes.is_empty() {
                return Err(MysqlInflightRecoveryError::InvalidMigration(
                    "a repeatable migration cannot carry supersedes edges".to_string(),
                ));
            }
            ("repeatable", Vec::new())
        } else if migration.supersedes.is_empty() {
            ("apply", Vec::new())
        } else {
            (
                "squash",
                migration
                    .supersedes
                    .iter()
                    .map(MigrationId::as_str)
                    .collect::<Vec<_>>(),
            )
        };

        journal_sql::append_recovery_audit(conn, cfg, &marker, resolution, recovered_by, reason)
            .await?;
        match resolution {
            MysqlInflightResolution::MarkAppliedAfterVerification => {
                journal_sql::append_completed(
                    conn,
                    cfg,
                    zero_migrate_backend::journal::CompletedRecord {
                        version,
                        name: &migration.name,
                        checksum: migration.checksum.as_str(),
                        applied_by: recovered_by,
                        exec_ms: 0,
                        kind,
                        down: migration.down.as_deref(),
                    },
                )
                .await?;
                session::insert_supersedes_edges(conn, cfg, version, &supersedes).await?;
                journal_sql::clear_inflight(conn, cfg, version).await?;
            }
            MysqlInflightResolution::ClearForRetryAfterRollback => {
                journal_sql::clear_inflight(conn, cfg, version).await?;
            }
        }
        Ok(MysqlInflightRecoveryOutcome { marker, resolution })
    }
    .await;

    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Err(rollback) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback,
                    version = %migration.version.as_str(),
                    "zero-migrate: MySQL ROLLBACK failed after inflight recovery error"
                );
            }
            return Err(error);
        }
    };
    conn.batch("COMMIT").await.map_err(recovery_db)?;
    Ok(outcome)
}

fn parse_mysql_version(raw: &str) -> Result<[u32; 3], String> {
    if raw.to_ascii_lowercase().contains("mariadb") {
        return Err(format!("connected server reports MariaDB version {raw:?}"));
    }
    let core = raw.split('-').next().unwrap_or(raw);
    let mut components = core.split('.');
    let parsed = [
        components.next().and_then(|value| value.parse().ok()),
        components.next().and_then(|value| value.parse().ok()),
        components.next().and_then(|value| value.parse().ok()),
    ];
    let [Some(major), Some(minor), Some(patch)] = parsed else {
        return Err(format!(
            "MySQL returned an unrecognized server version {raw:?}"
        ));
    };
    Ok([major, minor, patch])
}

impl<D: SqlSession> MigrationBackend for MysqlBackend<'_, D> {
    type SessionSnapshot = MysqlSessionSnapshot;

    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn timeout_setting_names(&self) -> Option<(&'static str, &'static str)> {
        Some(("max_execution_time", "innodb_lock_wait_timeout"))
    }

    fn preserves_authored_logical_columns(&self) -> bool {
        // MySQL's engine path consumes the refreshed catalog snapshot directly;
        // it does not maintain the logical-column side projection.
        false
    }

    fn projects_sdk_field_defs(&self) -> bool {
        // MySQL has no SDK-shaped table-rebuild projection in the engine.
        false
    }

    async fn verify_database_requirements(
        &self,
        requirements: &DatabaseRequirements,
    ) -> Result<(), ApplyError> {
        if requirements.is_empty() {
            return Ok(());
        }
        if requirements
            .iter()
            .any(|feature| feature == DatabaseFeature::UuidV7Generation)
        {
            return Err(ApplyError::Backend(
                "exact RFC 9562 UUIDv7 database generation is unsupported on MySQL; generate UUIDv7 values in the application"
                    .to_string(),
            ));
        }

        let needs_uuid_v4 = requirements
            .iter()
            .any(|feature| feature == DatabaseFeature::UuidV4Generation);
        let needs_uuid_validation = requirements
            .iter()
            .any(|feature| feature == DatabaseFeature::UuidValidation);
        let needs_type_id = requirements
            .iter()
            .any(|feature| feature == DatabaseFeature::TypeIdValidation);
        let needs_ulid = requirements
            .iter()
            .any(|feature| feature == DatabaseFeature::UlidValidation);

        let capabilities = session::database_capabilities(self.conn).await?;
        let (minimum, requirement) = if needs_uuid_validation || needs_type_id || needs_ulid {
            let requirement = match (needs_uuid_validation, needs_type_id, needs_ulid) {
                (true, false, false) => "canonical UUID format validation",
                (false, true, false) => "canonical TypeID format validation",
                (false, false, true) => "canonical ULID format validation",
                (false, true, true) => "canonical TypeID and ULID format validation",
                (true, true, false) => "canonical UUID and TypeID format validation",
                (true, false, true) => "canonical UUID and ULID format validation",
                (true, true, true) => "canonical UUID, TypeID, and ULID format validation",
                (false, false, false) => unreachable!("format-validation branch is guarded"),
            };
            ([8, 0, 16], requirement)
        } else {
            ([8, 0, 13], "exact RFC 9562 UUIDv4 database generation")
        };
        let version = parse_mysql_version(&capabilities.server_version).map_err(|detail| {
            ApplyError::Backend(format!(
                "{requirement} requires MySQL {}.{}.{} or newer; {detail}",
                minimum[0], minimum[1], minimum[2]
            ))
        })?;
        if version < minimum {
            return Err(ApplyError::Backend(format!(
                "{requirement} requires MySQL {}.{}.{} or newer; connected server reports {:?}",
                minimum[0], minimum[1], minimum[2], capabilities.server_version
            )));
        }
        if !needs_uuid_v4 {
            return Ok(());
        }
        if !capabilities
            .innodb_support
            .as_deref()
            .is_some_and(|support| {
                support.eq_ignore_ascii_case("YES") || support.eq_ignore_ascii_case("DEFAULT")
            })
        {
            return Err(ApplyError::Backend(format!(
                "exact RFC 9562 UUIDv4 database generation requires InnoDB support; connected server reports information_schema.ENGINES.SUPPORT={:?}",
                capabilities.innodb_support
            )));
        }
        if !capabilities
            .default_storage_engine
            .eq_ignore_ascii_case("InnoDB")
        {
            return Err(ApplyError::Backend(format!(
                "exact RFC 9562 UUIDv4 database generation requires @@SESSION.default_storage_engine=InnoDB; connected session reports {:?}",
                capabilities.default_storage_engine
            )));
        }

        for (setting, format) in [
            (
                "@@GLOBAL.binlog_format",
                capabilities.global_binlog_format.as_str(),
            ),
            (
                "@@SESSION.binlog_format",
                capabilities.session_binlog_format.as_str(),
            ),
        ] {
            if format.eq_ignore_ascii_case("ROW") {
                continue;
            }
            let reason = if format.eq_ignore_ascii_case("STATEMENT") {
                "statement-based replication can independently evaluate the nondeterministic default on a replica"
            } else if format.eq_ignore_ascii_case("MIXED") {
                "MIXED does not provide the explicit ongoing row-based deployment guarantee required by the UUID default contract"
            } else {
                "the UUID default contract supports only explicit row-based replication"
            };
            return Err(ApplyError::Backend(format!(
                "exact RFC 9562 UUIDv4 database generation requires {setting}=ROW; connected server reports {format:?}: {reason}"
            )));
        }
        Ok(())
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
        session::acquire_project_lock(self.conn, cfg, &cfg.project_id).await
    }

    async fn release_project_lock(&self, cfg: &ExecutorConfig) -> Result<(), ApplyError> {
        session::release_project_lock(self.conn, &cfg.project_id).await
    }

    /// One `GET_LOCK(name, 0)` per attempt, with a small fixed number of attempts
    /// so a reader that arrives during the brief gap between two of a deploy's
    /// statements still gets a real answer instead of reporting contention that has
    /// already cleared.
    ///
    /// The holder probe is best-effort. `performance_schema` needs a `SELECT` grant
    /// that a least-privilege migrator account is routinely not given, and failing
    /// the read over an unnamed holder would exit 1 on a contended run -- the exact
    /// false CI failure this method exists to remove. An unidentified holder is
    /// reported as no holder, and the caller says the session could not be named.
    async fn try_acquire_project_lock(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<ProjectLockAcquisition, ApplyError> {
        for attempt in 1..=PROJECT_LOCK_TRY_ATTEMPTS {
            if session::try_acquire_project_lock(self.conn, &cfg.project_id).await? {
                return Ok(ProjectLockAcquisition::Acquired);
            }
            if attempt < PROJECT_LOCK_TRY_ATTEMPTS {
                // The seam is one verb at a time over one pinned session, driven on
                // a thread that has nothing else to run, so parking it is the whole
                // cost of the pause.
                std::thread::sleep(PROJECT_LOCK_TRY_BACKOFF);
            }
        }
        let holders = session::project_lock_holders(self.conn, &cfg.project_id)
            .await
            .unwrap_or_default();
        Ok(ProjectLockAcquisition::Busy(holders))
    }

    async fn snapshot_session(&self) -> Result<Self::SessionSnapshot, ApplyError> {
        session::snapshot_session(self.conn).await
    }

    async fn restore_session(&self, snap: &Self::SessionSnapshot) -> Result<(), ApplyError> {
        session::restore_session(self.conn, snap).await
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
        // A repeatable rides the same two-phase path on MySQL because DDL
        // auto-commits, but it must retain `kind='repeatable'` so the next deploy's
        // checksum oracle can see and skip an unchanged definition.
        session::configure_session(self.conn, cfg, m).await?;
        session::apply_two_phase(
            self.conn,
            cfg,
            m,
            applied_by,
            had_inflight,
            supersedes,
            kind,
        )
        .await
    }

    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        // "Transactional" is the trait's name for the rollback entry, not a promise
        // this backend can keep: on MySQL the `down` auto-commits statement by
        // statement, the same two-phase reality as apply. What IS transactional is the
        // pair that follows it - the `rolled_back` append and the marker clear commit
        // together. The trait method name is kept for a single generic caller.
        session::rollback_one(self.conn, cfg, m, applied_by).await
    }

    async fn rollback_plan_transactional(
        &self,
        cfg: &ExecutorConfig,
        forward: &Migration,
        inverse_steps: &[zero_migrate_backend::step::PlanStep],
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        session::rollback_dml_plan_transactional(self.conn, cfg, forward, inverse_steps, applied_by)
            .await
    }

    /// MySQL has no statement that is refused inside a transaction the way
    /// PostgreSQL refuses the CONCURRENTLY family. Its DDL commits implicitly
    /// instead, which is a different problem and not this one.
    fn non_transactional_down_reason(&self, _m: &Migration) -> Option<String> {
        None
    }

    fn validate_non_txn(&self, _m: &Migration) -> Result<(), ApplyError> {
        // The PG non-txn idempotency scan (forbid bare DML, require IF NOT EXISTS on
        // CONCURRENTLY) is Postgres-specific (`pg_query`-parsed). MySQL accepts a
        // fresh `up` here, but its recovery path never assumes that generated
        // auto-committing DDL is replayable: an unmatched inflight marker is
        // preserved and apply fails closed with operator repair guidance.
        Ok(())
    }

    async fn journal_exists(&self, cfg: &ExecutorConfig) -> Result<bool, JournalError> {
        let rows = self
            .conn
            .query(
                "SELECT 1 AS journal_exists
                   FROM information_schema.TABLES
                  WHERE TABLE_SCHEMA = ?
                    AND TABLE_NAME = 'schema_migrations'
                    AND TABLE_TYPE = 'BASE TABLE'
                  LIMIT 1",
                &[cfg.confinement.meta_schema.as_str().into()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        session::ensure_idle_for_journal(self.conn).await?;
        session::acquire_journal_bootstrap_lock(self.conn, cfg, &cfg.project_id).await?;
        let result = journal_sql::ensure_journal(self.conn, cfg).await;
        let unlock = session::release_journal_bootstrap_lock(self.conn, &cfg.project_id).await;
        match (result, unlock) {
            (Err(error), Err(unlock)) => {
                tracing::warn!(
                    error = %unlock,
                    "zero-migrate: failed to release MySQL journal bootstrap lock after bootstrap error"
                );
                Err(error)
            }
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    async fn unresolved_rollback_markers(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<zero_migrate_backend::executor::RollbackMarker>, JournalError> {
        journal_sql::unresolved_rollback_markers(self.conn, cfg).await
    }

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal_sql::applied(self.conn, cfg).await
    }

    async fn history(
        &self,
        _cfg: &ExecutorConfig,
    ) -> Result<Vec<zero_migrate_backend::journal::HistoryEvent>, JournalError> {
        // MySQL's journal table records the events; what it has never had is the
        // READER that projects them into `HistoryEvent`. Refusing by name is the
        // honest posture: an empty Vec would be indistinguishable from a project
        // with no history at all, and this is an AUDIT surface - a caller that gets
        // a silent empty log cannot tell "nothing happened" from "I cannot see what
        // happened". Adding the reader is a MySQL change, not a core one.
        Err(JournalError::Backend(
            "mysql backend: the append-only journal audit trail (history) has no MySQL reader"
                .to_string(),
        ))
    }

    async fn net_rolled_back_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal_sql::net_rolled_back_versions(self.conn, cfg).await
    }

    async fn backfill_progress(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<zero_migrate_backend::backfill::BackfillProgressEntry>, JournalError> {
        backfill_sql::read_progress_entries(self.conn, cfg).await
    }

    async fn superseded_versions(&self, cfg: &ExecutorConfig) -> Result<Vec<String>, JournalError> {
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
    ) -> Result<zero_migrate_backend::drift::ChecksumDriftReport, DriftError> {
        // The drift/tamper comparison is dialect-agnostic
        // (`compare_applied_to_set`); only the journal read underneath is
        // dialect-coupled. Read the net-applied state through the MySQL journal and
        // run the SAME shared comparison the PG/SQLite backends use, so the
        // repeatable-exemption / kind-mismatch / tamper rules never diverge across
        // dialects.
        let applied = journal_sql::applied(self.conn, cfg).await?;
        Ok(zero_migrate_backend::drift::compare_applied_to_set(
            &applied, migrations,
        ))
    }

    async fn snapshot_schema(&self, cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        drift_sql::snapshot_schema_for(self.conn, &cfg.project_schema).await
    }

    async fn evaluate_preconditions(
        &self,
        _cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<zero_migrate_backend::executor::PreconditionVerdict, ApplyError> {
        // The executor calls this for EVERY migration, precondition-bearing or not.
        // A migration with NO preconditions needs no evaluator at all - evaluating an
        // empty list is `AllMet` by construction (exactly what the PG evaluator's
        // `evaluate_all` returns for an empty `m.preconditions`), so it must apply
        // normally on MySQL rather than trip the v1 capability gap.
        if m.preconditions.is_empty() {
            return Ok(zero_migrate_backend::executor::PreconditionVerdict::AllMet);
        }
        // A GENUINE precondition (boolean-SELECT probes gated by the `pg_query`
        // parser + PG-flavoured catalog reads) has no MySQL-native evaluator yet, so
        // a MySQL migration that DECLARES preconditions is refused (fail closed)
        // rather than silently treated as satisfied. Wiring a MySQL evaluator needs
        // live-MySQL tests for every probe shape, which is why the refusal stands
        // rather than a partial evaluator.
        Err(ApplyError::Backend(
            "mysql backend: precondition evaluation is not supported on MySQL".to_string(),
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
        // MySQL (it shares the baseline machinery below). The FRESH-path
        // squash - where the squash's `up` DOES run - is handled inline by
        // `apply_two_phase` (non-empty `supersedes`), so this refusal is only the
        // existing-DB record-not-run variant.
        Err(ApplyError::Backend(
            "mysql backend: existing-DB squash (record-not-run) is not supported on MySQL"
                .to_string(),
        ))
    }

    async fn rebuild_one(
        &self,
        spec: &TableRebuildSpec,
        _m: &Migration,
        _scope: &zero_migrate_backend::approval::ApprovalScope,
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

    async fn alter_primary_key(
        &self,
        cfg: &ExecutorConfig,
        step: &AlterPrimaryKeyStep,
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        primary_key_sql::alter_primary_key(self.conn, cfg, step, approval, scope, applied_by).await
    }

    async fn alter_column_type(
        &self,
        cfg: &ExecutorConfig,
        step: &AlterColumnTypeStep,
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        alter_column_type_sql::alter_column_type(self.conn, cfg, step, approval, scope, applied_by)
            .await
    }

    async fn synchronize_identity(
        &self,
        cfg: &ExecutorConfig,
        step: &SynchronizeIdentityStep,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        identity_sql::synchronize_identity(self.conn, cfg, step, applied_by).await
    }

    async fn run_backfill_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        checksum: &Checksum,
        spec: &BackfillSpec,
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: zero_migrate_backend::executor::LockMode,
    ) -> Result<zero_migrate_backend::executor::ApplyOutcome, ApplyError> {
        if completed_step_matches(self.applied(cfg).await?, version, checksum)? {
            return Ok(zero_migrate_backend::executor::ApplyOutcome {
                applied: Vec::new(),
                skipped: vec![version.as_str().to_string()],
                recovered: Vec::new(),
            });
        }

        // A pending backfill mutates table data. Refuse before progress
        // bootstrap or a target-table read. A completed matching step above is
        // an idempotent skip and does not need renewed approval.
        if approval != zero_migrate_backend::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }

        let snapshot = session::snapshot_session(self.conn).await?;
        let result =
            backfill_sql::run_backfill(self.conn, cfg, version, checksum, spec, applied_by).await;
        let outcome = restore_after_data_step(self.conn, &snapshot, result).await?;
        Ok(zero_migrate_backend::executor::ApplyOutcome {
            applied: if outcome.complete {
                vec![version.as_str().to_string()]
            } else {
                Vec::new()
            },
            skipped: Vec::new(),
            recovered: Vec::new(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_dml_step(
        &self,
        cfg: &ExecutorConfig,
        version: &MigrationId,
        checksum: &Checksum,
        name: &str,
        template: &str,
        binds: &[BindValue],
        target_schema: &str,
        target_table: &str,
        conflict_target: Option<&[String]>,
        mutates_data: bool,
        destructive: bool,
        _owner_app: &str,
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: zero_migrate_backend::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        if completed_step_matches(self.applied(cfg).await?, version, checksum)? {
            return Ok(false);
        }

        if destructive && approval != zero_migrate_backend::approval::Approval::Approved {
            return Err(ApplyError::ApprovalRequired);
        }
        if destructive && !scope.admits(version.as_str()) {
            return Err(ApplyError::ApprovalNotScoped {
                version: version.as_str().to_string(),
            });
        }

        let snapshot = session::snapshot_session(self.conn).await?;
        let result = session::apply_dml_transactional(
            self.conn,
            cfg,
            version.as_str(),
            checksum,
            name,
            template,
            binds,
            target_schema,
            target_table,
            mutates_data,
            conflict_target,
            applied_by,
        )
        .await;
        restore_after_data_step(self.conn, &snapshot, result).await?;
        Ok(true)
    }

    fn online(&self) -> Option<&dyn OnlineSchemaChange> {
        // No online expand-contract harness on MySQL.
        //
        // This used to justify itself with "the differ emits no renames for MySQL, so
        // `renames` is empty - no online path is reached". That was FALSE, and
        // measured false against a live server: the differ's rename author was
        // dialect-blind, so a hinted MySQL rename planned an `ExpandContract` (then
        // spelled `PgExpandContract`, a name the same investigation disproved), hit
        // this `None` mid-apply, and left the deploy half-applied. The claim is true
        // NOW because the differ refuses the rename at plan time
        // (`DeclarativeError::ColumnRenameRefused`) rather than because
        // anything about this `None` changed. Keep the two in step: re-enabling MySQL
        // renames means giving this seam a real harness, not relaxing the refusal.
        None
    }

    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations> {
        // No cross-deploy pending-contract partition on MySQL (the online
        // rename that opens obligations is unsupported here). `None` => reads are
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
        // machinery that is PG-specific, none of which MySQL has an equivalent for.
        Err(BaselineError::Backend(
            "mysql backend: schema baseline/adoption is not supported on MySQL".to_string(),
        ))
    }
}

/// UNIT / render tests for the MySQL backend.
///
/// These assert the **generated MySQL SQL** - `GET_LOCK`/`RELEASE_LOCK` for the
/// project lock, MySQL journal DDL (`AUTO_INCREMENT`, `CURRENT_TIMESTAMP(6)`,
/// `CREATE DATABASE`, InnoDB, SIGNAL-based immutability triggers), and the `?`
/// placeholder style on every journaled write - **WITHOUT a live MySQL server**.
/// A host-shaped [`RecordingSession`] records the SQL + binds of every verb and
/// returns canned rows for the reads (`GET_LOCK -> 1`, trigger-existence -> empty),
/// so a full lock + `ensure_journal` + apply sweep runs generically over a
/// non-compio driver and every emitted statement is inspected. The live-MySQL e2e
/// lives in the host CLI suite, gated on the `ZERO_MIGRATE_MYSQL_URL` env var.
///
/// That the live coverage sits there rather than in `crates/zero-migrate/tests/`
/// beside the live-PostgreSQL scenarios is a dependency fact, not an oversight, and
/// the asymmetry is worth stating so it is not read as a hole. [`MysqlBackend`] has
/// exactly one constructor - [`MysqlBackend::new_generic`], over the
/// [`SqlSession`](zero_migrate_backend::driver::SqlSession) trait - and this crate depends on no
/// MySQL client; `postgres` is a dev-dependency carried solely so `tests/support`'s
/// `PgDevSession` can drive the PG scenarios. A Rust-side MySQL harness would have
/// to add a client and a second `SqlSession` over it in order to re-prove what the
/// host suite already proves through the SHIPPED one: `zero-migrate-node`'s bridge
/// builds this same `MysqlBackend::new_generic` over the `mysql2` seam, so a live
/// apply or rollback from the CLI runs the code in this file against a real server.
/// The added driver would also sit between the test and the engine, where a bug in
/// it is indistinguishable from a bug here.
#[cfg(test)]
mod render_tests {
    use super::recording::*;
    use super::*;
    use zero_migrate_backend::backend::ProjectLockHolder;
    use zero_migrate_backend::driver::{Bind, Row, Value};
    use zero_migrate_ir::probe::{GuardDir, GuardProbe};

    /// The backend reports the MySQL dialect, the `?` placeholder style, and
    /// non-transactional DDL (auto-commit => two-phase path for every migration).
    #[test]
    fn backend_reports_mysql_dialect_and_question_placeholders() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        assert_eq!(backend.dialect(), DIALECT);
        assert_eq!(
            backend.timeout_setting_names(),
            Some(("max_execution_time", "innodb_lock_wait_timeout"))
        );
        assert!(!backend.preserves_authored_logical_columns());
        assert!(!backend.projects_sdk_field_defs());
        assert_eq!(backend.placeholder_style(), PlaceholderStyle::Question);
        assert!(
            !backend.ddl_is_transactional(),
            "MySQL DDL auto-commits — must report non-transactional (two-phase path)"
        );
        // The `?` placeholder renders positionally regardless of index.
        assert_eq!(backend.placeholder(1), "?");
        assert_eq!(backend.placeholder(7), "?");
    }

    #[compio::test]
    async fn uuid_v4_requirements_gate_mysql_version_and_innodb() {
        let empty = RecordingSession::with_uuid_capabilities(
            "8.0.12",
            "MyISAM",
            None,
            "STATEMENT",
            "STATEMENT",
        );
        MysqlBackend::new_generic(&empty)
            .verify_database_requirements(&DatabaseRequirements::default())
            .await
            .expect("empty requirements perform no MySQL capability probe");
        assert!(
            empty.log.borrow().is_empty(),
            "empty requirements must not query the server"
        );

        for version in ["8.0.13", "8.4.2-commercial"] {
            let supported = RecordingSession::with_uuid_capabilities(
                version,
                "InnoDB",
                Some("DEFAULT"),
                "ROW",
                "ROW",
            );
            MysqlBackend::new_generic(&supported)
                .verify_database_requirements(&requirements(DatabaseFeature::UuidV4Generation))
                .await
                .unwrap_or_else(|error| panic!("MySQL {version} should pass: {error}"));
            assert_eq!(
                supported
                    .log
                    .borrow()
                    .iter()
                    .filter(|entry| entry.contains("VERSION() AS server_version"))
                    .count(),
                1
            );
        }

        let old = RecordingSession::with_uuid_capabilities(
            "8.0.12",
            "InnoDB",
            Some("DEFAULT"),
            "ROW",
            "ROW",
        );
        let error = MysqlBackend::new_generic(&old)
            .verify_database_requirements(&requirements(DatabaseFeature::UuidV4Generation))
            .await
            .expect_err("MySQL before 8.0.13 must fail closed");
        let message = error.to_string();
        assert!(message.contains("8.0.13"), "got: {message}");
        assert!(message.contains("8.0.12"), "got: {message}");

        let mariadb = RecordingSession::with_uuid_capabilities(
            "10.11.9-MariaDB",
            "InnoDB",
            Some("DEFAULT"),
            "ROW",
            "ROW",
        );
        let error = MysqlBackend::new_generic(&mariadb)
            .verify_database_requirements(&requirements(DatabaseFeature::UuidV4Generation))
            .await
            .expect_err("MariaDB is not a supported MySQL 8 target");
        assert!(error.to_string().contains("MariaDB"), "got: {error}");

        for (default_engine, support, expected) in [
            ("MyISAM", Some("YES"), "default_storage_engine"),
            ("InnoDB", Some("DISABLED"), "InnoDB support"),
            ("InnoDB", None, "InnoDB support"),
        ] {
            let unsupported = RecordingSession::with_uuid_capabilities(
                "8.0.36",
                default_engine,
                support,
                "ROW",
                "ROW",
            );
            let error = MysqlBackend::new_generic(&unsupported)
                .verify_database_requirements(&requirements(DatabaseFeature::UuidV4Generation))
                .await
                .expect_err("unsupported/default non-InnoDB engines must fail closed");
            assert!(error.to_string().contains(expected), "got: {error}");
        }
    }

    #[compio::test]
    async fn uuid_v4_requirements_accept_only_row_binlog_formats() {
        for (global, session, expected) in [
            ("STATEMENT", "ROW", "statement-based replication"),
            ("MIXED", "ROW", "ongoing row-based"),
            ("ROW", "STATEMENT", "statement-based replication"),
            ("ROW", "MIXED", "ongoing row-based"),
        ] {
            let unsupported = RecordingSession::with_uuid_capabilities(
                "8.0.36",
                "InnoDB",
                Some("DEFAULT"),
                global,
                session,
            );
            let error = MysqlBackend::new_generic(&unsupported)
                .verify_database_requirements(&requirements(DatabaseFeature::UuidV4Generation))
                .await
                .expect_err("non-ROW replication must fail closed");
            let message = error.to_string();
            assert!(message.contains(expected), "got: {message}");
            assert!(
                message.contains("binlog_format") && message.contains("ROW"),
                "the remediation must be explicit: {message}"
            );
        }
    }

    #[compio::test]
    async fn type_id_validation_requires_mysql_8_0_16_only() {
        let old = RecordingSession::with_uuid_capabilities(
            "8.0.15",
            "MyISAM",
            None,
            "STATEMENT",
            "STATEMENT",
        );
        let error = MysqlBackend::new_generic(&old)
            .verify_database_requirements(&requirements(DatabaseFeature::TypeIdValidation))
            .await
            .expect_err("MySQL before enforced CHECK constraints must fail closed");
        let message = error.to_string();
        assert!(message.contains("TypeID"), "got: {message}");
        assert!(message.contains("8.0.16"), "got: {message}");
        assert!(message.contains("8.0.15"), "got: {message}");

        // TypeID validation needs enforced CHECK constraints, not UUIDv4's
        // InnoDB/default-engine/row-replication guarantees.
        let current = RecordingSession::with_uuid_capabilities(
            "8.0.16",
            "MyISAM",
            None,
            "STATEMENT",
            "MIXED",
        );
        MysqlBackend::new_generic(&current)
            .verify_database_requirements(&requirements(DatabaseFeature::TypeIdValidation))
            .await
            .expect("MySQL 8.0.16+ enforces the TypeID CHECK independently of UUID generation");
        assert_eq!(
            current
                .log
                .borrow()
                .iter()
                .filter(|entry| entry.contains("VERSION() AS server_version"))
                .count(),
            1
        );
    }

    #[compio::test]
    async fn ulid_validation_requires_mysql_8_0_16_only() {
        let old = RecordingSession::with_uuid_capabilities(
            "8.0.15",
            "MyISAM",
            None,
            "STATEMENT",
            "STATEMENT",
        );
        let error = MysqlBackend::new_generic(&old)
            .verify_database_requirements(&requirements(DatabaseFeature::UlidValidation))
            .await
            .expect_err("MySQL before enforced CHECK constraints must fail closed");
        let message = error.to_string();
        assert!(message.contains("ULID"), "got: {message}");
        assert!(message.contains("8.0.16"), "got: {message}");
        assert!(message.contains("8.0.15"), "got: {message}");

        let mut both_formats = DatabaseRequirements::default();
        both_formats.require(DatabaseFeature::TypeIdValidation);
        both_formats.require(DatabaseFeature::UlidValidation);
        let error = MysqlBackend::new_generic(&old)
            .verify_database_requirements(&both_formats)
            .await
            .expect_err("both text formats share the enforced-CHECK version floor");
        let message = error.to_string();
        assert!(
            message.contains("TypeID") && message.contains("ULID"),
            "got: {message}"
        );

        let current = RecordingSession::with_uuid_capabilities(
            "8.0.16",
            "MyISAM",
            None,
            "STATEMENT",
            "MIXED",
        );
        MysqlBackend::new_generic(&current)
            .verify_database_requirements(&requirements(DatabaseFeature::UlidValidation))
            .await
            .expect("MySQL 8.0.16+ enforces the ULID CHECK independently of UUID generation");
        assert_eq!(
            current
                .log
                .borrow()
                .iter()
                .filter(|entry| entry.contains("VERSION() AS server_version"))
                .count(),
            1
        );
    }

    #[compio::test]
    async fn uuid_v7_requirement_fails_without_querying_mysql() {
        let rec = RecordingSession::new();
        let error = MysqlBackend::new_generic(&rec)
            .verify_database_requirements(&requirements(DatabaseFeature::UuidV7Generation))
            .await
            .expect_err("MySQL never provides UUIDv7 database generation");
        assert!(error.to_string().contains("unsupported on MySQL"));
        assert!(rec.log.borrow().is_empty());
    }

    #[compio::test]
    async fn snapshot_schema_rejects_inconsistent_composite_foreign_key_ordinals() {
        let rec = RecordingSession::with_catalog(
            vec![Row::new(
                vec!["table_name".into()],
                vec![Value::Text("nodes".into())],
            )],
            vec![
                catalog_column("nodes", "tenant_id", "bigint", None, None, false, 1),
                catalog_column("nodes", "node_id", "bigint", None, None, false, 2),
            ],
            Vec::new(),
            vec![catalog_foreign_key_part(
                "nodes",
                "nodes_parent_fkey",
                2,
                2,
                "node_id",
                "proj_x",
                "nodes",
                "node_id",
                "NO ACTION",
                "RESTRICT",
            )],
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let error = backend
            .snapshot_schema(&cfg)
            .await
            .expect_err("a composite FK whose first row is ordinal two is inconsistent");
        assert!(
            error
                .to_string()
                .contains("inconsistent foreign-key metadata for nodes.nodes_parent_fkey"),
            "got: {error}"
        );
    }

    /// `project_lock_name` derives a `zero_migrate:`-prefixed lock name and folds a
    /// too-long id to a deterministic 64-char SHA-256 hex (MySQL's lock-name cap).
    #[test]
    fn project_lock_name_prefixes_and_folds_to_64_chars() {
        // Short id -> prefixed verbatim.
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

    #[test]
    fn conflict_target_requires_one_exact_full_column_unique_index() {
        let composite = vec![
            unique_index_part("uq_tenant_code", Some("tenant_id"), None),
            unique_index_part("uq_tenant_code", Some("code"), None),
        ];
        assert!(
            has_exact_unique_conflict_target(&composite, &["code".into(), "tenant_id".into()],)
                .unwrap()
        );
        assert!(!has_exact_unique_conflict_target(&composite, &["tenant_id".into()]).unwrap());
        assert!(!has_exact_unique_conflict_target(
            &composite,
            &["tenant_id".into(), "code".into(), "region".into()],
        )
        .unwrap());

        let prefix = vec![unique_index_part("uq_code_prefix", Some("code"), Some(8))];
        assert!(!has_exact_unique_conflict_target(&prefix, &["code".into()]).unwrap());

        let functional = vec![unique_index_part("uq_lower_code", None, None)];
        assert!(!has_exact_unique_conflict_target(&functional, &["code".into()]).unwrap());
    }

    /// The project lock rides `GET_LOCK(?, ?)` with the derived name bound (not
    /// interpolated); release rides `RELEASE_LOCK(?)`.
    #[compio::test]
    async fn project_lock_uses_get_lock_release_lock_with_bound_name() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        backend
            .acquire_project_lock(&cfg)
            .await
            .expect("acquire GET_LOCK");
        backend
            .release_project_lock(&cfg)
            .await
            .expect("release RELEASE_LOCK");

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
            rec.binds.borrow().iter().any(|b| b
                .iter()
                .any(|v| matches!(v, Bind::Text(t) if t == "zero_migrate:prj_x"))),
            "the project lock name is bound, not interpolated: {:?}",
            rec.binds.borrow()
        );
    }

    /// The read-only acquisition never waits: `GET_LOCK(?, 0)` fails immediately
    /// instead of spending the ten second budget `acquire_project_lock` waits out.
    ///
    /// The bounded form is asserted ABSENT rather than merely outranked, so a
    /// regression back to the waiting acquisition fails here instead of passing on
    /// the `GET_LOCK` substring both spellings share. An uncontended run also
    /// probes for no holder, because there is none to name.
    #[compio::test]
    async fn try_project_lock_uses_a_zero_timeout_get_lock_and_probes_no_holder() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let outcome = backend
            .try_acquire_project_lock(&cfg)
            .await
            .expect("contention is an outcome, not an error");
        assert_eq!(outcome, ProjectLockAcquisition::Acquired);

        let log = rec.log.borrow();
        assert!(
            log.iter().any(|s| s.contains("GET_LOCK(?, 0)")),
            "a read-only acquisition takes the lock with a zero timeout: {log:?}"
        );
        assert!(
            !log.iter().any(|s| s.contains("GET_LOCK(?, ?)")),
            "a read must never spend the deploy lock's ten second budget: {log:?}"
        );
        assert!(
            !log.iter().any(|s| s.contains("performance_schema")),
            "an uncontended acquisition has no holder to probe for: {log:?}"
        );
    }

    /// A contended acquisition reports the holder and stops, in a bounded number of
    /// attempts.
    ///
    /// The attempt count is asserted exactly: retrying until the lock frees is the
    /// unbounded wait this path exists to remove, only spelled with more round
    /// trips.
    #[compio::test]
    async fn try_project_lock_reports_the_holder_when_a_peer_holds_it() {
        let rec = RecordingSession::with_contended_project_lock();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let outcome = backend
            .try_acquire_project_lock(&cfg)
            .await
            .expect("contention is an outcome, not an error");
        let ProjectLockAcquisition::Busy(holders) = outcome else {
            panic!("a held lock must report busy, not acquired");
        };
        assert_eq!(
            holders,
            vec![ProjectLockHolder {
                pid: HOLDER_CONNECTION_ID,
                application_name: Some("deployer@10.0.0.7".to_string()),
                state: Some("Query: altering table".to_string()),
                query: Some("ALTER TABLE widgets ADD COLUMN name VARCHAR(64)".to_string()),
            }],
            "the busy outcome names the connection id KILL takes, not the internal \
             performance-schema thread id"
        );

        let log = rec.log.borrow();
        assert_eq!(
            log.iter().filter(|s| s.contains("GET_LOCK(?, 0)")).count(),
            3,
            "the retry is bounded at three attempts, never a loop until acquired: {log:?}"
        );
        assert!(
            !log.iter().any(|s| s.contains("RELEASE_LOCK")),
            "nothing was locked, so there is nothing to release: {log:?}"
        );
        // The lock name reaches the holder probe as a bind too: the probe matches
        // `metadata_locks.OBJECT_NAME` against this project's own lock rather than
        // reporting whoever holds any user-level lock on the server.
        assert!(
            rec.binds.borrow().iter().any(|b| b
                .iter()
                .any(|v| matches!(v, Bind::Text(t) if t == "zero_migrate:prj_x"))),
            "the holder probe is scoped to this project's lock name: {:?}",
            rec.binds.borrow()
        );
    }

    /// A holder probe the migrator account is not granted must not fail the read.
    ///
    /// `performance_schema` needs a `SELECT` grant a least-privilege migrator
    /// routinely lacks, and turning that denial into an error would exit 1 on a
    /// contended run -- the exact false CI failure the non-waiting acquisition
    /// exists to remove. The contention is still reported; only the holder's name
    /// is lost.
    #[compio::test]
    async fn a_denied_holder_probe_still_reports_contention() {
        let rec = RecordingSession::with_contended_project_lock();
        *rec.fail_once_contains.borrow_mut() = Some("performance_schema".to_string());
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let outcome = backend
            .try_acquire_project_lock(&cfg)
            .await
            .expect("a denied holder probe is not a failed acquisition");
        assert_eq!(
            outcome,
            ProjectLockAcquisition::Busy(Vec::new()),
            "an unidentified holder is reported as no holder, never as an error"
        );
    }

    /// `ensure_journal` emits the MySQL journal DDL: `CREATE DATABASE IF NOT
    /// EXISTS`, the `schema_migrations` table with `BIGINT AUTO_INCREMENT PRIMARY
    /// KEY` + `TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6)` + `ENGINE=InnoDB`, the
    /// `_supersedes` + `_inflight` tables, and `SIGNAL SQLSTATE '45000'`
    /// immutability triggers - never any Postgres-flavoured `GENERATED ALWAYS AS
    /// IDENTITY` / `TIMESTAMPTZ` / plpgsql.
    #[compio::test]
    async fn ensure_journal_emits_mysql_dialect_ddl() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        backend
            .ensure_journal(&cfg)
            .await
            .expect("ensure_journal DDL");

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
        let create_journal = log
            .iter()
            .find(|entry| {
                entry.contains("CREATE TABLE IF NOT EXISTS")
                    && entry.contains("schema_migrations (")
            })
            .expect("fresh journal table DDL");
        assert!(
            create_journal.contains("down") && create_journal.contains("LONGTEXT"),
            "fresh MySQL journal stores a nullable LONGTEXT reverse: {create_journal}"
        );
        assert!(
            log.iter().any(|entry| {
                entry.contains("information_schema.COLUMNS")
                    && entry.contains("COLUMN_NAME = 'down'")
            }),
            "bootstrap probes legacy journals for the reverse column: {log:?}"
        );
        assert!(
            log.iter().any(|entry| {
                entry.contains("ALTER TABLE `proj_x_migrations`.schema_migrations")
                    && entry.contains("ADD COLUMN down LONGTEXT NULL")
            }),
            "a legacy MySQL journal gains nullable reverse SQL: {log:?}"
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
            all.contains("UNIQUE KEY schema_migrations_supersedes_edge_uq")
                && all.contains("information_schema.STATISTICS")
                && all.contains("ALTER TABLE `proj_x_migrations`.schema_migrations_supersedes")
                && all.contains("ADD UNIQUE KEY schema_migrations_supersedes_edge_uq"),
            "fresh and existing journals enforce one row per squash edge: {all}"
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
        assert!(
            all.contains("GET_LOCK(?, ?)") && all.contains("RELEASE_LOCK(?)"),
            "journal bootstrap must be serialized for apply and read-only status callers: {all}"
        );
    }

    #[compio::test]
    async fn journal_bootstrap_failure_releases_its_serialization_lock() {
        let rec = RecordingSession::with_failure("CREATE DATABASE IF NOT EXISTS");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let result = backend.ensure_journal(&cfg).await;

        assert!(matches!(result, Err(JournalError::Db(_))), "{result:?}");
        let log = rec.log.borrow();
        let acquire = log
            .iter()
            .position(|entry| entry.contains("GET_LOCK(?, ?)"))
            .expect("bootstrap lock acquired");
        let failure = log
            .iter()
            .position(|entry| entry.contains("CREATE DATABASE IF NOT EXISTS"))
            .expect("bootstrap attempted");
        let release = log
            .iter()
            .position(|entry| entry.contains("RELEASE_LOCK(?)"))
            .expect("bootstrap lock released");
        assert!(acquire < failure && failure < release, "{log:?}");
    }

    #[compio::test]
    async fn existing_supersession_edge_key_is_not_added_twice() {
        let rec = RecordingSession::with_edge_index(vec![
            edge_index_part(0, 1, Some("squash_version"), None),
            edge_index_part(0, 2, Some("superseded_version"), None),
        ]);
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        backend
            .ensure_journal(&cfg)
            .await
            .expect("existing journal bootstrap succeeds");

        let log = rec.log.borrow();
        assert!(
            !log.iter().any(|entry| {
                entry.contains("ALTER TABLE `proj_x_migrations`.schema_migrations_supersedes")
            }),
            "the metadata probe must make the journal upgrade idempotent: {log:?}"
        );
    }

    #[compio::test]
    async fn malformed_named_supersession_edge_key_fails_closed() {
        let rec = RecordingSession::with_edge_index(vec![
            edge_index_part(1, 1, Some("squash_version"), None),
            edge_index_part(1, 2, Some("superseded_version"), None),
        ]);
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let result = backend.ensure_journal(&cfg).await;

        assert!(
            matches!(result, Err(JournalError::Backend(ref message))
                if message.contains("exists but is not the exact full-column UNIQUE")),
            "{result:?}"
        );
    }

    #[compio::test]
    async fn legacy_journal_identity_collations_are_upgraded_to_utf8mb4_bin() {
        let rec = RecordingSession::with_legacy_journal_collations();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        backend
            .ensure_journal(&cfg)
            .await
            .expect("legacy journal upgrades safely");

        let all = rec.log.borrow().join("\n");
        // Iterate the production list rather than a copy of it, so a table added to
        // the upgrade set cannot be added without this test demanding its repair.
        for (table, column) in journal_sql::BINARY_IDENTITY_COLUMNS {
            assert!(
                all.contains(&format!(
                    "ALTER TABLE `proj_x_migrations`.`{table}` MODIFY COLUMN `{column}` VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NOT NULL"
                )),
                "missing binary identity upgrade for {table}.{column}: {all}"
            );
        }
    }

    /// A full two-phase apply of one migration through the backend records the
    /// journal writes with `?` placeholders (never `$N`): the exact `started`
    /// `INSERT ... VALUES (?, ?, ?, ?)`, the creator `up` as a `batch`, the
    /// `completed` `INSERT ... VALUES ('applied', ?, ?, ?, ?, ?, 'completed',
    /// 'success', ?)`, and the inflight `DELETE ... WHERE version = ?`.
    #[compio::test]
    async fn two_phase_apply_writes_journal_with_question_placeholders() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        let recovered = backend
            .apply_one(&cfg, &m, "tester", false, &[], "apply")
            .await
            .expect("two-phase apply runs");
        assert!(!recovered, "a fresh apply recovered no inflight marker");

        let log = rec.log.borrow();
        let all = log.join("\n");
        // The inherited modes are preserved while safe literal and strict error
        // behavior is pinned, then the MySQL session budgets are set.
        assert!(
            all.contains("SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode")
                && all.contains(
                    "'NO_BACKSLASH_ESCAPES', 'STRICT_ALL_TABLES', 'ERROR_FOR_DIVISION_BY_ZERO', 'NO_AUTO_VALUE_ON_ZERO'"
                )
                && all.contains("SESSION time_zone = '+00:00'")
                && all.contains("SESSION max_execution_time")
                && all.contains("innodb_lock_wait_timeout")
                && all.contains("SESSION information_schema_stats_expiry = 0")
                && all.contains("SESSION autocommit = 1")
                && all.contains("SESSION foreign_key_checks = 1")
                && all.contains("SESSION unique_checks = 1"),
            "MySQL literal mode, strict errors, integrity checks, autocommit, cursor time zone, and session budgets must be pinned together: {all}"
        );
        assert!(
            !all.contains("search_path") && !all.contains("SET LOCAL ROLE"),
            "no Postgres search_path / SET ROLE on the MySQL apply path: {all}"
        );
        // The started marker is an exact insert under the held project lock.
        assert!(
            all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight")
                && all.contains("VALUES (?, ?, ?, ?)"),
            "started marker is a plain INSERT with ? placeholders: {all}"
        );
        assert!(
            !all.contains("INSERT IGNORE INTO `proj_x_migrations`.schema_migrations_inflight"),
            "INSERT IGNORE must not hide truncation, duplicate, or warning states: {all}"
        );
        // The creator up ran as a batch.
        assert!(
            log.iter().any(|s| s == "batch: CREATE TABLE t (id INT)"),
            "the creator up ran through batch: {log:?}"
        );
        let settings = log
            .iter()
            .position(|s| s.contains("NO_BACKSLASH_ESCAPES"))
            .expect("literal mode is pinned");
        let up = log
            .iter()
            .position(|s| s == "batch: CREATE TABLE t (id INT)")
            .expect("author up runs");
        assert!(
            settings < up,
            "literal mode must be pinned before author SQL: {log:?}"
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
            all.contains(
                "DELETE FROM `proj_x_migrations`.schema_migrations_inflight WHERE version = ?"
            ),
            "inflight clear is a DELETE ... WHERE version = ?: {all}"
        );
        // No Postgres `$N` placeholders anywhere on the MySQL write path.
        assert!(
            !all.contains("$1") && !all.contains("$2") && !all.contains("$3"),
            "no Postgres $N placeholders leak onto the MySQL apply path: {all}"
        );
    }

    #[compio::test]
    async fn guarded_create_against_absent_mysql_object_runs_after_the_probe() {
        let rec = RecordingSession::with_catalog(
            vec![catalog_table("users")],
            vec![catalog_column(
                "users",
                "email",
                "varchar(191)",
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                false,
                1,
            )],
            Vec::new(),
            Vec::new(),
        );
        let backend = MysqlBackend::new_generic(&rec);
        let effective = crate::test_fixtures::operator_with_data_security(
            &["proj_x", "reporting"],
            &[],
            false,
            zero_migrate_ir::policy::DestructiveOps::Allow,
        );
        let cfg = ExecutorConfig::new("prj_x", "proj_x", effective);
        let up = "CREATE INDEX users_email_idx ON reporting.users (email)";
        let migration = guarded_migration(
            up,
            GuardProbe::Index {
                schema: "reporting".into(),
                table: "users".into(),
                name: "users_email_idx".into(),
                direction: GuardDir::IfNotExists,
                expect: Some((false, vec!["email".into()])),
                ownership_only: false,
            },
        );

        backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await
            .expect("an absent guarded object must run the create");

        let log = rec.log.borrow();
        let snapshot = log
            .iter()
            .position(|entry| entry.contains("SELECT VERSION() AS server_version"))
            .expect("the MySQL existence probe must snapshot the live schema");
        let started = log
            .iter()
            .position(|entry| {
                entry.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight")
            })
            .expect("the running create must arm its inflight marker");
        let ddl = log
            .iter()
            .position(|entry| entry == &format!("batch: {up}"))
            .expect("the absent object must let the create run");
        assert!(snapshot < started && started < ddl, "{log:?}");
        assert!(
            rec.binds
                .borrow()
                .iter()
                .any(|binds| binds == &[Bind::Text("reporting".into())]),
            "the allowed cross-schema probe must snapshot reporting"
        );
    }

    #[compio::test]
    async fn guarded_create_against_present_mysql_object_noops_and_skips_ddl() {
        let rec = RecordingSession::with_catalog(
            vec![catalog_table("users")],
            vec![catalog_column(
                "users",
                "email",
                "varchar(191)",
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                false,
                1,
            )],
            vec![catalog_index_part(
                "users",
                "users_email_idx",
                1,
                1,
                Some("email"),
                None,
                Some("A"),
                None,
            )],
            Vec::new(),
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "CREATE INDEX users_email_idx ON proj_x.users (email)";
        let migration = guarded_migration(
            up,
            GuardProbe::Index {
                schema: "proj_x".into(),
                table: "users".into(),
                name: "users_email_idx".into(),
                direction: GuardDir::IfNotExists,
                expect: Some((false, vec!["email".into()])),
                ownership_only: false,
            },
        );

        backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await
            .expect("a matching present object must resolve as satisfied");

        let all = rec.log.borrow().join("\n");
        assert!(all.contains("SELECT VERSION() AS server_version"), "{all}");
        assert!(!all.contains(&format!("batch: {up}")), "{all}");
        assert!(
            !all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight"),
            "a satisfied no-op must not arm an inflight DDL marker: {all}"
        );
        assert!(
            all.contains("'applied', ?, ?, ?, ?, ?, 'completed', 'success', ?"),
            "a satisfied no-op must still journal completion: {all}"
        );
    }

    #[compio::test]
    async fn guarded_drop_against_present_mysql_object_runs_after_the_probe() {
        let rec = RecordingSession::with_catalog(
            vec![catalog_table("users")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "DROP TABLE proj_x.users";
        let migration = guarded_migration(
            up,
            GuardProbe::Table {
                schema: "proj_x".into(),
                table: "users".into(),
                direction: GuardDir::IfExists,
                expect_columns: Vec::new(),
            },
        );

        backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await
            .expect("a present guarded object must run the drop");

        let log = rec.log.borrow();
        let snapshot = log
            .iter()
            .position(|entry| entry.contains("SELECT VERSION() AS server_version"))
            .expect("the MySQL existence probe must snapshot the live schema");
        let ddl = log
            .iter()
            .position(|entry| entry == &format!("batch: {up}"))
            .expect("the present object must let the drop run");
        assert!(snapshot < ddl, "{log:?}");
    }

    #[compio::test]
    async fn guarded_drop_against_absent_mysql_object_noops_and_skips_ddl() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "DROP TABLE proj_x.users";
        let migration = guarded_migration(
            up,
            GuardProbe::Table {
                schema: "proj_x".into(),
                table: "users".into(),
                direction: GuardDir::IfExists,
                expect_columns: Vec::new(),
            },
        );

        backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await
            .expect("an absent guarded object must resolve as satisfied");

        let all = rec.log.borrow().join("\n");
        assert!(all.contains("SELECT VERSION() AS server_version"), "{all}");
        assert!(!all.contains(&format!("batch: {up}")), "{all}");
        assert!(
            !all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight"),
            "a satisfied no-op must not arm an inflight DDL marker: {all}"
        );
        assert!(
            all.contains("'applied', ?, ?, ?, ?, ?, 'completed', 'success', ?"),
            "a satisfied no-op must still journal completion: {all}"
        );
    }

    #[compio::test]
    async fn mysql_existence_probe_outside_policy_scope_is_refused_before_snapshot() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "DROP TABLE forbidden.users";
        let migration = guarded_migration(
            up,
            GuardProbe::Table {
                schema: "forbidden".into(),
                table: "users".into(),
                direction: GuardDir::IfExists,
                expect_columns: Vec::new(),
            },
        );
        let expected_version = migration.version.as_str().to_string();

        let result = backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await;

        match result {
            Err(ApplyError::ExistenceGuardSchemaOutOfScope {
                version,
                probe_schema,
            }) => {
                assert_eq!(version, expected_version);
                assert_eq!(probe_schema, "forbidden");
            }
            other => {
                panic!("expected ExistenceGuardSchemaOutOfScope for the MySQL probe, got {other:?}")
            }
        }
        let all = rec.log.borrow().join("\n");
        assert!(!all.contains("SELECT VERSION() AS server_version"), "{all}");
        assert!(!all.contains(&format!("batch: {up}")), "{all}");
    }

    #[compio::test]
    async fn mysql_guard_refuses_a_decision_that_needs_column_type_equality() {
        let rec = RecordingSession::with_catalog(
            vec![catalog_table("users")],
            vec![catalog_column(
                "users",
                "email",
                "varchar(64)",
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                false,
                1,
            )],
            Vec::new(),
            Vec::new(),
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "ALTER TABLE proj_x.users ADD COLUMN email VARCHAR(255) NOT NULL";
        let migration = guarded_migration(
            up,
            GuardProbe::Column {
                schema: "proj_x".into(),
                table: "users".into(),
                column: "email".into(),
                direction: GuardDir::IfNotExists,
                expect: Some(("character varying(255)".into(), false)),
            },
        );
        let expected_version = migration.version.as_str().to_string();

        let result = backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await;

        match result {
            Err(ApplyError::ExistenceGuardDrift {
                version,
                object,
                field,
                actual,
                ..
            }) => {
                assert_eq!(version, expected_version);
                assert_eq!(object, "column users.email");
                assert_eq!(field, "data_type");
                assert_eq!(
                    actual,
                    "<unknown: MySQL column-type equality is not implemented yet>"
                );
            }
            other => panic!("expected a typed MySQL column-type refusal, got {other:?}"),
        }
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("SELECT VERSION() AS server_version"), "{all}");
        assert!(
            !all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight"),
            "the refusal must happen before record_started: {all}"
        );
        assert!(!all.contains(&format!("batch: {up}")), "{all}");

        let table_rec = RecordingSession::with_catalog(
            vec![catalog_table("users")],
            vec![catalog_column(
                "users",
                "email",
                "varchar(64)",
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                false,
                1,
            )],
            Vec::new(),
            Vec::new(),
        );
        let table_backend = MysqlBackend::new_generic(&table_rec);
        let table_up = "CREATE TABLE proj_x.users (email VARCHAR(255) NOT NULL)";
        let table_migration = guarded_migration(
            table_up,
            GuardProbe::Table {
                schema: "proj_x".into(),
                table: "users".into(),
                direction: GuardDir::IfNotExists,
                expect_columns: vec![zero_migrate_ir::probe::ExpectColumn {
                    name: "email".into(),
                    data_type: "character varying(255)".into(),
                    nullable: false,
                }],
            },
        );
        let table_version = table_migration.version.as_str().to_string();

        let table_result = table_backend
            .apply_one(&cfg, &table_migration, "tester", false, &[], "apply")
            .await;

        match table_result {
            Err(ApplyError::ExistenceGuardDrift {
                version,
                object,
                field,
                actual,
                ..
            }) => {
                assert_eq!(version, table_version);
                assert_eq!(object, "table users");
                assert_eq!(field, "data_type");
                assert_eq!(
                    actual,
                    "<unknown: MySQL column-type equality is not implemented yet>"
                );
            }
            other => panic!("expected a typed MySQL table-type refusal, got {other:?}"),
        }
        let table_log = table_rec.log.borrow().join("\n");
        assert!(
            table_log.contains("SELECT VERSION() AS server_version"),
            "{table_log}"
        );
        assert!(
            !table_log.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight"),
            "the refusal must happen before record_started: {table_log}"
        );
        assert!(
            !table_log.contains(&format!("batch: {table_up}")),
            "{table_log}"
        );
    }

    #[compio::test]
    async fn mysql_guard_carries_an_unresolved_constraint_refusal_to_the_operator() {
        let rec = RecordingSession::with_catalog(
            vec![catalog_table("users")],
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "ALTER TABLE proj_x.users DROP CHECK users_age_check";
        let migration = guarded_migration(
            up,
            GuardProbe::Constraint {
                schema: "proj_x".into(),
                table: "users".into(),
                name: "users_age_check".into(),
                direction: GuardDir::IfExists,
                expect_kind: None,
                expect_definition: None,
            },
        );

        let result = backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await;

        match result {
            Err(ApplyError::ExistenceGuardDrift {
                object,
                field,
                expected,
                actual,
                ..
            }) => {
                assert_eq!(object, "constraint users_age_check");
                assert_eq!(field, "presence");
                assert_eq!(expected, "<absent>");
                assert!(
                    actual.contains("constraint scope excludes arbitrary CHECK identities"),
                    "{actual}"
                );
            }
            other => panic!("expected the unresolved MySQL constraint refusal, got {other:?}"),
        }
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("SELECT VERSION() AS server_version"), "{all}");
        assert!(
            !all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight"),
            "the refusal must happen before record_started: {all}"
        );
        assert!(!all.contains(&format!("batch: {up}")), "{all}");
    }

    #[compio::test]
    async fn mysql_inflight_refusal_precedes_the_existence_probe() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let up = "DROP TABLE proj_x.users";
        let migration = guarded_migration(
            up,
            GuardProbe::Table {
                schema: "proj_x".into(),
                table: "users".into(),
                direction: GuardDir::IfExists,
                expect_columns: Vec::new(),
            },
        );

        let result = backend
            .apply_one(&cfg, &migration, "tester", true, &[], "apply")
            .await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message)) if message.contains("inflight marker")),
            "{result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(!all.contains("SELECT VERSION() AS server_version"), "{all}");
        assert!(!all.contains(&format!("batch: {up}")), "{all}");
    }

    /// MySQL reads `max_execution_time = 0` and `innodb_lock_wait_timeout = 0` as
    /// "no limit" exactly as PostgreSQL reads its two timeouts, so a zero override
    /// is refused on the MySQL backend as well, before the session pin, and so
    /// before the first byte of author SQL. Both fields, and the executor-config
    /// source that no IR validation can see.
    #[compio::test]
    async fn a_zero_timeout_budget_is_refused_before_any_mysql_sql_is_sent() {
        for (label, mutate) in [
            (
                "lock_timeout_ms",
                Box::new(|m: &mut Migration, _cfg: &mut ExecutorConfig| {
                    m.flags.lock_timeout_ms = Some(0);
                }) as Box<dyn Fn(&mut Migration, &mut ExecutorConfig)>,
            ),
            (
                "timeout_ms",
                Box::new(|m: &mut Migration, _cfg: &mut ExecutorConfig| {
                    m.flags.timeout_ms = Some(0);
                }),
            ),
            (
                "confinement.lock_timeout",
                Box::new(|_m: &mut Migration, cfg: &mut ExecutorConfig| {
                    // A sub-millisecond config budget truncates to zero whole
                    // milliseconds, with no migration flag and no IR involved.
                    cfg.confinement.lock_timeout = std::time::Duration::from_micros(500);
                }),
            ),
        ] {
            let rec = RecordingSession::new();
            let backend = MysqlBackend::new_generic(&rec);
            let mut cfg =
                ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
            let mut m = trivial_migration();
            mutate(&mut m, &mut cfg);

            let result = backend
                .apply_one(&cfg, &m, "tester", false, &[], "apply")
                .await;
            let error = result.expect_err("a zero budget disables the timeout and is refused");
            assert!(
                matches!(error, ApplyError::IndefiniteTimeout(_)),
                "{label}: {error:?}"
            );
            assert!(
                error.to_string().contains(label),
                "{label}: the refusal must name the knob that produced the zero: {error}"
            );
            // The knob it names must be one a MySQL operator can find. The
            // executor-config budgets are read by MySQL (`max_execution_time` /
            // `innodb_lock_wait_timeout`) as well as by PostgreSQL, so the
            // refusal must not send a MySQL operator hunting for a
            // PostgreSQL-named field.
            assert!(
                !error.to_string().contains("pg."),
                "{label}: a MySQL refusal must not name a PostgreSQL-scoped knob: {error}"
            );
            assert!(
                rec.log.borrow().is_empty(),
                "{label}: nothing may reach the server: {:?}",
                rec.log.borrow()
            );
        }
    }

    /// The positive control for the MySQL leg: a finite override on both budgets
    /// still applies, and the session pin carries the author's own numbers
    /// (milliseconds for the execution budget, whole seconds for the lock wait).
    #[compio::test]
    async fn finite_mysql_timeout_overrides_still_apply_and_reach_the_session_pin() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let mut m = trivial_migration();
        m.flags.timeout_ms = Some(45_000);
        m.flags.lock_timeout_ms = Some(7_000);

        backend
            .apply_one(&cfg, &m, "tester", false, &[], "apply")
            .await
            .expect("a finite maintenance-window override still applies");

        let log = rec.log.borrow();
        let all = log.join("\n");
        assert!(
            all.contains("SESSION max_execution_time = 45000")
                && all.contains("SESSION innodb_lock_wait_timeout = 7"),
            "the finite overrides must reach the session pin: {all}"
        );
        assert!(
            log.iter().any(|s| s == "batch: CREATE TABLE t (id INT)"),
            "the author up still runs: {log:?}"
        );
    }

    #[compio::test]
    async fn overlong_inflight_marker_fields_fail_before_author_ddl() {
        for field in ["name", "applied_by"] {
            let rec = RecordingSession::new();
            let backend = MysqlBackend::new_generic(&rec);
            let cfg =
                ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
            let mut migration = trivial_migration();
            let mut applied_by = "tester".to_string();
            if field == "name" {
                migration.name = "界".repeat(256);
            } else {
                applied_by = "界".repeat(256);
            }

            let result = backend
                .apply_one(&cfg, &migration, &applied_by, false, &[], "apply")
                .await;

            assert!(
                matches!(result, Err(ApplyError::Journal(JournalError::Backend(ref message)))
                    if message.contains(field)
                        && message.contains("256 characters")
                        && message.contains("maximum is 255")),
                "{field}: {result:?}"
            );
            let all = rec.log.borrow().join("\n");
            assert!(
                !all.contains("schema_migrations_inflight")
                    && !all.contains("batch: CREATE TABLE t (id INT)"),
                "invalid marker input must fail before marker or author DDL: {all}"
            );
        }
    }

    #[compio::test]
    async fn inflight_marker_requires_exactly_one_inserted_row_before_ddl() {
        let rec = RecordingSession::with_zero_affected(
            "INSERT INTO `proj_x_migrations`.schema_migrations_inflight",
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let migration = trivial_migration();

        let result = backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await;

        assert!(
            matches!(result, Err(ApplyError::Journal(JournalError::Backend(ref message)))
                if message.contains("affected 0 rows")
                    && message.contains("expected exactly 1")),
            "{result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_inflight"));
        assert!(
            !all.contains("INSERT IGNORE") && !all.contains("batch: CREATE TABLE t (id INT)"),
            "an inexact marker write must stop before author DDL: {all}"
        );
    }

    #[compio::test]
    async fn repeatable_completion_keeps_the_repeatable_journal_kind() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let mut m = trivial_migration();
        m.flags.repeatable = true;
        m.down = None;

        backend
            .apply_one(&cfg, &m, "tester", false, &[], "repeatable")
            .await
            .expect("repeatable apply succeeds");

        let binds = rec.binds.borrow();
        assert!(
            binds.iter().any(|params| {
                // The applied insert now carries the stored reverse as a 7th
                // bind (NULL for a migration with none), so the kind is the
                // second-to-last parameter rather than the last.
                params.len() == 7 && params.get(5) == Some(&Bind::Text("repeatable".to_string()))
            }),
            "the completed event must remain visible to latest_completed_checksums: {binds:?}"
        );
    }

    /// An unmatched inflight marker is evidence that auto-committing DDL may
    /// already have landed. Recovery must preserve that evidence and fail closed;
    /// blindly deleting the marker and replaying a bare CREATE/ALTER can wedge the
    /// deployment or apply only part of a multi-statement migration.
    ///
    /// Failing closed is only half the contract: the refusal has to name a repair
    /// the reader can actually run. `recover_inflight_ddl` is Rust-host-only (it
    /// has no binding across the addon seam), so a CLI or Node operator can reach
    /// nothing but the marker `DELETE`. The message must name that statement, must
    /// keep the audited Rust-host route as the alternative, and must not let the
    /// reader believe either route inspects the live schema.
    #[compio::test]
    async fn crashed_ddl_is_not_blindly_replayed() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        let result = backend
            .apply_one(&cfg, &m, "tester", true, &[], "apply")
            .await;

        // The exact statement an operator without a Rust host can copy and run.
        let reachable_delete = format!(
            "DELETE FROM `proj_x_migrations`.schema_migrations_inflight WHERE version = '{}'",
            m.version.as_str()
        );
        assert!(
            matches!(result, Err(ApplyError::Backend(ref message))
                if message.contains("inflight")
                    && message.contains("inspect")
                    // The audited Rust-host alternative stays named.
                    && message.contains("recover_inflight_ddl")
                    && message.contains(m.version.as_str())
                    // ... but it is not the ONLY named route.
                    && message.contains(reachable_delete.as_str())
                    // Recovery audits an operator assertion; it proves nothing.
                    && message.contains("recovery does NOT verify schema shape")),
            "recovery must fail closed with an actionable error: {result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(
            !all.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight")
                && !all.contains("batch: CREATE TABLE t (id INT)"),
            "the marker and possibly-applied DDL must be left untouched: {all}"
        );
    }

    #[compio::test]
    async fn verified_inflight_recovery_marks_applied_without_replaying_ddl() {
        let migration = trivial_migration();
        let rec = RecordingSession::with_inflight(&migration, "deploy-bot");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let outcome = backend
            .recover_inflight_ddl(
                &cfg,
                &migration,
                MysqlInflightResolution::MarkAppliedAfterVerification,
                "operator@example.com",
                "verified every table and index from the reviewed migration",
            )
            .await
            .expect("verified recovery commits");

        assert_eq!(outcome.marker.version, migration.version.as_str());
        assert_eq!(
            outcome.resolution,
            MysqlInflightResolution::MarkAppliedAfterVerification
        );
        let log = rec.log.borrow();
        let audit = log
            .iter()
            .position(|entry| {
                entry.contains("INSERT INTO `proj_x_migrations`.schema_migrations_recovery")
            })
            .expect("immutable audit appended");
        let completed = log
            .iter()
            .position(|entry| entry.contains("'completed', 'success'"))
            .expect("normal completion appended");
        let clear = log
            .iter()
            .position(|entry| {
                entry.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight")
            })
            .expect("marker cleared");
        let commit = log
            .iter()
            .rposition(|entry| entry == "batch: COMMIT")
            .expect("recovery transaction committed");
        assert!(
            audit < completed && completed < clear && clear < commit,
            "{log:?}"
        );
        assert!(
            !log.iter()
                .any(|entry| entry == "batch: CREATE TABLE t (id INT)"),
            "recovery must never replay ambiguous author DDL: {log:?}"
        );
    }

    #[compio::test]
    async fn verified_rollback_recovery_audits_clear_for_normal_retry() {
        let migration = trivial_migration();
        let rec = RecordingSession::with_inflight(&migration, "deploy-bot");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        backend
            .recover_inflight_ddl(
                &cfg,
                &migration,
                MysqlInflightResolution::ClearForRetryAfterRollback,
                "operator@example.com",
                "restored and verified the complete pre-migration schema",
            )
            .await
            .expect("verified rollback recovery commits");

        let all = rec.log.borrow().join("\n");
        assert!(all.contains("schema_migrations_recovery"));
        assert!(all.contains("schema_migrations_inflight WHERE version = ?"));
        assert!(
            !all.contains("'completed', 'success'")
                && !all.contains("batch: CREATE TABLE t (id INT)"),
            "clear-for-retry records no fake completion and runs no author SQL: {all}"
        );
        assert!(rec.binds.borrow().iter().any(|binds| {
            binds
                .iter()
                .any(|bind| matches!(bind, Bind::Text(value) if value == "clear_for_retry"))
        }));
    }

    #[compio::test]
    async fn inflight_recovery_rejects_a_different_reviewed_migration() {
        let original = trivial_migration();
        let rec = RecordingSession::with_inflight(&original, "deploy-bot");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let mut edited = original.clone();
        edited.name = "different_reviewed_name".to_string();

        let result = backend
            .recover_inflight_ddl(
                &cfg,
                &edited,
                MysqlInflightResolution::MarkAppliedAfterVerification,
                "operator@example.com",
                "this must not be accepted",
            )
            .await;

        assert!(
            matches!(
                result,
                Err(MysqlInflightRecoveryError::MarkerMismatch { field: "name", .. })
            ),
            "{result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("batch: ROLLBACK"));
        assert!(
            !all.contains("INSERT INTO `proj_x_migrations`.schema_migrations_recovery")
                && !all.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight"),
            "identity mismatch must preserve all recovery evidence: {all}"
        );
    }

    /// The completed event and inflight cleanup are ordinary InnoDB DML and must
    /// commit together after the auto-committing author DDL has finished.
    #[compio::test]
    async fn completion_and_inflight_cleanup_share_one_transaction() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        backend
            .apply_one(&cfg, &m, "tester", false, &[], "apply")
            .await
            .expect("fresh apply succeeds");

        let log = rec.log.borrow();
        let up = log
            .iter()
            .position(|entry| entry == "batch: CREATE TABLE t (id INT)")
            .expect("DDL ran");
        let begin = log
            .iter()
            .position(|entry| entry == "batch: START TRANSACTION")
            .expect("journal finalization transaction began");
        let completed = log
            .iter()
            .position(|entry| entry.contains("'completed', 'success'"))
            .expect("completed event appended");
        let clear = log
            .iter()
            .position(|entry| {
                entry.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight")
            })
            .expect("inflight marker cleared");
        let commit = log
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .expect("journal finalization committed");
        assert!(
            up < begin && begin < completed && completed < clear && clear < commit,
            "{log:?}"
        );
    }

    /// A fresh squash becomes visible only when its completion event and every
    /// supersession edge commit together. Edge writes are duplicate-safe so the
    /// finalization operation itself is repairable and idempotent.
    #[compio::test]
    async fn squash_completion_and_all_edges_commit_atomically() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        backend
            .apply_one(&cfg, &m, "tester", false, &["v1", "v2"], "squash")
            .await
            .expect("fresh squash succeeds");

        let log = rec.log.borrow();
        let begin = log
            .iter()
            .position(|entry| entry == "batch: START TRANSACTION")
            .expect("squash finalization transaction began");
        let completed = log
            .iter()
            .position(|entry| entry.contains("'completed', 'success'"))
            .expect("squash completion appended");
        let edges = log
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry
                    .contains("INSERT IGNORE INTO `proj_x_migrations`.schema_migrations_supersedes")
            })
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        let clear = log
            .iter()
            .position(|entry| {
                entry.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight")
            })
            .expect("inflight marker cleared");
        let commit = log
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .expect("squash finalization committed");
        assert_eq!(edges.len(), 2, "every edge is inserted once: {log:?}");
        assert!(
            begin < completed
                && edges.iter().all(|edge| completed < *edge && *edge < clear)
                && clear < commit,
            "{log:?}"
        );
    }

    /// A failed edge append rolls the entire journal finalization back, leaving
    /// the inflight marker as recovery evidence and exposing no partial squash.
    #[compio::test]
    async fn squash_edge_failure_rolls_back_completion_and_preserves_marker() {
        let rec = RecordingSession::with_failure(
            "INSERT IGNORE INTO `proj_x_migrations`.schema_migrations_supersedes",
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        let result = backend
            .apply_one(&cfg, &m, "tester", false, &["v1", "v2"], "squash")
            .await;
        assert!(matches!(result, Err(ApplyError::Journal(_))), "{result:?}");

        let log = rec.log.borrow();
        assert!(
            log.iter().any(|entry| entry == "batch: START TRANSACTION")
                && log.iter().any(|entry| entry == "batch: ROLLBACK")
                && !log.iter().any(|entry| entry == "batch: COMMIT")
                && !log.iter().any(|entry| {
                    entry.contains("DELETE FROM `proj_x_migrations`.schema_migrations_inflight")
                }),
            "a partial squash finalization must roll back without clearing recovery evidence: {log:?}"
        );
    }

    #[compio::test]
    async fn mysql_integrity_and_autocommit_settings_are_snapshotted_and_restored() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);

        let snapshot = backend
            .snapshot_session()
            .await
            .expect("session snapshot decodes");
        assert_eq!(snapshot.autocommit, 0);
        assert_eq!(snapshot.information_schema_stats_expiry, 86_400);
        assert_eq!(snapshot.foreign_key_checks, 0);
        assert_eq!(snapshot.unique_checks, 0);

        backend
            .restore_session(&snapshot)
            .await
            .expect("session snapshot restores");

        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("@@SESSION.autocommit AS autocommit")
                && all.contains(
                    "@@SESSION.information_schema_stats_expiry AS information_schema_stats_expiry"
                )
                && all.contains("@@SESSION.foreign_key_checks AS foreign_key_checks")
                && all.contains("@@SESSION.unique_checks AS unique_checks")
                && all.contains("SESSION information_schema_stats_expiry = 86400")
                && all.contains("SESSION autocommit = 0")
                && all.contains("SESSION foreign_key_checks = 0")
                && all.contains("SESSION unique_checks = 0"),
            "all correctness-sensitive settings must round-trip through the snapshot: {all}"
        );
    }

    /// Rollback runs the `down` as a batch then appends a `rolled_back` event with
    /// `?` placeholders and the NULL applied-only columns (only the 5 event fields).
    #[compio::test]
    async fn rollback_runs_down_then_appends_rolled_back_event() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        backend
            .rollback_one_transactional(&cfg, &m, "tester")
            .await
            .expect("rollback runs");

        let log = rec.log.borrow();
        let all = log.join("\n");
        let settings = log
            .iter()
            .position(|s| s.contains("NO_BACKSLASH_ESCAPES"))
            .expect("rollback literal mode is pinned");
        let down = log
            .iter()
            .position(|s| s == "batch: DROP TABLE t")
            .expect("author down runs");
        assert!(
            settings < down,
            "rollback must pin literal mode before author down SQL: {log:?}"
        );
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

    // The sibling test above asserts the `down` ran and the `rolled_back` row has the
    // right shape, and it kept passing when the marker and the transaction bracket
    // were added around it - it reads a narrow slice of a rich log. This one pins the
    // ordering those two things exist for, so removing either fails here.
    //
    // MySQL DDL auto-commits, so the marker must be durable BEFORE the `down`, and the
    // `rolled_back` append must share a transaction with the marker delete. Otherwise
    // a `down` that half-runs, or an append that fails after it, leaves no evidence.
    #[compio::test]
    async fn rollback_arms_a_marker_before_the_down_and_clears_it_with_the_event() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        backend
            .rollback_one_transactional(&cfg, &m, "tester")
            .await
            .expect("rollback runs");

        let log = rec.log.borrow();
        let at = |needle: &str| {
            log.iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle} in {log:?}"))
        };

        let probe = at("FROM `proj_x_migrations`.schema_migrations_rollback_inflight");
        let arm = at("INSERT INTO `proj_x_migrations`.schema_migrations_rollback_inflight");
        let down = at("batch: DROP TABLE t");
        let begin = at("START TRANSACTION");
        let event = at("'rolled_back', ?, ?, ?, ?, ?");
        let clear = at("DELETE FROM `proj_x_migrations`.schema_migrations_rollback_inflight");
        let commit = at("COMMIT");

        assert!(
            probe < arm && arm < down,
            "an unmatched marker is checked, then one is armed, both before the down: {log:?}"
        );
        assert!(
            down < begin && begin < event && begin < clear,
            "the journal bracket opens after the down: {log:?}"
        );
        assert!(
            event < commit && clear < commit,
            "the rolled_back append and the marker clear commit as one unit: {log:?}"
        );
    }

    #[compio::test]
    async fn backfill_progress_reader_decodes_existing_rows_without_bootstrap() {
        let rec = RecordingSession::with_progress(
            vec![Row::new(
                vec!["backfill_id".into(), "checksum".into(), "complete".into()],
                vec![
                    Value::Text("mig_progress".into()),
                    Value::Text("checksum_a".into()),
                    Value::Int(0),
                ],
            )],
            true,
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let progress = backend
            .backfill_progress(&cfg)
            .await
            .expect("progress read runs");

        assert_eq!(
            progress,
            vec![zero_migrate_backend::backfill::BackfillProgressEntry {
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

    /// A structured one-shot DML step runs with its native typed binds, then
    /// writes its completed marker and commits in order. No value is interpolated
    /// into the statement text.
    #[compio::test]
    async fn dml_step_binds_natively_and_journals_atomically() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("authoritative DML artifact");
        let hostile = "x'); DROP TABLE users; --";
        let binds = vec![BindValue::Int(7), BindValue::Text(hostile.into())];

        let ran = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "update users",
                "UPDATE `proj_x`.`users` SET `score` = ? WHERE `name` = ?",
                &binds,
                "proj_x",
                "users",
                None,
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await
            .expect("MySQL DML runs");
        assert!(ran);

        let log = rec.log.borrow();
        let settings = log
            .iter()
            .position(|entry| entry.contains("NO_BACKSLASH_ESCAPES"))
            .expect("data-step literal mode is pinned");
        let begin = log
            .iter()
            .position(|entry| entry == "batch: START TRANSACTION")
            .expect("transaction begins");
        let metadata_lock = log
            .iter()
            .position(|entry| entry.contains("zero_migrate_metadata_lock"))
            .expect("target metadata lock is acquired");
        let engine_check = log
            .iter()
            .position(|entry| entry.contains("information_schema.TABLES"))
            .expect("target engine is checked under the metadata lock");
        let trigger_check = log
            .iter()
            .position(|entry| entry.contains("information_schema.TRIGGERS"))
            .expect("target triggers are checked under the metadata lock");
        let dml = log
            .iter()
            .position(|entry| entry.starts_with("exec: UPDATE `proj_x`.`users`"))
            .expect("DML executes");
        let journal = log
            .iter()
            .position(|entry| {
                entry.starts_with("exec: INSERT INTO `proj_x_migrations`.schema_migrations")
            })
            .expect("journal event executes");
        let commit = log
            .iter()
            .position(|entry| entry == "batch: COMMIT")
            .expect("transaction commits");
        assert!(
            settings < begin
                && begin < metadata_lock
                && metadata_lock < engine_check
                && engine_check < trigger_check
                && trigger_check < dml
                && dml < journal
                && journal < commit,
            "{log:?}"
        );
        assert!(
            !log.iter().any(|entry| entry.contains(hostile)),
            "hostile bind data must never be interpolated into SQL: {log:?}"
        );
        assert!(
            rec.binds.borrow().iter().any(|params| {
                params.as_slice() == [Bind::Int(7), Bind::Text(hostile.to_string())]
            }),
            "the DML values cross as native typed binds: {:?}",
            rec.binds.borrow()
        );
        assert!(
            rec.binds.borrow().iter().any(|params| {
                params.get(2) == Some(&Bind::Text(checksum.as_str().to_string()))
                    && params.first() == Some(&Bind::Text(version.as_str().to_string()))
            }),
            "the supplied artifact checksum is journaled verbatim: {:?}",
            rec.binds.borrow()
        );
    }

    /// MySQL normally interprets an explicit zero for an AUTO_INCREMENT column
    /// as "allocate a value". Import DML must pin NO_AUTO_VALUE_ON_ZERO before
    /// sending the native zero bind, and a failure to establish that invariant
    /// must stop before the row can be written.
    #[compio::test]
    async fn legacy_zero_import_is_preserved_or_rejected_before_dml() {
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("legacy zero import");
        let template = "INSERT INTO `proj_x`.`users` (`id`) VALUES (?)";

        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "import legacy zero user",
                template,
                &[BindValue::Int(0)],
                "proj_x",
                "users",
                None,
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await
            .expect("legacy zero import runs under the pinned SQL mode");

        {
            let log = rec.log.borrow();
            let mode = log
                .iter()
                .position(|entry| entry.contains("NO_AUTO_VALUE_ON_ZERO"))
                .expect("legacy-zero semantics are pinned");
            let insert = log
                .iter()
                .position(|entry| entry == &format!("exec: {template}"))
                .expect("import insert executes");
            assert!(
                mode < insert,
                "SQL mode must be pinned before import DML: {log:?}"
            );
            assert!(
                rec.binds
                    .borrow()
                    .iter()
                    .any(|params| params.as_slice() == [Bind::Int(0)]),
                "the explicit legacy zero must remain a zero bind: {:?}",
                rec.binds.borrow()
            );
        }

        let rejected = RecordingSession::with_failure("NO_AUTO_VALUE_ON_ZERO");
        let rejected_backend = MysqlBackend::new_generic(&rejected);
        let result = rejected_backend
            .run_dml_step(
                &cfg,
                &MigrationId::generate(),
                &step_checksum("rejected legacy zero import"),
                "import legacy zero user",
                template,
                &[BindValue::Int(0)],
                "proj_x",
                "users",
                None,
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;
        assert!(result.is_err(), "mode-pin failure must reject the import");
        assert!(
            !rejected
                .log
                .borrow()
                .iter()
                .any(|entry| entry == &format!("exec: {template}")),
            "the zero row must not reach MySQL when its preserving mode was not established: {:?}",
            rejected.log.borrow()
        );
    }

    #[compio::test]
    async fn mysql_conflict_target_preflight_rejects_nonunique_columns_before_mutation() {
        let rec = RecordingSession::with_unique_indexes(vec![unique_index_part(
            "PRIMARY",
            Some("id"),
            None,
        )]);
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("nonunique onConflict target");
        let target = vec!["group_id".to_string()];

        let result = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "insert user",
                "INSERT INTO `proj_x`.`users` (`id`, `group_id`, `label`) VALUES (?, ?, ?) \
                 AS `zero-migrate-incoming`(`zero-migrate-value-0`, `zero-migrate-value-1`, \
                 `zero-migrate-value-2`) ON DUPLICATE KEY UPDATE `label` = ?",
                &[
                    BindValue::Int(1),
                    BindValue::Int(7),
                    BindValue::Text("incoming".into()),
                    BindValue::Text("must-not-apply".into()),
                ],
                "proj_x",
                "users",
                Some(&target),
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;

        assert!(matches!(result, Err(ApplyError::Backend(ref message))
                if message.contains("group_id") && message.contains("UNIQUE or PRIMARY")));
        let log = rec.log.borrow();
        let start = log
            .iter()
            .position(|entry| entry == "batch: START TRANSACTION")
            .expect("preflight transaction starts");
        let metadata_lock = log
            .iter()
            .position(|entry| entry.contains("zero_migrate_metadata_lock"))
            .expect("target metadata lock is acquired");
        let statistics = log
            .iter()
            .position(|entry| entry.contains("information_schema.STATISTICS"))
            .expect("unique indexes are inspected");
        let rollback = log
            .iter()
            .position(|entry| entry == "batch: ROLLBACK")
            .expect("failed proof rolls back");
        assert!(start < metadata_lock && metadata_lock < statistics && statistics < rollback);
        assert!(
            !log.iter()
                .any(|entry| entry.starts_with("exec: INSERT INTO `proj_x`.`users`")),
            "an unproven target must not execute application DML: {log:?}"
        );
    }

    #[compio::test]
    async fn mutating_dml_rejects_nontransactional_target_before_execution() {
        let rec = RecordingSession::with_table_engine("MyISAM");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("nontransactional target");

        let result = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "insert user",
                "INSERT INTO `proj_x`.`users` (`id`) VALUES (?)",
                &[BindValue::Int(7)],
                "proj_x",
                "users",
                None,
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message)) if message.contains("requires InnoDB"))
        );
        let log = rec.log.borrow();
        assert!(
            log.iter()
                .any(|entry| entry.contains("information_schema.TABLES")),
            "the structurally carried target is checked: {log:?}"
        );
        assert!(
            log.iter().any(|entry| entry == "batch: START TRANSACTION")
                && log
                    .iter()
                    .any(|entry| entry.contains("zero_migrate_metadata_lock"))
                && log.iter().any(|entry| entry == "batch: ROLLBACK")
                && !log
                    .iter()
                    .any(|entry| entry.starts_with("exec: INSERT INTO `proj_x`.`users`")),
            "nontransactional target data must remain untouched: {log:?}"
        );
    }

    #[compio::test]
    async fn mutating_dml_rejects_unprovable_trigger_side_effects() {
        let rec = RecordingSession::with_trigger("users_audit");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("triggered target");

        let result = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "update user",
                "UPDATE `proj_x`.`users` SET `active` = ? WHERE `id` = ?",
                &[BindValue::Bool(true), BindValue::Int(7)],
                "proj_x",
                "users",
                None,
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message)) if message.contains("users_audit") && message.contains("fail closed"))
        );
        let log = rec.log.borrow();
        assert!(
            log.iter().any(|entry| entry == "batch: START TRANSACTION")
                && log
                    .iter()
                    .any(|entry| entry.contains("zero_migrate_metadata_lock"))
                && log.iter().any(|entry| entry == "batch: ROLLBACK")
                && !log
                    .iter()
                    .any(|entry| entry.starts_with("exec: UPDATE `proj_x`.`users`")),
            "a target with unprovable trigger effects must remain untouched: {log:?}"
        );
    }

    /// Pending destructive DML is refused after the idempotency check and before
    /// target inspection or mutation unless approval and version scope admit it.
    #[compio::test]
    async fn destructive_dml_requires_approval_and_version_scope() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("destructive DML artifact");

        let no_approval = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "delete users",
                "DELETE FROM `proj_x`.`users` WHERE `id` = ?",
                &[BindValue::Int(7)],
                "proj_x",
                "users",
                None,
                true,
                true,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;
        assert!(matches!(no_approval, Err(ApplyError::ApprovalRequired)));
        assert!(
            rec.log
                .borrow()
                .iter()
                .any(|entry| entry.contains("WITH ranked AS")),
            "the completed-step check runs before approval"
        );
        assert!(
            !rec.log.borrow().iter().any(|entry| {
                entry == "batch: START TRANSACTION"
                    || entry.contains("DELETE FROM `proj_x`.`users`")
                    || entry.contains("information_schema.TABLES")
            }),
            "refusal must not inspect or mutate the target table"
        );

        let no_scope = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "delete users",
                "DELETE FROM `proj_x`.`users` WHERE `id` = ?",
                &[BindValue::Int(7)],
                "proj_x",
                "users",
                None,
                true,
                true,
                "app_test",
                zero_migrate_backend::approval::Approval::Approved,
                &zero_migrate_backend::approval::ApprovalScope::Versions(Default::default()),
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;
        assert!(matches!(
            no_scope,
            Err(ApplyError::ApprovalNotScoped { .. })
        ));
        assert!(!rec.log.borrow().iter().any(|entry| {
            entry == "batch: START TRANSACTION"
                || entry.contains("DELETE FROM `proj_x`.`users`")
                || entry.contains("information_schema.TABLES")
        }));
    }

    /// Standalone backfills are data-mutating destructive checkpoints. Both
    /// approval and the exact stable step version must be admitted after the
    /// completed-step check and before any progress or target-table work.
    #[compio::test]
    async fn backfill_requires_approval_and_version_scope_before_io() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let version = MigrationId::generate();
        let checksum = step_checksum("backfill artifact");
        let spec = BackfillSpec {
            schema: "proj_x".into(),
            table: "users".into(),
            cursor_columns: vec!["id".into()],
            cursor_stability: zero_migrate_ir::ir::CursorStability::ExternalInvariant {
                name: "users_id_immutable_during_backfill".into(),
            },
            cursor_contract: Some(zero_migrate_backend::backfill::CursorContract {
                columns: vec![zero_migrate_backend::backfill::CursorColumnContract {
                    name: "id".into(),
                    scalar_type: zero_migrate_backend::backfill::CursorScalarType::Int64,
                    database_type: "bigint".into(),
                    comparison: zero_migrate_backend::backfill::CursorComparison::Default,
                }],
            }),
            batch_size: 100,
            set_clause: "`done` = TRUE".into(),
            per_row: std::collections::BTreeMap::new(),
            filter: None,
            name: "finish users".into(),
        };

        let no_approval = backend
            .run_backfill_step(
                &cfg,
                &version,
                &checksum,
                &spec,
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;
        assert!(matches!(no_approval, Err(ApplyError::ApprovalRequired)));
        assert!(
            rec.log
                .borrow()
                .iter()
                .any(|entry| entry.contains("WITH ranked AS")),
            "the completed-step check runs before approval"
        );
        assert!(
            !rec.log
                .borrow()
                .iter()
                .any(|entry| entry.contains("schema_backfills")),
            "refusal must not touch backfill progress"
        );

        let no_scope = backend
            .run_backfill_step(
                &cfg,
                &version,
                &checksum,
                &spec,
                zero_migrate_backend::approval::Approval::Approved,
                &zero_migrate_backend::approval::ApprovalScope::Versions(Default::default()),
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;
        assert!(matches!(
            no_scope,
            Err(ApplyError::ApprovalNotScoped { .. })
        ));
        assert!(
            !rec.log
                .borrow()
                .iter()
                .any(|entry| entry.contains("schema_backfills")),
            "scope refusal must not touch backfill progress"
        );
    }

    /// A completed destructive DML sub-version is a journal-backed no-op on
    /// rerun without renewed approval.
    #[compio::test]
    async fn completed_dml_step_is_skipped_without_reexecution() {
        let version = MigrationId::generate();
        let checksum = step_checksum("delete DML artifact");
        let rec = RecordingSession::with_applied(version.as_str(), &checksum);
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let ran = backend
            .run_dml_step(
                &cfg,
                &version,
                &checksum,
                "delete user",
                "DELETE FROM `proj_x`.`users` WHERE `id` = ?",
                &[BindValue::Int(7)],
                "proj_x",
                "users",
                None,
                true,
                true,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await
            .expect("journal lookup succeeds");
        assert!(!ran, "completed DML must be skipped");
        let log = rec.log.borrow();
        assert!(
            log.iter().any(|entry| entry.contains("WITH ranked AS")),
            "the net journal state is consulted: {log:?}"
        );
        assert!(
            !log.iter().any(|entry| {
                entry.contains("DELETE FROM `proj_x`.`users`")
                    || entry == "batch: START TRANSACTION"
            }),
            "a completed step must not execute again: {log:?}"
        );
    }

    /// Stable step identity is fail-closed: a completed version with a different
    /// authoritative artifact checksum is drift, never an idempotent skip.
    #[compio::test]
    async fn completed_dml_checksum_mismatch_aborts_before_target_io() {
        let version = MigrationId::generate();
        let recorded = step_checksum("old DML artifact");
        let expected = step_checksum("edited DML artifact");
        let rec = RecordingSession::with_applied(version.as_str(), &recorded);
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let result = backend
            .run_dml_step(
                &cfg,
                &version,
                &expected,
                "insert user",
                "INSERT INTO `proj_x`.`users` (`id`) VALUES (?)",
                &[BindValue::Int(7)],
                "proj_x",
                "users",
                None,
                true,
                false,
                "app_test",
                zero_migrate_backend::approval::Approval::None,
                &zero_migrate_backend::approval::ApprovalScope::All,
                "tester",
                zero_migrate_backend::executor::LockMode::AlreadyHeld,
            )
            .await;
        assert!(matches!(
            result,
            Err(ApplyError::ChecksumDrift {
                version: ref drift_version,
                ref recorded,
                ref expected,
            }) if drift_version == version.as_str()
                && recorded == step_checksum("old DML artifact").as_str()
                && expected == step_checksum("edited DML artifact").as_str()
        ));
        let log = rec.log.borrow();
        assert!(log.iter().any(|entry| entry.contains("WITH ranked AS")));
        assert!(
            !log.iter().any(|entry| {
                entry == "batch: START TRANSACTION"
                    || entry.contains("INSERT INTO `proj_x`.`users`")
            }),
            "checksum drift must abort before target mutation: {log:?}"
        );
    }

    /// The remaining capability gaps are fail-closed, not silent: preconditions,
    /// rebuild, baseline, and squash-record-not-run all surface a clear error;
    /// online/shadow/pending-contracts report the capability absent. Catalog
    /// introspection is supported and an actually empty schema is a valid result.
    #[compio::test]
    async fn v1_capability_gaps_fail_closed() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        assert!(backend
            .snapshot_schema(&cfg)
            .await
            .expect("empty catalog snapshot")
            .tables
            .is_empty());
        // A migration with NO preconditions applies normally (empty list => AllMet,
        // no evaluator needed - the executor calls this for every migration).
        assert_eq!(
            backend.evaluate_preconditions(&cfg, &m).await.unwrap(),
            zero_migrate_backend::executor::PreconditionVerdict::AllMet,
            "an empty precondition list is AllMet, not the v1 capability gap",
        );
        // A migration that DECLARES a precondition fails closed (no MySQL evaluator).
        let mut m_pc = trivial_migration();
        m_pc.preconditions = vec![zero_migrate_ir::precondition::PreconditionCheck::halt(
            zero_migrate_ir::precondition::Precondition::TableNotExists { table: "t".into() },
        )];
        assert!(
            backend.evaluate_preconditions(&cfg, &m_pc).await.is_err(),
            "a DECLARED precondition fails closed on MySQL",
        );
        // The PLAN-WIDE seam ABSTAINS from the trait default. MySQL has no
        // evaluator, so it forms no plan-level opinion and the per-migration
        // refusal above stays the only judge - the same error, at the same seam,
        // in the same words as before the plan-wide phase existed. Abstain, NOT
        // `Met`: a backend that cannot answer must never wave a plan through.
        assert_eq!(
            backend
                .evaluate_plan_precondition(
                    &cfg,
                    "v_any",
                    &zero_migrate_ir::precondition::Precondition::ColumnHasNoBlockingDependents {
                        table: "t".into(),
                        column: "c".into(),
                    },
                )
                .await
                .expect("abstaining is not an error"),
            zero_migrate_backend::backend::PlanPreconditionVerdict::Abstain,
            "a backend with no precondition evaluator says nothing at plan level",
        );
        assert!(backend.baseline_one(&cfg, &m, "t").await.is_err());
        assert!(backend.record_squash(&cfg, &m, "t", &["v1"]).await.is_err());
        assert!(backend.online().is_none(), "no online harness on MySQL");
        assert!(
            backend.pending_contracts().is_none(),
            "no cross-deploy pending-contract partition on MySQL"
        );
    }

    /// The checksum-drift gate runs the SAME dialect-agnostic comparison the PG /
    /// SQLite backends use, over the MySQL journal read: an empty journal yields no
    /// drift and no orphans for a fresh set.
    #[compio::test]
    async fn checksum_drift_gate_runs_over_mysql_journal_read() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
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
