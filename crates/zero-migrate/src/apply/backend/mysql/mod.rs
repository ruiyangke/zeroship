//! MySQL [`MigrationBackend`](crate::apply::backend::MigrationBackend)
//! implementation.
//!
//! Generic over the dialect-neutral [`SqlSession`](crate::driver::SqlSession) seam
//! (engine root `crate::driver`) — a host driver (the napi `mysql2` shell) supplies
//! the `SqlSession` impl, exactly as the `pg` shell does for
//! [`PostgresBackend`](crate::apply::backend::PostgresBackend). MySQL rides the
//! SAME seam as Postgres; only the dialect SQL (lock, session, journal DDL,
//! placeholders) differs, and all of it lives here + in [`session`] / [`journal_sql`]
//! — never in the shared executor (the structural fix that lets MySQL ride the
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

pub(crate) mod backfill_sql;
pub(crate) mod drift_sql;
pub(crate) mod identity_sql;
pub(crate) mod journal_sql;
pub(crate) mod primary_key_sql;
pub(crate) mod session;

use super::capability::{BackfillSpec, OnlineSchemaChange, ShadowDryRun};
use super::{CrossDeployObligations, MigrationBackend, PlaceholderStyle};
use crate::apply::baseline::{BaselineError, BaselineOutcome};
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, RollbackError};
use crate::apply::journal::{AppliedEntry, JournalError};
use crate::conn::ExecutorConfig;
use crate::driver::{Row, SqlSession};
use crate::model::migration::{Checksum, Migration, MigrationId};
use crate::model::snapshot::SchemaSnapshot;
use crate::render::plan::{DatabaseFeature, DatabaseRequirements, SqliteRebuildSpec};
use crate::render::step::{AlterPrimaryKeyStep, BindValue, SynchronizeIdentityStep};
use crate::schema::query::SqlDialect;

/// The generic MySQL [`MigrationBackend`] implementation.
///
/// Generic over the [`SqlSession`] driver seam. It carries no online/shadow
/// harness (`online()` / `shadow()` are always `None` — the honest v1 gap), and
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
            && matches!(entry.phase, crate::apply::journal::Phase::Completed)
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
        session::acquire_project_lock(self.conn, &cfg.project_id).await?;
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

fn recovery_db(error: crate::driver::DbError) -> MysqlInflightRecoveryError {
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
                    crate::apply::journal::CompletedRecord {
                        version,
                        name: &migration.name,
                        checksum: migration.checksum.as_str(),
                        applied_by: recovered_by,
                        exec_ms: 0,
                        kind,
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

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Mysql
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
        // "Transactional" is the trait's name for the rollback entry; on MySQL the
        // `down` auto-commits and the `rolled_back` append is ordered after it (the
        // same two-phase reality as apply). The trait method name is kept for a
        // single generic rollback caller.
        session::rollback_one(self.conn, cfg, m, applied_by).await
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
                &[cfg.pg.meta_schema.as_str().into()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        session::ensure_idle_for_journal(self.conn).await?;
        session::acquire_journal_bootstrap_lock(self.conn, &cfg.project_id).await?;
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

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal_sql::applied(self.conn, cfg).await
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
    ) -> Result<Vec<crate::apply::backend::BackfillProgressEntry>, JournalError> {
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
    ) -> Result<crate::apply::drift::ChecksumDriftReport, DriftError> {
        // The drift/tamper comparison is dialect-agnostic
        // (`compare_applied_to_set`); only the journal read underneath is
        // dialect-coupled. Read the net-applied state through the MySQL journal and
        // run the SAME shared comparison the PG/SQLite backends use, so the
        // repeatable-exemption / kind-mismatch / tamper rules never diverge across
        // dialects.
        let applied = journal_sql::applied(self.conn, cfg).await?;
        Ok(crate::apply::drift::compare_applied_to_set(
            &applied, migrations,
        ))
    }

    async fn snapshot_schema(&self, cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        drift_sql::snapshot_schema(self.conn, cfg).await
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

    async fn alter_primary_key(
        &self,
        cfg: &ExecutorConfig,
        step: &AlterPrimaryKeyStep,
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        primary_key_sql::alter_primary_key(self.conn, cfg, step, approval, scope, applied_by).await
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
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<crate::apply::executor::ApplyOutcome, ApplyError> {
        if completed_step_matches(self.applied(cfg).await?, version, checksum)? {
            return Ok(crate::apply::executor::ApplyOutcome {
                applied: Vec::new(),
                skipped: vec![version.as_str().to_string()],
                recovered: Vec::new(),
            });
        }

        // A pending backfill mutates table data. Refuse before progress
        // bootstrap or a target-table read. A completed matching step above is
        // an idempotent skip and does not need renewed approval.
        if approval != crate::approval::Approval::Approved {
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
        Ok(crate::apply::executor::ApplyOutcome {
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
        approval: crate::approval::Approval,
        scope: &crate::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: crate::apply::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        if completed_step_matches(self.applied(cfg).await?, version, checksum)? {
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

/// UNIT / render tests for the MySQL backend.
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
    use crate::apply::drift::diff_snapshots;
    use crate::driver::{Bind, DbError, Row, Value};
    use crate::model::expr::Expr;
    use crate::model::ir::{
        ColType, IdentityCol, IrColumn, IrConstraint, IrConstraintKind, IrDefault, MigrationIr, Op,
        ValueFormat, CURRENT_IR_VERSION,
    };
    use crate::model::migration::{Checksum, MigrationFlags};
    use crate::model::snapshot::IdDefaultSnapshot;
    use serde_json::json;
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
        applied: RefCell<Option<(String, String)>>,
        inflight_marker: RefCell<Option<MysqlInflightDdlMarker>>,
        table_engine: RefCell<String>,
        server_version: String,
        default_storage_engine: String,
        innodb_support: Option<String>,
        global_binlog_format: String,
        session_binlog_format: String,
        trigger_name: RefCell<Option<String>>,
        unique_index_rows: RefCell<Vec<Row>>,
        edge_index_rows: RefCell<Vec<Row>>,
        binary_journal_collations: bool,
        catalog_tables: RefCell<Vec<Row>>,
        catalog_columns: RefCell<Vec<Row>>,
        catalog_checks: RefCell<Vec<Row>>,
        catalog_indexes: RefCell<Vec<Row>>,
        catalog_foreign_keys: RefCell<Vec<Row>>,
        progress: RefCell<Vec<Row>>,
        progress_table_exists: bool,
        progress_checksum_exists: bool,
        session_in_transaction: i64,
        zero_affected_contains: RefCell<Option<String>>,
        fail_once_contains: RefCell<Option<String>>,
    }

    impl RecordingSession {
        fn new() -> Self {
            Self {
                log: RefCell::new(Vec::new()),
                binds: RefCell::new(Vec::new()),
                applied: RefCell::new(None),
                inflight_marker: RefCell::new(None),
                table_engine: RefCell::new("InnoDB".to_string()),
                server_version: "8.0.13".to_string(),
                default_storage_engine: "InnoDB".to_string(),
                innodb_support: Some("DEFAULT".to_string()),
                global_binlog_format: "ROW".to_string(),
                session_binlog_format: "ROW".to_string(),
                trigger_name: RefCell::new(None),
                unique_index_rows: RefCell::new(Vec::new()),
                edge_index_rows: RefCell::new(Vec::new()),
                binary_journal_collations: true,
                catalog_tables: RefCell::new(Vec::new()),
                catalog_columns: RefCell::new(Vec::new()),
                catalog_checks: RefCell::new(Vec::new()),
                catalog_indexes: RefCell::new(Vec::new()),
                catalog_foreign_keys: RefCell::new(Vec::new()),
                progress: RefCell::new(Vec::new()),
                progress_table_exists: false,
                progress_checksum_exists: false,
                session_in_transaction: 0,
                zero_affected_contains: RefCell::new(None),
                fail_once_contains: RefCell::new(None),
            }
        }

        fn with_table_engine(engine: &str) -> Self {
            let session = Self::new();
            *session.table_engine.borrow_mut() = engine.to_string();
            session
        }

        fn with_uuid_capabilities(
            version: &str,
            default_engine: &str,
            innodb_support: Option<&str>,
            global_binlog_format: &str,
            session_binlog_format: &str,
        ) -> Self {
            let mut session = Self::new();
            session.server_version = version.to_string();
            session.default_storage_engine = default_engine.to_string();
            session.innodb_support = innodb_support.map(str::to_string);
            session.global_binlog_format = global_binlog_format.to_string();
            session.session_binlog_format = session_binlog_format.to_string();
            session
        }

        fn with_trigger(name: &str) -> Self {
            let session = Self::new();
            *session.trigger_name.borrow_mut() = Some(name.to_string());
            session
        }

        fn with_unique_indexes(rows: Vec<Row>) -> Self {
            let session = Self::new();
            *session.unique_index_rows.borrow_mut() = rows;
            session
        }

        fn with_edge_index(rows: Vec<Row>) -> Self {
            let session = Self::new();
            *session.edge_index_rows.borrow_mut() = rows;
            session
        }

        fn with_legacy_journal_collations() -> Self {
            let mut session = Self::new();
            session.binary_journal_collations = false;
            session
        }

        fn with_catalog(
            tables: Vec<Row>,
            columns: Vec<Row>,
            indexes: Vec<Row>,
            foreign_keys: Vec<Row>,
        ) -> Self {
            let session = Self::new();
            *session.catalog_tables.borrow_mut() = tables;
            *session.catalog_columns.borrow_mut() = columns;
            *session.catalog_indexes.borrow_mut() = indexes;
            *session.catalog_foreign_keys.borrow_mut() = foreign_keys;
            session
        }

        fn with_catalog_checks(
            tables: Vec<Row>,
            columns: Vec<Row>,
            indexes: Vec<Row>,
            foreign_keys: Vec<Row>,
            checks: Vec<Row>,
        ) -> Self {
            let mut session = Self::with_catalog(tables, columns, indexes, foreign_keys);
            session.server_version = "8.0.16".to_string();
            *session.catalog_checks.borrow_mut() = checks;
            session
        }

        fn with_progress(rows: Vec<Row>, checksum_exists: bool) -> Self {
            let mut session = Self::new();
            *session.progress.borrow_mut() = rows;
            session.progress_table_exists = true;
            session.progress_checksum_exists = checksum_exists;
            session
        }

        fn with_applied(version: &str, checksum: &Checksum) -> Self {
            let session = Self::new();
            *session.applied.borrow_mut() =
                Some((version.to_string(), checksum.as_str().to_string()));
            session
        }

        fn with_inflight(migration: &Migration, applied_by: &str) -> Self {
            let session = Self::new();
            *session.inflight_marker.borrow_mut() = Some(MysqlInflightDdlMarker {
                version: migration.version.as_str().to_string(),
                name: migration.name.clone(),
                checksum: migration.checksum.as_str().to_string(),
                applied_by: applied_by.to_string(),
                started_at: "2026-07-15 12:00:00.000000".to_string(),
            });
            session
        }

        fn with_failure(fragment: &str) -> Self {
            let session = Self::new();
            *session.fail_once_contains.borrow_mut() = Some(fragment.to_string());
            session
        }

        fn with_in_transaction(in_transaction: i64) -> Self {
            let mut session = Self::new();
            session.session_in_transaction = in_transaction;
            session
        }

        fn with_zero_affected(fragment: &str) -> Self {
            let session = Self::new();
            *session.zero_affected_contains.borrow_mut() = Some(fragment.to_string());
            session
        }

        fn fail_if_requested(&self, sql: &str) -> Result<(), DbError> {
            let should_fail = self
                .fail_once_contains
                .borrow()
                .as_deref()
                .is_some_and(|fragment| sql.contains(fragment));
            if should_fail {
                self.fail_once_contains.borrow_mut().take();
                return Err(DbError::message("injected RecordingSession failure"));
            }
            Ok(())
        }

        /// Route a read to its canned rows by SQL shape. `GET_LOCK` returns a
        /// single `got=1` row; everything else (trigger-existence probe, journal
        /// net-state reads) returns empty — enough to drive the whole apply/journal
        /// sweep end-to-end without a live server.
        fn rows_for(&self, sql: &str) -> Vec<Row> {
            if sql.contains("GET_LOCK") {
                vec![Row::new(vec!["got".to_string()], vec![Value::Int(1)])]
            } else if sql.contains("VERSION() AS server_version") {
                vec![Row::new(
                    vec![
                        "server_version".into(),
                        "default_storage_engine".into(),
                        "innodb_support".into(),
                        "global_binlog_format".into(),
                        "session_binlog_format".into(),
                    ],
                    vec![
                        Value::Text(self.server_version.clone()),
                        Value::Text(self.default_storage_engine.clone()),
                        self.innodb_support
                            .as_ref()
                            .map_or(Value::Null, |value| Value::Text(value.clone())),
                        Value::Text(self.global_binlog_format.clone()),
                        Value::Text(self.session_binlog_format.clone()),
                    ],
                )]
            } else if sql.contains("@@SESSION.sql_mode") {
                vec![Row::new(
                    vec![
                        "sql_mode".into(),
                        "time_zone".into(),
                        "max_execution_time".into(),
                        "innodb_lock_wait_timeout".into(),
                        "information_schema_stats_expiry".into(),
                        "autocommit".into(),
                        "foreign_key_checks".into(),
                        "unique_checks".into(),
                        "transaction_tracking_enabled".into(),
                        "in_transaction".into(),
                    ],
                    vec![
                        Value::Text("STRICT_TRANS_TABLES".into()),
                        Value::Text("SYSTEM".into()),
                        Value::Int(0),
                        Value::Int(50),
                        Value::Int(86_400),
                        Value::Int(0),
                        Value::Int(0),
                        Value::Int(0),
                        Value::Int(1),
                        Value::Int(self.session_in_transaction),
                    ],
                )]
            } else if sql.contains("performance_schema.events_transactions_current") {
                vec![Row::new(
                    vec![
                        "transaction_tracking_enabled".into(),
                        "in_transaction".into(),
                    ],
                    vec![Value::Int(1), Value::Int(self.session_in_transaction)],
                )]
            } else if sql.contains("schema_migrations_inflight") && sql.contains("FOR UPDATE") {
                self.inflight_marker
                    .borrow()
                    .as_ref()
                    .map_or_else(Vec::new, |marker| {
                        vec![Row::new(
                            vec![
                                "version".into(),
                                "name".into(),
                                "checksum".into(),
                                "applied_by".into(),
                                "started_at".into(),
                            ],
                            vec![
                                Value::Text(marker.version.clone()),
                                Value::Text(marker.name.clone()),
                                Value::Text(marker.checksum.clone()),
                                Value::Text(marker.applied_by.clone()),
                                Value::Text(marker.started_at.clone()),
                            ],
                        )]
                    })
            } else if sql.contains("WITH ranked AS") {
                self.applied
                    .borrow()
                    .as_ref()
                    .map_or_else(Vec::new, |(version, checksum)| {
                        vec![Row::new(
                            vec![
                                "version".into(),
                                "checksum".into(),
                                "mig_kind".into(),
                                "event_seq".into(),
                                "phase".into(),
                            ],
                            vec![
                                Value::Text(version.clone()),
                                Value::Text(checksum.clone()),
                                Value::Text("apply".into()),
                                Value::Int(1),
                                Value::Text("completed".into()),
                            ],
                        )]
                    })
            } else if sql.contains("AS table_exists") && sql.contains("schema_backfills") {
                vec![Row::new(
                    vec!["table_exists".into(), "checksum_exists".into()],
                    vec![
                        Value::Int(i64::from(self.progress_table_exists)),
                        Value::Int(i64::from(self.progress_checksum_exists)),
                    ],
                )]
            } else if sql.contains("schema_backfills") && sql.contains("AS checksum") {
                self.progress.borrow().clone()
            } else if sql.contains("information_schema.TRIGGERS") {
                self.trigger_name
                    .borrow()
                    .as_ref()
                    .map_or_else(Vec::new, |name| {
                        vec![Row::new(
                            vec!["trigger_name".into()],
                            vec![Value::Text(name.clone())],
                        )]
                    })
            } else if sql.contains("TABLE_TYPE = 'BASE TABLE'") {
                self.catalog_tables.borrow().clone()
            } else if sql.contains("COLUMN_TYPE AS column_type")
                && sql.contains("ORDINAL_POSITION AS ordinal_position")
            {
                self.catalog_columns.borrow().clone()
            } else if sql.contains("information_schema.CHECK_CONSTRAINTS")
                && sql.contains("tc.ENFORCED AS enforced")
            {
                self.catalog_checks.borrow().clone()
            } else if sql.contains("EXPRESSION AS expression")
                && sql.contains("ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX")
            {
                self.catalog_indexes.borrow().clone()
            } else if sql.contains("information_schema.REFERENTIAL_CONSTRAINTS")
                && sql.contains("POSITION_IN_UNIQUE_CONSTRAINT")
            {
                self.catalog_foreign_keys.borrow().clone()
            } else if sql.contains("COLLATION_NAME AS collation_name")
                && sql.contains("schema_migrations_inflight")
            {
                let collation = if self.binary_journal_collations {
                    "utf8mb4_bin"
                } else {
                    "utf8mb4_0900_ai_ci"
                };
                [
                    ("schema_migrations", "version"),
                    ("schema_migrations", "checksum"),
                    ("schema_migrations_supersedes", "squash_version"),
                    ("schema_migrations_supersedes", "superseded_version"),
                    ("schema_migrations_inflight", "version"),
                    ("schema_migrations_inflight", "checksum"),
                    ("schema_migrations_recovery", "version"),
                    ("schema_migrations_recovery", "checksum"),
                ]
                .into_iter()
                .map(|(table, column)| {
                    Row::new(
                        vec![
                            "table_name".into(),
                            "column_name".into(),
                            "character_set_name".into(),
                            "collation_name".into(),
                        ],
                        vec![
                            Value::Text(table.into()),
                            Value::Text(column.into()),
                            Value::Text("utf8mb4".into()),
                            Value::Text(collation.into()),
                        ],
                    )
                })
                .collect()
            } else if sql.contains("information_schema.STATISTICS")
                && sql.contains("schema_migrations_supersedes_edge_uq")
            {
                self.edge_index_rows.borrow().clone()
            } else if sql.contains("information_schema.STATISTICS") {
                self.unique_index_rows.borrow().clone()
            } else if sql.contains("information_schema.TABLES") {
                vec![Row::new(
                    vec!["table_engine".into()],
                    vec![Value::Text(self.table_engine.borrow().clone())],
                )]
            } else {
                Vec::new()
            }
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.log.borrow_mut().push(format!("batch: {sql}"));
            self.fail_if_requested(sql)?;
            Ok(())
        }
        async fn exec(&self, sql: &str, params: &[Bind]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            self.fail_if_requested(sql)?;
            Ok(u64::from(
                !self
                    .zero_affected_contains
                    .borrow()
                    .as_deref()
                    .is_some_and(|fragment| sql.contains(fragment)),
            ))
        }
        async fn exec_text(&self, sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            self.log.borrow_mut().push(format!("exec_text: {sql}"));
            Ok(1)
        }
        async fn query(&self, sql: &str, params: &[Bind]) -> Result<Vec<Row>, DbError> {
            self.log.borrow_mut().push(format!("query: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            self.fail_if_requested(sql)?;
            Ok(self.rows_for(sql))
        }
        async fn query_one(&self, sql: &str, params: &[Bind]) -> Result<Row, DbError> {
            self.log.borrow_mut().push(format!("query_one: {sql}"));
            self.binds.borrow_mut().push(params.to_vec());
            self.fail_if_requested(sql)?;
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

    fn step_checksum(label: &str) -> Checksum {
        Checksum::of(&crate::model::migration::ChecksumInput {
            up: label,
            down: None,
            flags: &MigrationFlags::default(),
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        })
    }

    fn unique_index_part(index: &str, column: Option<&str>, sub_part: Option<i64>) -> Row {
        Row::new(
            vec!["index_name".into(), "column_name".into(), "sub_part".into()],
            vec![
                Value::Text(index.into()),
                column.map_or(Value::Null, |value| Value::Text(value.into())),
                sub_part.map_or(Value::Null, Value::Int),
            ],
        )
    }

    fn edge_index_part(
        non_unique: i64,
        sequence: i64,
        column: Option<&str>,
        sub_part: Option<i64>,
    ) -> Row {
        Row::new(
            vec![
                "non_unique".into(),
                "seq_in_index".into(),
                "column_name".into(),
                "sub_part".into(),
            ],
            vec![
                Value::Int(non_unique),
                Value::Int(sequence),
                column.map_or(Value::Null, |value| Value::Text(value.into())),
                sub_part.map_or(Value::Null, Value::Int),
            ],
        )
    }

    fn catalog_column(
        table: &str,
        column: &str,
        column_type: &str,
        character_set: Option<&str>,
        collation: Option<&str>,
        nullable: bool,
        ordinal: i64,
    ) -> Row {
        catalog_column_with_generation(
            table,
            column,
            column_type,
            character_set,
            collation,
            nullable,
            ordinal,
            None,
            "",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn catalog_column_with_generation(
        table: &str,
        column: &str,
        column_type: &str,
        character_set: Option<&str>,
        collation: Option<&str>,
        nullable: bool,
        ordinal: i64,
        default: Option<&str>,
        extra: &str,
    ) -> Row {
        Row::new(
            vec![
                "table_name".into(),
                "column_name".into(),
                "column_type".into(),
                "character_set_name".into(),
                "collation_name".into(),
                "is_nullable".into(),
                "column_default".into(),
                "extra".into(),
                "ordinal_position".into(),
            ],
            vec![
                Value::Text(table.into()),
                Value::Text(column.into()),
                Value::Text(column_type.into()),
                character_set.map_or(Value::Null, |value| Value::Text(value.into())),
                collation.map_or(Value::Null, |value| Value::Text(value.into())),
                Value::Text(if nullable { "YES" } else { "NO" }.into()),
                default.map_or(Value::Null, |value| Value::Text(value.into())),
                Value::Text(extra.into()),
                Value::Int(ordinal),
            ],
        )
    }

    fn catalog_check(table: &str, constraint: &str, enforced: bool, check_clause: &str) -> Row {
        Row::new(
            vec![
                "table_name".into(),
                "constraint_name".into(),
                "enforced".into(),
                "check_clause".into(),
            ],
            vec![
                Value::Text(table.into()),
                Value::Text(constraint.into()),
                Value::Text(if enforced { "YES" } else { "NO" }.into()),
                Value::Text(check_clause.into()),
            ],
        )
    }

    const MYSQL_CATALOG_UUID_V4_DEFAULT: &str = "lower(concat(hex(random_bytes(4)),_latin1'-',hex(random_bytes(2)),_latin1'-',hex(((ord(random_bytes(1)) & 15) | 64)),hex(random_bytes(1)),_latin1'-',hex(((ord(random_bytes(1)) & 63) | 128)),hex(random_bytes(1)),_latin1'-',hex(random_bytes(6))))";

    fn id_catalog_columns(
        generated_uuid_default: Option<&str>,
        supplied_uuid_default: Option<&str>,
        type_id_default: Option<&str>,
        ulid_default: Option<&str>,
    ) -> Vec<Row> {
        id_catalog_columns_with_generated_uuid_extra(
            generated_uuid_default,
            supplied_uuid_default,
            type_id_default,
            ulid_default,
            "DEFAULT_GENERATED",
        )
    }

    fn id_catalog_columns_with_generated_uuid_extra(
        generated_uuid_default: Option<&str>,
        supplied_uuid_default: Option<&str>,
        type_id_default: Option<&str>,
        ulid_default: Option<&str>,
        generated_uuid_extra: &str,
    ) -> Vec<Row> {
        vec![
            catalog_column_with_generation(
                "ids",
                "auto_id",
                "bigint",
                None,
                None,
                false,
                1,
                None,
                "auto_increment",
            ),
            catalog_column_with_generation(
                "ids",
                "generated_uuid",
                "varchar(36)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                2,
                generated_uuid_default,
                generated_uuid_extra,
            ),
            catalog_column_with_generation(
                "ids",
                "supplied_uuid",
                "varchar(36)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                3,
                supplied_uuid_default,
                supplied_uuid_default.map_or("", |_| "DEFAULT_GENERATED"),
            ),
            catalog_column_with_generation(
                "ids",
                "type_id",
                "varchar(191)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                4,
                type_id_default,
                type_id_default.map_or("", |_| "DEFAULT_GENERATED"),
            ),
            catalog_column_with_generation(
                "ids",
                "ulid",
                "varchar(191)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                5,
                ulid_default,
                ulid_default.map_or("", |_| "DEFAULT_GENERATED"),
            ),
            // An ordinary text default that happens to call uuid() must remain
            // outside the narrow ID-default comparison surface without an
            // engine-owned UUID format CHECK.
            catalog_column_with_generation(
                "ids",
                "ordinary",
                "varchar(191)",
                Some("utf8mb4"),
                Some("utf8mb4_bin"),
                false,
                6,
                Some("uuid()"),
                "DEFAULT_GENERATED",
            ),
        ]
    }

    fn catalog_index_part(
        table: &str,
        index: &str,
        non_unique: i64,
        sequence: i64,
        column: Option<&str>,
        prefix: Option<i64>,
        collation: Option<&str>,
        expression: Option<&str>,
    ) -> Row {
        Row::new(
            vec![
                "table_name".into(),
                "index_name".into(),
                "non_unique".into(),
                "seq_in_index".into(),
                "column_name".into(),
                "sub_part".into(),
                "index_collation".into(),
                "index_type".into(),
                "expression".into(),
            ],
            vec![
                Value::Text(table.into()),
                Value::Text(index.into()),
                Value::Int(non_unique),
                Value::Int(sequence),
                column.map_or(Value::Null, |value| Value::Text(value.into())),
                prefix.map_or(Value::Null, Value::Int),
                collation.map_or(Value::Null, |value| Value::Text(value.into())),
                Value::Text("BTREE".into()),
                expression.map_or(Value::Null, |value| Value::Text(value.into())),
            ],
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn catalog_foreign_key_part(
        table: &str,
        constraint: &str,
        ordinal: i64,
        unique_position: i64,
        column: &str,
        referenced_schema: &str,
        referenced_table: &str,
        referenced_column: &str,
        update_rule: &str,
        delete_rule: &str,
    ) -> Row {
        Row::new(
            vec![
                "table_name".into(),
                "constraint_name".into(),
                "ordinal_position".into(),
                "position_in_unique_constraint".into(),
                "column_name".into(),
                "referenced_table_schema".into(),
                "referenced_table_name".into(),
                "referenced_column_name".into(),
                "update_rule".into(),
                "delete_rule".into(),
            ],
            vec![
                Value::Text(table.into()),
                Value::Text(constraint.into()),
                Value::Int(ordinal),
                Value::Int(unique_position),
                Value::Text(column.into()),
                Value::Text(referenced_schema.into()),
                Value::Text(referenced_table.into()),
                Value::Text(referenced_column.into()),
                Value::Text(update_rule.into()),
                Value::Text(delete_rule.into()),
            ],
        )
    }

    fn plan_dml_step(label: &str, destructive: bool) -> (crate::PlanStep, MigrationId) {
        let version = MigrationId::generate();
        let template = if destructive {
            "DELETE FROM `proj_x`.`users` WHERE `id` = ?"
        } else {
            "INSERT INTO `proj_x`.`users` (`id`) VALUES (?)"
        };
        (
            crate::PlanStep::Dml {
                version: version.clone(),
                checksum: step_checksum(label),
                name: label.into(),
                template: template.into(),
                binds: vec![BindValue::Int(7)],
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
        )
    }

    fn requirements(feature: DatabaseFeature) -> DatabaseRequirements {
        let mut requirements = DatabaseRequirements::default();
        requirements.require(feature);
        requirements
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
    async fn uuid_v4_capability_failure_precedes_authored_sql() {
        let rec = RecordingSession::with_uuid_capabilities(
            "8.0.12",
            "InnoDB",
            Some("DEFAULT"),
            "ROW",
            "ROW",
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let migration = trivial_migration();
        let authored_sql = migration.up.clone();
        let mut plan = crate::AppliedPlan::single_step(migration);
        plan.database_requirements
            .require(DatabaseFeature::UuidV4Generation);

        let error = crate::MigrationEngine::new()
            .apply_applied_plan_with_touched_and_depends(
                &plan,
                &[],
                &[],
                crate::approval::Approval::None,
                &backend,
                &cfg,
                "tester",
                crate::apply::executor::LockMode::Acquire,
            )
            .await
            .expect_err("the unsupported server must stop the complete plan");
        assert!(error.to_string().contains("MySQL 8.0.13"), "got: {error}");
        let log = rec.log.borrow();
        assert!(
            log.iter()
                .any(|entry| entry.contains("VERSION() AS server_version")),
            "the capability probe must run: {log:?}"
        );
        assert!(
            !log.iter().any(|entry| entry.contains(&authored_sql)),
            "authored SQL must not run after capability failure: {log:?}"
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
    async fn snapshot_schema_reads_canonical_columns_and_ordered_unique_indexes() {
        let rec = RecordingSession::with_catalog(
            vec![Row::new(
                vec!["table_name".into()],
                vec![Value::Text("users".into())],
            )],
            vec![
                catalog_column("users", "id", "bigint(20)", None, None, false, 1),
                catalog_column("users", "tenant_id", "bigint", None, None, false, 2),
                catalog_column(
                    "users",
                    "email",
                    "varchar(191)",
                    Some("utf8mb4"),
                    Some("utf8mb4_0900_ai_ci"),
                    false,
                    3,
                ),
                catalog_column(
                    "users",
                    "nickname",
                    "varchar(40)",
                    Some("utf8mb4"),
                    Some("utf8mb4_bin"),
                    true,
                    4,
                ),
            ],
            vec![
                catalog_index_part("users", "PRIMARY", 0, 1, Some("id"), None, Some("A"), None),
                catalog_index_part(
                    "users",
                    "idx_nickname_prefix",
                    1,
                    1,
                    Some("nickname"),
                    Some(8),
                    Some("A"),
                    None,
                ),
                catalog_index_part(
                    "users",
                    "uq_users_tenant_email",
                    0,
                    1,
                    Some("tenant_id"),
                    None,
                    Some("A"),
                    None,
                ),
                catalog_index_part(
                    "users",
                    "uq_users_tenant_email",
                    0,
                    2,
                    Some("email"),
                    None,
                    Some("D"),
                    None,
                ),
            ],
            vec![
                catalog_foreign_key_part(
                    "users",
                    "users_id_fkey",
                    1,
                    1,
                    "id",
                    "proj_x",
                    "users",
                    "id",
                    "RESTRICT",
                    "CASCADE",
                ),
                catalog_foreign_key_part(
                    "users",
                    "users_tenant_email_fkey",
                    1,
                    1,
                    "tenant_id",
                    "proj_x",
                    "users",
                    "tenant_id",
                    "CASCADE",
                    "SET NULL",
                ),
                catalog_foreign_key_part(
                    "users",
                    "users_tenant_email_fkey",
                    2,
                    2,
                    "email",
                    "proj_x",
                    "users",
                    "email",
                    "CASCADE",
                    "SET NULL",
                ),
            ],
        );
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        let snapshot = backend
            .snapshot_schema(&cfg)
            .await
            .expect("MySQL catalog snapshot");

        let users = snapshot.tables.get("users").expect("users table");
        assert_eq!(
            users
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.data_type.as_str(),
                    column.nullable,
                    column.case_sensitive,
                    column.mysql_text_storage.as_ref().map(|storage| (
                        storage.character_set.as_str(),
                        storage.collation.as_str(),
                    )),
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    "email",
                    "text",
                    false,
                    Some(false),
                    Some(("utf8mb4", "utf8mb4_0900_ai_ci")),
                ),
                ("id", "bigint", false, None, None),
                (
                    "nickname",
                    "text",
                    true,
                    None,
                    Some(("utf8mb4", "utf8mb4_bin")),
                ),
                ("tenant_id", "bigint", false, None, None),
            ]
        );
        assert_eq!(
            users
                .indexes
                .iter()
                .map(|index| index.name.as_str())
                .collect::<Vec<_>>(),
            vec!["idx_nickname_prefix", "uq_users_tenant_email", "users_pkey"]
        );
        assert!(users.indexes[0].columns.is_empty());
        assert!(users.indexes[1].unique);
        assert_eq!(users.indexes[1].columns, ["tenant_id", "email"]);
        assert!(users.indexes[2].unique);
        assert_eq!(users.indexes[2].columns, ["id"]);
        assert_eq!(users.constraints.len(), 3);
        let primary = users
            .constraints
            .iter()
            .find(|constraint| constraint.name == "users_pkey")
            .expect("primary key");
        assert_eq!(primary.kind, "PRIMARY KEY");
        assert_eq!(primary.definition, "PRIMARY KEY (id)");
        let single_fk = users
            .constraints
            .iter()
            .find(|constraint| constraint.name == "users_id_fkey")
            .expect("single-column FK");
        assert_eq!(single_fk.kind, "FOREIGN KEY");
        assert_eq!(
            single_fk.definition,
            "FOREIGN KEY (id) REFERENCES proj_x.users(id) ON DELETE CASCADE"
        );
        let composite_fk = users
            .constraints
            .iter()
            .find(|constraint| constraint.name == "users_tenant_email_fkey")
            .expect("composite FK");
        assert_eq!(composite_fk.kind, "FOREIGN KEY");
        assert_eq!(
            composite_fk.definition,
            "FOREIGN KEY (tenant_id, email) REFERENCES proj_x.users(tenant_id, email) ON UPDATE CASCADE ON DELETE SET NULL"
        );
        assert!(
            diff_snapshots(&snapshot, &snapshot).is_clean(),
            "the exact single/composite FK catalog must stay clean"
        );
        let assert_fk_drop = |constraint_name: &str, scenario: &str| {
            let mut changed = snapshot.clone();
            changed
                .tables
                .get_mut("users")
                .expect("users table")
                .constraints
                .retain(|constraint| constraint.name != constraint_name);
            let drift = diff_snapshots(&snapshot, &changed);
            assert!(
                drift
                    .missing_objects
                    .iter()
                    .any(|missing| missing.contains(constraint_name)),
                "{scenario} must drift: {drift:?}"
            );
        };
        let assert_fk_definition_drift =
            |constraint_name: &str, definition: &str, scenario: &str| {
                let mut changed = snapshot.clone();
                changed
                    .tables
                    .get_mut("users")
                    .expect("users table")
                    .constraints
                    .iter_mut()
                    .find(|constraint| constraint.name == constraint_name)
                    .expect("foreign key")
                    .definition = definition.to_string();
                let drift = diff_snapshots(&snapshot, &changed);
                assert!(
                    drift.altered_objects.iter().any(|altered| {
                        altered.object == format!("constraint {constraint_name}")
                            && altered.field == "definition"
                    }),
                    "{scenario} must drift: {drift:?}"
                );
            };

        assert_fk_drop("users_id_fkey", "a dropped single-column FK");
        assert_fk_definition_drift(
            "users_id_fkey",
            "FOREIGN KEY (id) REFERENCES proj_x.accounts(id) ON DELETE CASCADE",
            "a repointed single-column FK",
        );
        assert_fk_definition_drift(
            "users_id_fkey",
            "FOREIGN KEY (id) REFERENCES proj_x.users(id) ON UPDATE CASCADE ON DELETE SET NULL",
            "a single-column FK action change",
        );

        assert_fk_drop("users_tenant_email_fkey", "a dropped composite foreign key");
        assert_fk_definition_drift(
            "users_tenant_email_fkey",
            "FOREIGN KEY (tenant_id, email) REFERENCES proj_x.accounts(tenant_id, email) ON UPDATE CASCADE ON DELETE SET NULL",
            "a repointed composite foreign key",
        );
        assert_fk_definition_drift(
            "users_tenant_email_fkey",
            "FOREIGN KEY (email, tenant_id) REFERENCES proj_x.users(email, tenant_id) ON UPDATE CASCADE ON DELETE SET NULL",
            "a reordered composite foreign key",
        );
        assert_fk_definition_drift(
            "users_tenant_email_fkey",
            "FOREIGN KEY (tenant_id, email) REFERENCES proj_x.users(tenant_id, email) ON UPDATE NO ACTION ON DELETE CASCADE",
            "a composite foreign-key action change",
        );
        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("TABLE_TYPE = 'BASE TABLE'")
                && all.contains("COLUMN_TYPE AS column_type")
                && all.contains("CHARACTER_SET_NAME AS character_set_name")
                && all.contains("COLLATION_NAME AS collation_name")
                && all.contains("ORDER BY TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX")
                && all.contains("information_schema.REFERENTIAL_CONSTRAINTS")
                && all.contains("POSITION_IN_UNIQUE_CONSTRAINT")
                && all
                    .contains("ORDER BY kcu.TABLE_NAME, kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION")
                && !all.contains("information_schema.CHECK_CONSTRAINTS")
                && !all.contains("INDEX_NAME <> 'PRIMARY'"),
            "snapshot must use schema-scoped authoritative catalog reads and gate CHECK_CONSTRAINTS below MySQL 8.0.16: {all}"
        );
        assert_eq!(
            rec.binds
                .borrow()
                .iter()
                .filter(|binds| binds.as_slice() == [Bind::Text("proj_x".to_string())])
                .count(),
            4,
            "every catalog query scopes itself with the project database bind"
        );
    }

    #[compio::test]
    async fn mysql_single_and_composite_fk_mutations_drift_through_catalog_rows() {
        let single = |target: &str, update: &str, delete: &str| {
            catalog_foreign_key_part(
                "child",
                "child_single_fkey",
                1,
                1,
                "single_parent_id",
                "proj_x",
                target,
                "id",
                update,
                delete,
            )
        };
        let composite = |target: &str, reversed: bool, update: &str, delete: &str| -> Vec<Row> {
            let (first_local, first_target, second_local, second_target) = if reversed {
                ("parent_right", "right_id", "parent_left", "left_id")
            } else {
                ("parent_left", "left_id", "parent_right", "right_id")
            };
            vec![
                catalog_foreign_key_part(
                    "child",
                    "child_composite_fkey",
                    1,
                    1,
                    first_local,
                    "proj_x",
                    target,
                    first_target,
                    update,
                    delete,
                ),
                catalog_foreign_key_part(
                    "child",
                    "child_composite_fkey",
                    2,
                    2,
                    second_local,
                    "proj_x",
                    target,
                    second_target,
                    update,
                    delete,
                ),
            ]
        };
        let baseline_rows = || {
            let mut rows = vec![single("parent", "RESTRICT", "CASCADE")];
            rows.extend(composite("parent", false, "CASCADE", "SET NULL"));
            rows
        };
        let snapshot = |foreign_keys: Vec<Row>| async move {
            let session = RecordingSession::with_catalog(
                ["alternate_parent", "child", "parent"]
                    .into_iter()
                    .map(|table| {
                        Row::new(
                            vec!["table_name".into()],
                            vec![Value::Text(table.to_string())],
                        )
                    })
                    .collect(),
                vec![
                    catalog_column("alternate_parent", "id", "bigint", None, None, false, 1),
                    catalog_column(
                        "alternate_parent",
                        "left_id",
                        "bigint",
                        None,
                        None,
                        false,
                        2,
                    ),
                    catalog_column(
                        "alternate_parent",
                        "right_id",
                        "bigint",
                        None,
                        None,
                        false,
                        3,
                    ),
                    catalog_column("child", "single_parent_id", "bigint", None, None, true, 1),
                    catalog_column("child", "parent_left", "bigint", None, None, true, 2),
                    catalog_column("child", "parent_right", "bigint", None, None, true, 3),
                    catalog_column("parent", "id", "bigint", None, None, false, 1),
                    catalog_column("parent", "left_id", "bigint", None, None, false, 2),
                    catalog_column("parent", "right_id", "bigint", None, None, false, 3),
                ],
                Vec::new(),
                foreign_keys,
            );
            MysqlBackend::new_generic(&session)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .expect("MySQL FK catalog snapshot")
        };

        let expected = snapshot(baseline_rows()).await;
        let clean = snapshot(baseline_rows()).await;
        assert!(
            diff_snapshots(&expected, &clean).is_clean(),
            "unchanged catalog FK rows must stay clean"
        );
        let assert_missing = |actual: &SchemaSnapshot, constraint: &str, label: &str| {
            let drift = diff_snapshots(&expected, actual);
            assert!(
                drift
                    .missing_objects
                    .iter()
                    .any(|missing| missing.contains(constraint)),
                "{label} must be missing drift after catalog introspection: {drift:#?}"
            );
        };
        let assert_altered = |actual: &SchemaSnapshot, constraint: &str, label: &str| {
            let drift = diff_snapshots(&expected, actual);
            assert!(
                drift.altered_objects.iter().any(|altered| {
                    altered.object == format!("constraint {constraint}")
                        && altered.field == "definition"
                }),
                "{label} must be definition drift after catalog introspection: {drift:#?}"
            );
        };

        let dropped_single = snapshot(composite("parent", false, "CASCADE", "SET NULL")).await;
        assert_missing(
            &dropped_single,
            "child_single_fkey",
            "dropped single-column FK",
        );
        let mut repointed_single_rows = vec![single("alternate_parent", "RESTRICT", "CASCADE")];
        repointed_single_rows.extend(composite("parent", false, "CASCADE", "SET NULL"));
        assert_altered(
            &snapshot(repointed_single_rows).await,
            "child_single_fkey",
            "repointed single-column FK",
        );
        let mut changed_single_action = vec![single("parent", "CASCADE", "SET NULL")];
        changed_single_action.extend(composite("parent", false, "CASCADE", "SET NULL"));
        assert_altered(
            &snapshot(changed_single_action).await,
            "child_single_fkey",
            "single-column FK action change",
        );

        let dropped_composite = snapshot(vec![single("parent", "RESTRICT", "CASCADE")]).await;
        assert_missing(
            &dropped_composite,
            "child_composite_fkey",
            "dropped composite FK",
        );
        for (label, rows) in [
            (
                "repointed composite FK",
                composite("alternate_parent", false, "CASCADE", "SET NULL"),
            ),
            (
                "reordered composite FK",
                composite("parent", true, "CASCADE", "SET NULL"),
            ),
            (
                "composite FK action change",
                composite("parent", false, "RESTRICT", "CASCADE"),
            ),
        ] {
            let mut variant = vec![single("parent", "RESTRICT", "CASCADE")];
            variant.extend(rows);
            assert_altered(&snapshot(variant).await, "child_composite_fkey", label);
        }
    }

    #[compio::test]
    async fn snapshot_schema_recovers_mysql_identity_id_defaults_and_format_checks() {
        let uuid_generated_check =
            crate::render::value_format::uuid_column_metadata("generated_uuid", SqlDialect::Mysql)
                .expect("UUID metadata")
                .expect("MySQL UUID CHECK")
                .inline_check;
        let uuid_supplied_check =
            crate::render::value_format::uuid_column_metadata("supplied_uuid", SqlDialect::Mysql)
                .expect("UUID metadata")
                .expect("MySQL UUID CHECK")
                .inline_check;
        let type_id_check = crate::render::value_format::column_metadata(
            "type_id",
            &ValueFormat::TypeId {
                prefix: "user".to_string(),
            },
            SqlDialect::Mysql,
        )
        .expect("TypeID metadata")
        .inline_check;
        let ulid_check = crate::render::value_format::column_metadata(
            "ulid",
            &ValueFormat::Ulid,
            SqlDialect::Mysql,
        )
        .expect("ULID metadata")
        .inline_check;
        let table_rows = || {
            vec![Row::new(
                vec!["table_name".into()],
                vec![Value::Text("ids".into())],
            )]
        };
        let primary_index = || {
            vec![catalog_index_part(
                "ids",
                "PRIMARY",
                0,
                1,
                Some("auto_id"),
                None,
                Some("A"),
                None,
            )]
        };
        let checks = || {
            vec![
                catalog_check("ids", "ids_chk_1", true, &uuid_generated_check),
                catalog_check("ids", "ids_chk_2", true, &uuid_supplied_check),
                catalog_check("ids", "ids_chk_3", true, &type_id_check),
                catalog_check("ids", "ids_chk_4", true, &ulid_check),
            ]
        };
        let id_column = |name: &str, ty: ColType| IrColumn {
            name: name.to_string(),
            ty,
            nullable: Some(false),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity: None,
        };
        let mut auto_id = id_column("auto_id", ColType::BigInt);
        auto_id.identity = Some(IdentityCol { always: false });
        let mut generated_uuid = id_column("generated_uuid", ColType::Uuid);
        generated_uuid.default = Some(IrDefault::Expr { expr: Expr::UuidV4 });
        let supplied_uuid = id_column("supplied_uuid", ColType::Uuid);
        let mut type_id = id_column("type_id", ColType::Text);
        type_id.value_format = Some(ValueFormat::TypeId {
            prefix: "user".to_string(),
        });
        let mut ulid = id_column("ulid", ColType::Text);
        ulid.value_format = Some(ValueFormat::Ulid);
        let ordinary = id_column("ordinary", ColType::Text);
        let expected = crate::render::fold::fold_ops(
            &[Op::CreateTable {
                name: "ids".to_string(),
                columns: vec![
                    auto_id,
                    generated_uuid,
                    supplied_uuid,
                    type_id,
                    ulid,
                    ordinary,
                ],
                primary_key: Some(vec!["auto_id".to_string()]),
                constraints: Vec::new(),
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            }],
            SqlDialect::Mysql,
            "proj_x",
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("portable ID table fold");

        let rec = RecordingSession::with_catalog_checks(
            table_rows(),
            id_catalog_columns(Some(MYSQL_CATALOG_UUID_V4_DEFAULT), None, None, None),
            primary_index(),
            Vec::new(),
            checks(),
        );
        let snapshot = MysqlBackend::new_generic(&rec)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("ID-aware MySQL catalog snapshot");
        let ids = &snapshot.tables["ids"];
        let column = |name: &str| {
            ids.columns
                .iter()
                .find(|column| column.name == name)
                .expect("catalog column")
        };
        assert_eq!(
            column("auto_id").identity,
            Some(IdentityCol { always: false })
        );
        assert_eq!(
            column("auto_id").id_default,
            Some(IdDefaultSnapshot::Absent)
        );
        assert_eq!(
            column("generated_uuid").id_default,
            Some(IdDefaultSnapshot::UuidV4),
            "real MySQL catalog normalization must retain UUIDv4 semantics"
        );
        assert_eq!(
            column("supplied_uuid").id_default,
            Some(IdDefaultSnapshot::Absent)
        );
        assert_eq!(
            column("type_id").value_format,
            Some(ValueFormat::TypeId {
                prefix: "user".to_string()
            })
        );
        assert_eq!(
            column("type_id").id_default,
            Some(IdDefaultSnapshot::Absent)
        );
        assert_eq!(column("ulid").value_format, Some(ValueFormat::Ulid));
        assert_eq!(column("ulid").id_default, Some(IdDefaultSnapshot::Absent));
        assert_eq!(column("ordinary").id_default, None);
        let clean_drift = diff_snapshots(&expected, &snapshot);
        assert!(
            clean_drift.is_clean(),
            "the portable auto-increment and exact format/default catalog shape must stay clean: {clean_drift:?}"
        );
        assert!(
            rec.log
                .borrow()
                .iter()
                .any(|sql| sql.contains("information_schema.CHECK_CONSTRAINTS")
                    && sql.contains("tc.ENFORCED AS enforced")),
            "MySQL 8.0.16+ must introspect enforced CHECK clauses"
        );

        let account_type_id_check = crate::render::value_format::column_metadata(
            "type_id",
            &ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            SqlDialect::Mysql,
        )
        .expect("altered TypeID metadata")
        .inline_check;
        let altered_formats = RecordingSession::with_catalog_checks(
            table_rows(),
            id_catalog_columns(Some(MYSQL_CATALOG_UUID_V4_DEFAULT), None, None, None),
            primary_index(),
            Vec::new(),
            vec![
                catalog_check("ids", "ids_chk_1", true, &uuid_generated_check),
                catalog_check("ids", "ids_chk_2", true, &uuid_supplied_check),
                catalog_check("ids", "ids_chk_3", true, &account_type_id_check),
                // The ULID CHECK was dropped out of band. A disabled CHECK is
                // likewise not an enforced format contract.
                catalog_check("ids", "ids_chk_4", false, &ulid_check),
            ],
        );
        let altered_formats = MysqlBackend::new_generic(&altered_formats)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("altered format snapshot");
        let format_drift = diff_snapshots(&snapshot, &altered_formats);
        assert!(
            format_drift.altered_objects.iter().any(|altered| {
                altered.object == "column type_id"
                    && altered.field == "format"
                    && altered.actual == "typeId(account)"
            }),
            "a TypeID prefix mismatch must drift: {format_drift:?}"
        );
        assert!(
            format_drift
                .altered_objects
                .iter()
                .any(|altered| { altered.object == "column ulid" && altered.field == "format" }),
            "a dropped or unenforced ULID CHECK must drift: {format_drift:?}"
        );

        let altered_defaults = RecordingSession::with_catalog_checks(
            table_rows(),
            id_catalog_columns(
                None,
                Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
                Some("make_typeid()"),
                Some("make_ulid()"),
            ),
            primary_index(),
            Vec::new(),
            checks(),
        );
        let altered_defaults = MysqlBackend::new_generic(&altered_defaults)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("altered default snapshot");
        let default_drift = diff_snapshots(&snapshot, &altered_defaults);
        for name in ["generated_uuid", "supplied_uuid", "type_id", "ulid"] {
            assert!(
                default_drift.altered_objects.iter().any(|altered| {
                    altered.object == format!("column {name}") && altered.field == "default"
                }),
                "an ID-default add/remove/swap on {name} must drift: {default_drift:?}"
            );
        }
        let swapped_default = RecordingSession::with_catalog_checks(
            table_rows(),
            id_catalog_columns(Some("uuid()"), None, None, None),
            primary_index(),
            Vec::new(),
            checks(),
        );
        let swapped_default = MysqlBackend::new_generic(&swapped_default)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("swapped default snapshot");
        let swapped_default_drift = diff_snapshots(&snapshot, &swapped_default);
        assert!(
            swapped_default_drift.altered_objects.iter().any(|altered| {
                altered.object == "column generated_uuid"
                    && altered.field == "default"
                    && altered.expected == "uuidV4"
                    && altered.actual
                        == crate::render::value_format::catalog_expression_fingerprint("uuid()")
            }),
            "swapping the exact UUIDv4 generator for MySQL UUIDv1 must drift: {swapped_default_drift:?}"
        );

        let literal_generator_without_check = RecordingSession::with_catalog_checks(
            table_rows(),
            id_catalog_columns_with_generated_uuid_extra(
                Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
                None,
                None,
                None,
                "",
            ),
            primary_index(),
            Vec::new(),
            vec![
                catalog_check("ids", "ids_chk_2", true, &uuid_supplied_check),
                catalog_check("ids", "ids_chk_3", true, &type_id_check),
                catalog_check("ids", "ids_chk_4", true, &ulid_check),
            ],
        );
        let literal_generator_without_check =
            MysqlBackend::new_generic(&literal_generator_without_check)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .expect("same-text literal UUID default without format CHECK");
        let marker_drift = diff_snapshots(&snapshot, &literal_generator_without_check);
        assert!(
            marker_drift.altered_objects.iter().any(|altered| {
                altered.object == "column generated_uuid"
                    && altered.field == "default"
                    && altered.expected == "uuidV4"
                    && altered.actual.contains(MYSQL_CATALOG_UUID_V4_DEFAULT)
            }),
            "dropping the UUID CHECK and changing its same-text generator into a literal must drift: {marker_drift:#?}"
        );

        let literal_uuid_snapshot = |value: &'static str| async move {
            let session = RecordingSession::with_catalog_checks(
                table_rows(),
                id_catalog_columns_with_generated_uuid_extra(Some(value), None, None, None, ""),
                primary_index(),
                Vec::new(),
                checks(),
            );
            MysqlBackend::new_generic(&session)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .expect("literal UUID catalog snapshot")
        };
        let lowercase_literal = literal_uuid_snapshot("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").await;
        let uppercase_literal = literal_uuid_snapshot("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA").await;
        let case_drift = diff_snapshots(&lowercase_literal, &uppercase_literal);
        assert!(
            case_drift.altered_objects.iter().any(|altered| {
                altered.object == "column generated_uuid" && altered.field == "default"
            }),
            "MySQL UUID VARCHAR literal case changes must drift: {case_drift:#?}"
        );

        let mut altered_identity = snapshot.clone();
        let altered_ids = altered_identity.tables.get_mut("ids").expect("ids table");
        altered_ids
            .columns
            .iter_mut()
            .find(|column| column.name == "auto_id")
            .expect("auto_id")
            .identity = None;
        altered_ids
            .columns
            .iter_mut()
            .find(|column| column.name == "ordinary")
            .expect("ordinary")
            .identity = Some(IdentityCol { always: false });
        let identity_drift = diff_snapshots(&snapshot, &altered_identity);
        for name in ["auto_id", "ordinary"] {
            assert!(
                identity_drift.altered_objects.iter().any(|altered| {
                    altered.object == format!("column {name}") && altered.field == "identity"
                }),
                "an AUTO_INCREMENT drop/add on {name} must drift: {identity_drift:?}"
            );
        }
        altered_identity
            .tables
            .get_mut("ids")
            .expect("ids table")
            .columns
            .iter_mut()
            .find(|column| column.name == "auto_id")
            .expect("auto_id")
            .identity = Some(IdentityCol { always: true });
        assert!(
            diff_snapshots(&snapshot, &altered_identity)
                .altered_objects
                .iter()
                .any(|altered| {
                    altered.object == "column auto_id" && altered.field == "identity"
                }),
            "an always/by-default identity flip must drift"
        );
    }

    #[compio::test]
    async fn mysql_auto_increment_add_and_drop_are_recovered_from_catalog_extra() {
        let column = |identity: Option<IdentityCol>| IrColumn {
            name: "id".to_string(),
            ty: ColType::BigInt,
            nullable: Some(false),
            default: None,
            unique: None,
            value_format: None,
            references: None,
            id_prefix: None,
            case_sensitive: None,
            vector_metric: None,
            mask: None,
            generated: None,
            identity,
        };
        let expected = |identity| {
            crate::render::fold::fold_ops(
                &[Op::CreateTable {
                    name: "identity_probe".to_string(),
                    columns: vec![column(identity)],
                    primary_key: Some(vec!["id".to_string()]),
                    constraints: Vec::new(),
                    indexes: Vec::new(),
                    partition_by: None,
                    runtime_options: None,
                    schema: None,
                    existence_guard: None,
                }],
                SqlDialect::Mysql,
                "proj_x",
                &crate::test_fixtures::no_inject("app"),
            )
            .expect("identity probe must fold")
        };
        let snapshot = |extra: &'static str| async move {
            let session = RecordingSession::with_catalog(
                vec![Row::new(
                    vec!["table_name".into()],
                    vec![Value::Text("identity_probe".into())],
                )],
                vec![catalog_column_with_generation(
                    "identity_probe",
                    "id",
                    "bigint",
                    None,
                    None,
                    false,
                    1,
                    None,
                    extra,
                )],
                vec![catalog_index_part(
                    "identity_probe",
                    "PRIMARY",
                    0,
                    1,
                    Some("id"),
                    None,
                    Some("A"),
                    None,
                )],
                Vec::new(),
            );
            MysqlBackend::new_generic(&session)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .expect("identity catalog snapshot")
        };

        let auto = snapshot("auto_increment").await;
        let plain = snapshot("").await;
        let expected_auto = expected(Some(IdentityCol { always: false }));
        let expected_plain = expected(None);
        assert!(
            diff_snapshots(&expected_auto, &auto).is_clean(),
            "portable AUTO_INCREMENT must match catalog EXTRA"
        );
        assert!(
            diff_snapshots(&expected_plain, &plain).is_clean(),
            "portable non-identity key must remain clean"
        );
        for (expected_snapshot, actual_snapshot, label) in
            [(&auto, &plain, "drop"), (&plain, &auto, "add")]
        {
            let drift = diff_snapshots(expected_snapshot, actual_snapshot);
            assert!(
                drift.altered_objects.iter().any(|altered| {
                    altered.object == "column id" && altered.field == "identity"
                }),
                "an out-of-band AUTO_INCREMENT {label} must drift: {drift:#?}"
            );
        }
    }

    #[compio::test]
    async fn snapshot_schema_compares_mysql_literal_defaults_on_format_typed_references() {
        const LITERAL: &str = "account_00000000000000000000000000";
        const CHANGED_LITERAL: &str = "account_00000000000000000000000001";
        let ir: MigrationIr = serde_json::from_value(json!({
            "ir_version": CURRENT_IR_VERSION,
            "name": "mysql_typed_reference_literal_default",
            "owner_app": "app_mysql_drift",
            "ops": [
                {
                    "op": "createTable",
                    "name": "parents",
                    "columns": [{
                        "name": "id",
                        "type": "text",
                        "nullable": false,
                        "valueFormat": { "typeId": { "prefix": "account" } },
                        "default": { "literal": { "value": LITERAL } }
                    }],
                    "primaryKey": null,
                    "constraints": [],
                    "indexes": []
                },
                {
                    "op": "createTable",
                    "name": "children",
                    "columns": [{
                        "name": "parent_id",
                        "type": "text",
                        "nullable": true,
                        "valueFormat": { "typeId": { "prefix": "account" } },
                        "default": { "literal": { "value": LITERAL } },
                        "references": {
                            "table": "parents",
                            "column": "id",
                            "onDelete": "cascade",
                            "onUpdate": "cascade"
                        }
                    }],
                    "primaryKey": null,
                    "constraints": [],
                    "indexes": []
                }
            ]
        }))
        .expect("typed-reference literal fixture must deserialize");
        let expected = crate::render::fold::fold_ops(
            &ir.ops,
            SqlDialect::Mysql,
            "proj_x",
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("typed-reference literal fixture must fold");
        let fk_name = expected.tables["children"]
            .constraints
            .iter()
            .find(|constraint| constraint.kind == "FOREIGN KEY")
            .expect("typed reference folds to a foreign key")
            .name
            .clone();
        let type_id_check = crate::render::value_format::column_metadata(
            "id",
            &ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            SqlDialect::Mysql,
        )
        .expect("TypeID metadata")
        .inline_check;
        let table_rows = || {
            ["children", "parents"]
                .into_iter()
                .map(|table| {
                    Row::new(
                        vec!["table_name".into()],
                        vec![Value::Text(table.to_string())],
                    )
                })
                .collect()
        };
        let column_rows = |child_default: Option<&str>| {
            vec![
                catalog_column_with_generation(
                    "children",
                    "parent_id",
                    "varchar(191)",
                    Some("ascii"),
                    Some("ascii_bin"),
                    true,
                    1,
                    child_default,
                    "",
                ),
                catalog_column_with_generation(
                    "parents",
                    "id",
                    "varchar(191)",
                    Some("ascii"),
                    Some("ascii_bin"),
                    false,
                    1,
                    Some(LITERAL),
                    "",
                ),
            ]
        };
        let foreign_keys = || {
            vec![catalog_foreign_key_part(
                "children",
                &fk_name,
                1,
                1,
                "parent_id",
                "proj_x",
                "parents",
                "id",
                "CASCADE",
                "CASCADE",
            )]
        };
        let checks = || {
            vec![catalog_check(
                "parents",
                "parents_chk_1",
                true,
                &type_id_check,
            )]
        };

        let clean_session = RecordingSession::with_catalog_checks(
            table_rows(),
            column_rows(Some(LITERAL)),
            Vec::new(),
            foreign_keys(),
            checks(),
        );
        let clean = MysqlBackend::new_generic(&clean_session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("clean typed-reference literal snapshot");
        let child = clean.tables["children"]
            .columns
            .iter()
            .find(|column| column.name == "parent_id")
            .expect("typed reference column");
        assert_eq!(
            child.id_default,
            Some(IdDefaultSnapshot::Literal(
                serde_json::to_string(LITERAL).expect("literal serializes")
            )),
            "the child inherits its ID-default comparison surface through the foreign key"
        );
        assert_eq!(child.default.as_deref(), Some(LITERAL));
        assert_eq!(
            clean.tables["parents"].columns[0].id_default,
            Some(IdDefaultSnapshot::Literal(
                serde_json::to_string(LITERAL).expect("literal serializes")
            )),
            "a non-expression MySQL COLUMN_DEFAULT must recover as a string literal"
        );
        let drift = diff_snapshots(&expected, &clean);
        assert!(
            drift.is_clean(),
            "bare MySQL COLUMN_DEFAULT literals must match authored typed literals: {drift:#?}"
        );

        for (catalog_default, label) in [(Some(CHANGED_LITERAL), "changed"), (None, "removed")] {
            let session = RecordingSession::with_catalog_checks(
                table_rows(),
                column_rows(catalog_default),
                Vec::new(),
                foreign_keys(),
                checks(),
            );
            let actual = MysqlBackend::new_generic(&session)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .unwrap_or_else(|error| panic!("{label} literal snapshot: {error}"));
            let drift = diff_snapshots(&expected, &actual);
            assert!(
                drift.altered_objects.iter().any(|altered| {
                    altered.object == "column parent_id" && altered.field == "default"
                }),
                "an out-of-band {label} typed-reference literal must drift: {drift:#?}"
            );
        }
    }

    #[compio::test]
    async fn snapshot_schema_preserves_mysql_expression_markers_on_fk_columns() {
        let table_rows = || {
            ["children", "parents"]
                .into_iter()
                .map(|table| {
                    Row::new(
                        vec!["table_name".into()],
                        vec![Value::Text(table.to_string())],
                    )
                })
                .collect()
        };
        let column_rows = |extra: &str| {
            vec![
                catalog_column_with_generation(
                    "children",
                    "parent_id",
                    "varchar(36)",
                    Some("ascii"),
                    Some("ascii_bin"),
                    true,
                    1,
                    Some(MYSQL_CATALOG_UUID_V4_DEFAULT),
                    extra,
                ),
                catalog_column_with_generation(
                    "parents",
                    "id",
                    "varchar(36)",
                    Some("ascii"),
                    Some("ascii_bin"),
                    false,
                    1,
                    None,
                    "",
                ),
            ]
        };
        let foreign_keys = || {
            vec![catalog_foreign_key_part(
                "children",
                "children_parent_id_fkey",
                1,
                1,
                "parent_id",
                "proj_x",
                "parents",
                "id",
                "NO ACTION",
                "NO ACTION",
            )]
        };
        let snapshot = |extra: &'static str| async move {
            let session = RecordingSession::with_catalog(
                table_rows(),
                column_rows(extra),
                Vec::new(),
                foreign_keys(),
            );
            MysqlBackend::new_generic(&session)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .expect("typed-reference expression snapshot")
        };

        let expression = snapshot("DEFAULT_GENERATED").await;
        let literal = snapshot("").await;
        let child_default = |snapshot: &SchemaSnapshot| {
            snapshot.tables["children"]
                .columns
                .iter()
                .find(|column| column.name == "parent_id")
                .and_then(|column| column.id_default.clone())
        };
        assert_eq!(child_default(&expression), Some(IdDefaultSnapshot::UuidV4));
        assert_eq!(
            child_default(&literal),
            Some(IdDefaultSnapshot::Literal(
                serde_json::to_string(MYSQL_CATALOG_UUID_V4_DEFAULT)
                    .expect("literal generator spelling serializes")
            ))
        );
        let drift = diff_snapshots(&expression, &literal);
        assert!(
            drift.altered_objects.iter().any(|altered| {
                altered.object == "column parent_id" && altered.field == "default"
            }),
            "a same-text literal must not satisfy an expression default: {drift:#?}"
        );
    }

    #[compio::test]
    async fn mysql_key_format_checks_drop_prefix_and_clause_changes_drift_from_catalog() {
        let ir: MigrationIr = serde_json::from_value(json!({
            "ir_version": CURRENT_IR_VERSION,
            "name": "mysql_key_formats",
            "owner_app": "app_mysql_drift",
            "ops": [
                {
                    "op": "createTable",
                    "name": "type_keys",
                    "columns": [{
                        "name": "id",
                        "type": "text",
                        "nullable": false,
                        "valueFormat": { "typeId": { "prefix": "account" } }
                    }],
                    "primaryKey": ["id"],
                    "constraints": [],
                    "indexes": []
                },
                {
                    "op": "createTable",
                    "name": "ulid_keys",
                    "columns": [{
                        "name": "id",
                        "type": "text",
                        "nullable": false,
                        "valueFormat": "ulid"
                    }],
                    "primaryKey": ["id"],
                    "constraints": [],
                    "indexes": []
                }
            ]
        }))
        .expect("MySQL key-format fixture must deserialize");
        let expected = crate::render::fold::fold_ops(
            &ir.ops,
            SqlDialect::Mysql,
            "proj_x",
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("MySQL key-format fixture must fold");
        let type_check = crate::render::value_format::column_metadata(
            "id",
            &ValueFormat::TypeId {
                prefix: "account".to_string(),
            },
            SqlDialect::Mysql,
        )
        .expect("TypeID key metadata")
        .inline_check;
        let team_check = crate::render::value_format::column_metadata(
            "id",
            &ValueFormat::TypeId {
                prefix: "team".to_string(),
            },
            SqlDialect::Mysql,
        )
        .expect("altered TypeID key metadata")
        .inline_check;
        let ulid_check = crate::render::value_format::column_metadata(
            "id",
            &ValueFormat::Ulid,
            SqlDialect::Mysql,
        )
        .expect("ULID key metadata")
        .inline_check;
        let altered_ulid_check =
            ulid_check.replacen("CHAR_LENGTH(`id`) = 26", "CHAR_LENGTH(`id`) = 25", 1);
        assert_ne!(
            altered_ulid_check, ulid_check,
            "ULID clause fixture must change"
        );

        let snapshot = |checks: Vec<Row>| async move {
            let session = RecordingSession::with_catalog_checks(
                ["type_keys", "ulid_keys"]
                    .into_iter()
                    .map(|table| {
                        Row::new(
                            vec!["table_name".into()],
                            vec![Value::Text(table.to_string())],
                        )
                    })
                    .collect(),
                ["type_keys", "ulid_keys"]
                    .into_iter()
                    .map(|table| {
                        catalog_column(
                            table,
                            "id",
                            "varchar(191)",
                            Some("ascii"),
                            Some("ascii_bin"),
                            false,
                            1,
                        )
                    })
                    .collect(),
                ["type_keys", "ulid_keys"]
                    .into_iter()
                    .map(|table| {
                        catalog_index_part(
                            table,
                            "PRIMARY",
                            0,
                            1,
                            Some("id"),
                            None,
                            Some("A"),
                            None,
                        )
                    })
                    .collect(),
                Vec::new(),
                checks,
            );
            MysqlBackend::new_generic(&session)
                .snapshot_schema(&ExecutorConfig::new(
                    "prj_x",
                    "proj_x",
                    crate::test_fixtures::no_inject("proj_x"),
                ))
                .await
                .expect("MySQL key-format catalog snapshot")
        };
        let check = |table: &str, clause: &str| {
            catalog_check(table, &format!("{table}_chk_1"), true, clause)
        };
        let clean = snapshot(vec![
            check("type_keys", &type_check),
            check("ulid_keys", &ulid_check),
        ])
        .await;
        let clean_drift = diff_snapshots(&expected, &clean);
        assert!(
            clean_drift.is_clean(),
            "authored key formats must match MySQL catalog CHECKs: {clean_drift:#?}"
        );

        for (label, checks, table, actual) in [
            (
                "TypeID drop",
                vec![check("ulid_keys", &ulid_check)],
                "type_keys",
                "",
            ),
            (
                "TypeID prefix change",
                vec![
                    check("type_keys", &team_check),
                    check("ulid_keys", &ulid_check),
                ],
                "type_keys",
                "typeId(team)",
            ),
            (
                "ULID drop",
                vec![check("type_keys", &type_check)],
                "ulid_keys",
                "",
            ),
            (
                "ULID clause change",
                vec![
                    check("type_keys", &type_check),
                    check("ulid_keys", &altered_ulid_check),
                ],
                "ulid_keys",
                "",
            ),
        ] {
            let actual_snapshot = snapshot(checks).await;
            let drift = diff_snapshots(&expected, &actual_snapshot);
            assert!(
                drift.altered_objects.iter().any(|altered| {
                    altered.table == table
                        && altered.object == "column id"
                        && altered.field == "format"
                        && altered.actual == actual
                }),
                "{label} must drift through catalog introspection: {drift:#?}"
            );
        }
    }

    #[compio::test]
    async fn snapshot_schema_rejects_semantically_regrouped_mysql_format_check() {
        let value_format = ValueFormat::TypeId {
            prefix: "account".to_string(),
        };
        let ir: MigrationIr = serde_json::from_value(json!({
            "ir_version": CURRENT_IR_VERSION,
            "name": "mysql_regrouped_type_id_check",
            "owner_app": "app_mysql_drift",
            "ops": [{
                "op": "createTable",
                "name": "ids",
                "columns": [{
                    "name": "id",
                    "type": "text",
                    "nullable": false,
                    "valueFormat": { "typeId": { "prefix": "account" } }
                }],
                "primaryKey": null,
                "constraints": [],
                "indexes": []
            }]
        }))
        .expect("regrouped TypeID fixture must deserialize");
        let expected = crate::render::fold::fold_ops(
            &ir.ops,
            SqlDialect::Mysql,
            "proj_x",
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("regrouped TypeID fixture must fold");
        let canonical =
            crate::render::value_format::column_metadata("id", &value_format, SqlDialect::Mysql)
                .expect("TypeID metadata")
                .inline_check;
        let regrouped = canonical.replacen("CHECK (", "CHECK (((", 1).replacen(
            "OR (CHAR_LENGTH(`id`) = 34 AND ",
            "OR CHAR_LENGTH(`id`) = 34) AND ",
            1,
        );
        assert_ne!(regrouped, canonical, "the CHECK mutation must take effect");
        let erase_grouping = |sql: &str| {
            sql.chars()
                .filter(|character| !character.is_whitespace() && !matches!(character, '(' | ')'))
                .collect::<String>()
        };
        assert_eq!(
            erase_grouping(&regrouped),
            erase_grouping(&canonical),
            "the regression must keep token order and differ only in semantic grouping"
        );

        let session = RecordingSession::with_catalog_checks(
            vec![Row::new(
                vec!["table_name".into()],
                vec![Value::Text("ids".into())],
            )],
            vec![catalog_column_with_generation(
                "ids",
                "id",
                "varchar(191)",
                Some("ascii"),
                Some("ascii_bin"),
                false,
                1,
                None,
                "",
            )],
            Vec::new(),
            Vec::new(),
            vec![catalog_check("ids", "ids_chk_1", true, &regrouped)],
        );
        let actual = MysqlBackend::new_generic(&session)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("regrouped TypeID snapshot");
        assert_eq!(
            actual.tables["ids"].columns[0].value_format, None,
            "a regrouped nullable guard is not the canonical TypeID contract"
        );
        let drift = diff_snapshots(&expected, &actual);
        assert!(
            drift
                .altered_objects
                .iter()
                .any(|altered| { altered.object == "column id" && altered.field == "format" }),
            "semantic CHECK regrouping must surface as format drift: {drift:#?}"
        );
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

    #[compio::test]
    async fn named_table_unique_candidate_and_composite_fk_have_clean_mysql_drift() {
        fn bigint_column(name: &str, nullable: bool) -> IrColumn {
            IrColumn {
                name: name.to_string(),
                ty: ColType::BigInt,
                nullable: Some(nullable),
                default: None,
                unique: None,
                value_format: None,
                references: None,
                id_prefix: None,
                case_sensitive: None,
                vector_metric: None,
                mask: None,
                generated: None,
                identity: None,
            }
        }

        let parent_key_name = "parents_tenant_external_key";
        let child_fk_name = "children_parent_fkey";
        let ops = vec![
            Op::CreateTable {
                name: "parents".to_string(),
                columns: vec![
                    bigint_column("tenant_id", false),
                    bigint_column("external_id", false),
                ],
                primary_key: None,
                constraints: vec![IrConstraint {
                    name: Some(parent_key_name.to_string()),
                    kind: IrConstraintKind::Unique {
                        columns: vec!["tenant_id".to_string(), "external_id".to_string()],
                    },
                }],
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            },
            Op::CreateTable {
                name: "children".to_string(),
                columns: vec![
                    bigint_column("parent_tenant", true),
                    bigint_column("parent_external", true),
                ],
                primary_key: None,
                constraints: vec![IrConstraint {
                    name: Some(child_fk_name.to_string()),
                    kind: IrConstraintKind::Fk {
                        columns: vec!["parent_tenant".to_string(), "parent_external".to_string()],
                        references_table: "parents".to_string(),
                        references_columns: vec![
                            "tenant_id".to_string(),
                            "external_id".to_string(),
                        ],
                        on_delete: None,
                        on_update: None,
                        deferrable: None,
                        initially_deferred: None,
                        not_valid: None,
                    },
                }],
                indexes: Vec::new(),
                partition_by: None,
                runtime_options: None,
                schema: None,
                existence_guard: None,
            },
        ];
        let expected = crate::render::fold::fold_ops(
            &ops,
            SqlDialect::Mysql,
            "proj_x",
            &crate::test_fixtures::no_inject("app"),
        )
        .expect("named table UNIQUE and composite FK fold for MySQL");

        let parent_candidate = expected.tables["parents"]
            .indexes
            .iter()
            .find(|index| index.name == parent_key_name)
            .expect("table UNIQUE canonicalizes to its MySQL unique key");
        assert!(parent_candidate.unique);
        assert_eq!(parent_candidate.columns, ["tenant_id", "external_id"]);
        assert!(
            expected.tables["parents"]
                .constraints
                .iter()
                .all(|constraint| constraint.name != parent_key_name),
            "MySQL cannot recover whether a unique key was authored as CONSTRAINT or INDEX"
        );

        let rec = RecordingSession::with_catalog(
            vec![
                Row::new(
                    vec!["table_name".into()],
                    vec![Value::Text("children".into())],
                ),
                Row::new(
                    vec!["table_name".into()],
                    vec![Value::Text("parents".into())],
                ),
            ],
            vec![
                catalog_column("children", "parent_tenant", "bigint", None, None, true, 1),
                catalog_column("children", "parent_external", "bigint", None, None, true, 2),
                catalog_column("parents", "tenant_id", "bigint", None, None, false, 1),
                catalog_column("parents", "external_id", "bigint", None, None, false, 2),
            ],
            vec![
                catalog_index_part(
                    "children",
                    "children_parent_fkey_idx",
                    1,
                    1,
                    Some("parent_tenant"),
                    None,
                    Some("A"),
                    None,
                ),
                catalog_index_part(
                    "children",
                    "children_parent_fkey_idx",
                    1,
                    2,
                    Some("parent_external"),
                    None,
                    Some("A"),
                    None,
                ),
                catalog_index_part(
                    "parents",
                    parent_key_name,
                    0,
                    1,
                    Some("tenant_id"),
                    None,
                    Some("A"),
                    None,
                ),
                catalog_index_part(
                    "parents",
                    parent_key_name,
                    0,
                    2,
                    Some("external_id"),
                    None,
                    Some("A"),
                    None,
                ),
            ],
            vec![
                catalog_foreign_key_part(
                    "children",
                    child_fk_name,
                    1,
                    1,
                    "parent_tenant",
                    "proj_x",
                    "parents",
                    "tenant_id",
                    "RESTRICT",
                    "RESTRICT",
                ),
                catalog_foreign_key_part(
                    "children",
                    child_fk_name,
                    2,
                    2,
                    "parent_external",
                    "proj_x",
                    "parents",
                    "external_id",
                    "RESTRICT",
                    "RESTRICT",
                ),
            ],
        );
        let actual = MysqlBackend::new_generic(&rec)
            .snapshot_schema(&ExecutorConfig::new(
                "prj_x",
                "proj_x",
                crate::test_fixtures::no_inject("proj_x"),
            ))
            .await
            .expect("equivalent MySQL catalog snapshot");

        assert_eq!(
            actual.tables["parents"]
                .indexes
                .iter()
                .find(|index| index.name == parent_key_name)
                .expect("introspected candidate key")
                .columns,
            ["tenant_id", "external_id"],
            "candidate-key tuple order must survive MySQL introspection"
        );
        let drift = diff_snapshots(&expected, &actual);
        assert!(
            drift.is_clean(),
            "named table UNIQUE + composite FK must round-trip without false MySQL drift: {drift:?}"
        );
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

    #[compio::test]
    async fn snapshot_failure_aborts_before_author_sql_and_releases_lock() {
        let rec = RecordingSession::with_failure("@@SESSION.sql_mode");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let migration = trivial_migration();

        let result = crate::apply::executor::apply_with_lock_backend(
            &backend,
            &cfg,
            &[migration],
            crate::Approval::Approved,
            &crate::ApprovalScope::All,
            "tester",
            crate::apply::executor::LockMode::Acquire,
        )
        .await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message))
                if message.contains("cannot verify the dedicated session is idle")),
            "{result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(all.contains("GET_LOCK(?, ?)") && all.contains("RELEASE_LOCK(?)"));
        assert!(
            !all.contains("CREATE TABLE t (id INT)")
                && !all.contains("CREATE DATABASE IF NOT EXISTS"),
            "snapshot failure must stop before journal bootstrap and author SQL: {all}"
        );
    }

    #[compio::test]
    async fn active_caller_transaction_is_rejected_before_autocommit_or_author_sql() {
        let rec = RecordingSession::with_in_transaction(1);
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let migration = trivial_migration();

        let result = crate::apply::executor::apply_with_lock_backend(
            &backend,
            &cfg,
            &[migration],
            crate::Approval::Approved,
            &crate::ApprovalScope::All,
            "tester",
            crate::apply::executor::LockMode::Acquire,
        )
        .await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message))
                if message.contains("dedicated idle session")
                    && message.contains("active transaction")),
            "{result:?}"
        );
        let all = rec.log.borrow().join("\n");
        assert!(
            all.contains("performance_schema.events_transactions_current")
                && all.contains("PS_CURRENT_THREAD_ID()")
        );
        assert!(all.contains("RELEASE_LOCK(?)"));
        assert!(
            !all.contains("SESSION autocommit = 1")
                && !all.contains("CREATE TABLE t (id INT)")
                && !all.contains("CREATE DATABASE IF NOT EXISTS"),
            "an active caller transaction must remain untouched: {all}"
        );
    }

    #[compio::test]
    async fn successful_apply_surfaces_restore_failure_after_releasing_lock() {
        let rec = RecordingSession::with_failure("SET SESSION sql_mode = ?");
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let migration = trivial_migration();

        let result = crate::apply::executor::apply_with_lock_backend(
            &backend,
            &cfg,
            &[migration],
            crate::Approval::Approved,
            &crate::ApprovalScope::All,
            "tester",
            crate::apply::executor::LockMode::Acquire,
        )
        .await;

        assert!(matches!(result, Err(ApplyError::Db(_))), "{result:?}");
        let log = rec.log.borrow();
        let author = log
            .iter()
            .position(|entry| entry == "batch: CREATE TABLE t (id INT)")
            .expect("author SQL ran");
        let restore = log
            .iter()
            .position(|entry| entry.contains("SET SESSION sql_mode = ?"))
            .expect("restore was attempted");
        let release = log
            .iter()
            .rposition(|entry| entry.contains("RELEASE_LOCK(?)"))
            .expect("project lock was released");
        assert!(author < restore && restore < release, "{log:?}");
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
        for (table, column) in [
            ("schema_migrations", "version"),
            ("schema_migrations", "checksum"),
            ("schema_migrations_supersedes", "squash_version"),
            ("schema_migrations_supersedes", "superseded_version"),
            ("schema_migrations_inflight", "version"),
            ("schema_migrations_inflight", "checksum"),
            ("schema_migrations_recovery", "version"),
            ("schema_migrations_recovery", "checksum"),
        ] {
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
                params.len() == 6 && params.last() == Some(&Bind::Text("repeatable".to_string()))
            }),
            "the completed event must remain visible to latest_completed_checksums: {binds:?}"
        );
    }

    /// An unmatched inflight marker is evidence that auto-committing DDL may
    /// already have landed. Recovery must preserve that evidence and fail closed;
    /// blindly deleting the marker and replaying a bare CREATE/ALTER can wedge the
    /// deployment or apply only part of a multi-statement migration.
    #[compio::test]
    async fn crashed_ddl_is_not_blindly_replayed() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let m = trivial_migration();

        let result = backend
            .apply_one(&cfg, &m, "tester", true, &[], "apply")
            .await;

        assert!(
            matches!(result, Err(ApplyError::Backend(ref message))
                if message.contains("inflight")
                    && message.contains("inspect")
                    && message.contains("recover_inflight_ddl")
                    && message.contains(m.version.as_str())),
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::Approved,
                &crate::approval::ApprovalScope::Versions(Default::default()),
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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

    #[compio::test]
    async fn whole_plan_preflight_refuses_delete_before_earlier_insert() {
        let rec = RecordingSession::new();
        let backend = MysqlBackend::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let (insert, _) = plan_dml_step("insert user", false);
        let (delete, delete_version) = plan_dml_step("delete user", true);

        let result = crate::MigrationEngine::new()
            .apply_plan_with_touched_and_depends_scoped(
                &[insert, delete],
                &["users".into()],
                &[],
                crate::approval::Approval::Approved,
                &crate::approval::ApprovalScope::Versions(Default::default()),
                &backend,
                &cfg,
                "tester",
                crate::apply::executor::LockMode::Acquire,
                None,
            )
            .await;

        assert!(matches!(
            result,
            Err(crate::engine::DeclarativeApplyError::Plain(
                crate::engine::EngineError::ApprovalNotScoped { ref version }
            )) if version == delete_version.as_str()
        ));
        let log = rec.log.borrow();
        assert!(
            !log.iter().any(|entry| {
                entry.contains("INSERT INTO `proj_x`.`users`")
                    || entry.contains("DELETE FROM `proj_x`.`users`")
                    || entry.contains("information_schema.TABLES")
            }),
            "scope preflight must refuse before either target inspection or mutation: {log:?}"
        );
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
            cursor_stability: crate::model::ir::CursorStability::ExternalInvariant {
                name: "users_id_immutable_during_backfill".into(),
            },
            cursor_contract: Some(crate::model::backfill::CursorContract {
                columns: vec![crate::model::backfill::CursorColumnContract {
                    name: "id".into(),
                    scalar_type: crate::model::backfill::CursorScalarType::Int64,
                    database_type: "bigint".into(),
                    comparison: crate::model::backfill::CursorComparison::Default,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::Approved,
                &crate::approval::ApprovalScope::Versions(Default::default()),
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
                crate::approval::Approval::None,
                &crate::approval::ApprovalScope::All,
                "tester",
                crate::apply::executor::LockMode::AlreadyHeld,
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
