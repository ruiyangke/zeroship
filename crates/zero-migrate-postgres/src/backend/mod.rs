//! Postgres [`MigrationBackend`](zero_migrate_backend::backend::MigrationBackend)
//! implementation.
//!
//! Generic over the dialect-neutral
//! [`SqlSession`](zero_migrate_backend::driver::SqlSession) seam - a host driver (the
//! napi `pg` shell) supplies the impl. SQLite does NOT ride this seam (it is an
//! in-process rusqlite actor).

use zero_migrate_backend::driver::SqlSession;

mod backfill_sql;
/// The PostgreSQL adoption baseline (record-not-run), relocated out of the neutral
/// `apply::baseline` module, which now holds only the vocabulary all three backends
/// speak.
mod baseline_sql;
/// The Postgres catalog reads behind drift: the `pg_catalog`/`information_schema`
/// introspection, the catalog-text parsers, and the PG journal read the checksum
/// comparison runs on. `pub` because these reads are PostgreSQL-shaped and are
/// reached BY THAT NAME; the crate root promises nothing dialect-specific.
pub mod drift_sql;
mod identity_sql;
pub mod journal_sql;
/// The Postgres precondition evaluator: the `pg_query` shape gate that proves a
/// `SqlBoolean` cannot mutate state, and the `information_schema` catalog reads the
/// structured checks run. `pub(crate)` because the crate root re-exports its two
/// public entry points at their historical `zero_migrate::...` paths.
pub(crate) mod precondition;
mod primary_key_sql;
/// The Postgres dialect SQL leaves (session/lock/txn/journal/DML/rollback) this
/// backend drives - relocated out of the generic `apply::executor` so no
/// dialect-specific SQL lives in the shared executor.
pub(crate) mod session;
/// The PostgreSQL `REPEATABLE READ READ ONLY` status snapshot, relocated out of
/// the neutral `zero_migrate::ops::status` module, whose remaining verbs go through
/// [`MigrationBackend`](zero_migrate_backend::backend::MigrationBackend).
pub mod status_sql;

/// The canned `SqlSession` this backend's tests drive, shared with the engine's
/// integration tests through the `testing` feature. See its own header for why it
/// is not simply `#[cfg(test)]`.
#[cfg(any(test, feature = "testing"))]
pub mod recording;

use zero_migrate_backend::backend::{
    CrossDeployObligations, JournalFuture, MigrationBackend, ProjectLockAcquisition,
    PROJECT_LOCK_TRY_ATTEMPTS, PROJECT_LOCK_TRY_BACKOFF,
};
use zero_migrate_backend::backfill::BackfillSpec;
use zero_migrate_backend::baseline::{BaselineError, BaselineOutcome};
use zero_migrate_backend::capability::OnlineSchemaChange;
use zero_migrate_backend::conn::ExecutorConfig;
use zero_migrate_backend::drift::DriftError;
use zero_migrate_backend::executor::{ApplyError, RollbackError};
use zero_migrate_backend::journal::{self, AppliedEntry, JournalError};
use zero_migrate_backend::requirements::{DatabaseFeature, DatabaseRequirements};
use zero_migrate_backend::snapshot::SchemaSnapshot;
use zero_migrate_backend::step::BindValue;
use zero_migrate_backend::step::{AlterPrimaryKeyStep, SynchronizeIdentityStep};
use zero_migrate_backend::table_rebuild::TableRebuildSpec;
use zero_migrate_ir::dialect::DialectId;
use zero_migrate_ir::migration::{Migration, MigrationId};

pub(crate) use crate::DIALECT;

/// This crate's DECLARED identifier byte cap, read off its own descriptor rather than
/// restated as a literal, and handed to the dual-write name derivations in the backend
/// contract. The engine's author reads the same declaration through the registry, so
/// the name the author writes and the name this executor looks for are capped by ONE
/// number.
pub(crate) const IDENT_MAX_BYTES: usize =
    match crate::descriptor::POSTGRES_DESCRIPTOR.limits.identifier {
        zero_migrate_ir::backend::IdentifierLimit::Bytes(n) => n,
        zero_migrate_ir::backend::IdentifierLimit::Unbounded
        | zero_migrate_ir::backend::IdentifierLimit::Characters(_) => {
            panic!("PostgreSQL declares a BYTE identifier cap")
        }
    };

/// The Postgres session GUCs the backend restores on exit so its per-apply
/// settings never leak onto the pooled/long-lived connection.
///
/// The generic executor sees this only as
/// [`MigrationBackend::SessionSnapshot`] and never inspects the fields.
#[derive(Debug, Clone, Default)]
pub struct PostgresSessionSnapshot {
    /// PG `statement_timeout` GUC text (e.g. `"60s"`). Empty for a backend that
    /// has no such setting.
    pub statement_timeout: String,
    /// PG `lock_timeout` GUC text.
    pub lock_timeout: String,
    /// PG `search_path` GUC text.
    pub search_path: String,
}

/// This server's `server_version_num` floor for a plan-required database feature,
/// or `0` for a feature it has had throughout the engine's supported range.
///
/// A version FLOOR is this backend's own knowledge - its numbering scheme, its
/// release history - so it lives here rather than as a method on the neutral
/// [`DatabaseFeature`]. It was
/// `DatabaseFeature::minimum_postgres_version_num` in the contract crate, whose only
/// production caller was `verify_database_requirements` below: the engine asks WHAT a
/// plan needs, and each target answers whether it has it, in whatever terms its own
/// versions come in.
///
/// The three validation features are collected only for a target whose CHECK
/// enforcement is version-gated. This server has enforced them throughout, so it
/// answers `0` and they impose no floor here.
const fn minimum_server_version_num(feature: DatabaseFeature) -> i32 {
    match feature {
        DatabaseFeature::UuidV4Generation => 130_000,
        DatabaseFeature::UuidV7Generation => 180_000,
        DatabaseFeature::UuidValidation
        | DatabaseFeature::TypeIdValidation
        | DatabaseFeature::UlidValidation => 0,
    }
}

/// The generic Postgres [`MigrationBackend`] implementation.
///
/// Generic over the [`SqlSession`] driver seam. Online expand-contract work uses
/// the same generic DDL, backfill, journal, and lock primitives as ordinary host
/// apply. Shadow-database dry runs still require a separate provisioning harness
/// and therefore remain unavailable on this backend.
#[derive(Debug)]
pub struct PostgresBackend<'a, D: SqlSession> {
    conn: &'a D,
}

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
    type SessionSnapshot = PostgresSessionSnapshot;

    fn dialect(&self) -> DialectId {
        DIALECT
    }

    fn timeout_setting_names(&self) -> Option<(&'static str, &'static str)> {
        Some(("statement_timeout", "lock_timeout"))
    }

    fn preserves_authored_logical_columns(&self) -> bool {
        // PostgreSQL lowering consumes the refreshed catalog snapshot directly;
        // it does not maintain the logical-column side projection.
        false
    }

    fn projects_sdk_field_defs(&self) -> bool {
        // PostgreSQL does not rebuild tables from the SDK-shaped projection.
        false
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
            let minimum = minimum_server_version_num(feature);
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

    /// One `pg_try_advisory_lock` per attempt, with a small fixed number of
    /// attempts so a reader that arrives during the brief gap between two of a
    /// deploy's statements still gets a real answer instead of reporting
    /// contention that has already cleared. The attempt count is FIXED and small:
    /// retrying until the lock is free is the unbounded wait this method exists to
    /// avoid, only spelled with more round trips.
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
        let holders = session::project_lock_holders(self.conn, &cfg.project_id).await?;
        Ok(ProjectLockAcquisition::Busy(holders))
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
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        primary_key_sql::apply(self.conn, cfg, step, approval, scope, applied_by).await
    }

    async fn synchronize_identity(
        &self,
        cfg: &ExecutorConfig,
        step: &SynchronizeIdentityStep,
        applied_by: &str,
    ) -> Result<bool, ApplyError> {
        identity_sql::apply(self.conn, cfg, step, applied_by).await
    }

    async fn rollback_one_transactional(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<(), RollbackError> {
        session::rollback_one_transactional(self.conn, cfg, m, applied_by).await
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

    fn validate_non_txn(&self, m: &Migration) -> Result<(), ApplyError> {
        session::validate_non_txn_idempotent(m)
    }

    fn non_transactional_down_reason(&self, m: &Migration) -> Option<String> {
        session::non_transactional_down_reason(m)
    }

    async fn journal_exists(&self, cfg: &ExecutorConfig) -> Result<bool, JournalError> {
        let rows = self
            .conn
            .query(
                "SELECT 1 AS journal_exists
                   FROM pg_catalog.pg_class AS c
                   JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
                  WHERE n.nspname = $1
                    AND c.relname = 'schema_migrations'
                    AND c.relkind IN ('r', 'p')
                  LIMIT 1",
                &[cfg.confinement.meta_schema.as_str().into()],
            )
            .await?;
        Ok(!rows.is_empty())
    }

    async fn ensure_journal(&self, cfg: &ExecutorConfig) -> Result<(), JournalError> {
        journal_sql::ensure_journal(self.conn, cfg).await
    }

    async fn applied(&self, cfg: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
        journal_sql::applied(self.conn, cfg).await
    }

    async fn history(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<zero_migrate_backend::journal::HistoryEvent>, JournalError> {
        journal_sql::history(self.conn, cfg).await
    }

    async fn net_rolled_back_versions(
        &self,
        cfg: &ExecutorConfig,
    ) -> Result<Vec<String>, JournalError> {
        journal_sql::net_rolled_back(self.conn, cfg)
            .await
            .map(|entries| entries.into_iter().map(|entry| entry.version).collect())
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
        drift_sql::check_checksum_drift(self.conn, cfg, migrations).await
    }

    async fn snapshot_schema(&self, cfg: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
        drift_sql::snapshot_schema_for(self.conn, &cfg.project_schema).await
    }

    async fn evaluate_preconditions(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
    ) -> Result<zero_migrate_backend::executor::PreconditionVerdict, ApplyError> {
        precondition::evaluate_all(self.conn, cfg, &DIALECT, m).await
    }

    /// The blocking-dependency predicate, MEASURED against a live server by
    /// `tests/pg_column_drop_dependency_oracle.rs`, which calls THIS function and
    /// attempts a real drop per shape, asserting the two agree on every one.
    ///
    /// The oracle used to run its own SQL spelling of the same rule, which made an
    /// edit here invisible to it. It executes the shipped function now, so changing
    /// this query is what the oracle reports on.
    ///
    /// Refuse iff a NORMAL dependency exists whose own object does NOT also hold an
    /// AUTO edge on the column, or an AUTO dependency comes from an index whose
    /// OWNING constraint does not itself depend on the column.
    ///
    /// Both qualifiers are counterexamples the server supplied, not caution. "Any
    /// NORMAL dependency" is too NARROW for an EXCLUDE whose expression reads the
    /// column: that reports AUTO and is still refused, because the exclusion's index
    /// is internally owned by its constraint. It is simultaneously too WIDE for a
    /// CHECK constraint, which reports BOTH edges and is dropped: the AUTO edge is
    /// PostgreSQL's own record that it will remove the constraint rather than block
    /// on it. A view's rewrite rule and a generated column's default report NORMAL
    /// alone, and those do block. The same "does the object also depend on the column
    /// directly" question answers both legs, which is why an exclusion naming the
    /// column both plainly and in an expression is droppable while the
    /// expression-only form is not.
    ///
    /// The second leg asks about the OWNING constraint specifically, read from the
    /// internal edge's `refobjid`, rather than about any constraint on the column.
    /// Two separate exclusions on one column - one expression-only, one plain -
    /// otherwise cancel each other: the plain one supplies an unrelated AUTO
    /// `pg_constraint` edge, the expression-only one's index still blocks, and the
    /// uncorrelated form waved the drop through. That is the `excl_sep` shape.
    ///
    /// `pg_describe_object` renders each blocker the way PostgreSQL's own error
    /// DETAIL does, so the refusal names what the server would have named.
    /// Answer one plan-wide precondition through the SAME body the per-migration
    /// seam uses, so the two seams cannot reach different conclusions about one
    /// assertion or word the same refusal differently.
    async fn evaluate_plan_precondition(
        &self,
        cfg: &ExecutorConfig,
        version: &str,
        check: &zero_migrate_ir::precondition::Precondition,
    ) -> Result<zero_migrate_backend::backend::PlanPreconditionVerdict, ApplyError> {
        let (met, blockers) =
            precondition::evaluate_one(self.conn, cfg, &DIALECT, version, check).await?;
        if met {
            return Ok(zero_migrate_backend::backend::PlanPreconditionVerdict::Met);
        }
        Ok(
            zero_migrate_backend::backend::PlanPreconditionVerdict::Unmet {
                blockers: blockers.unwrap_or_default(),
            },
        )
    }

    async fn blocking_column_dependents(
        &self,
        cfg: &ExecutorConfig,
        table: &str,
        column: &str,
    ) -> Result<Vec<String>, ApplyError> {
        let rows = self
            .conn
            .query(
                "WITH dep AS (
                   SELECT d.deptype, d.classid, d.objid, d.objsubid
                     FROM pg_attribute att
                     JOIN pg_class c ON c.oid = att.attrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                     LEFT JOIN pg_depend d
                            ON d.refobjid = att.attrelid
                           AND d.refobjsubid = att.attnum
                           AND d.refclassid = 'pg_class'::regclass
                    WHERE n.nspname = $1 AND c.relname = $2 AND att.attname = $3
                      AND att.attnum > 0 AND NOT att.attisdropped
                 )
                 SELECT pg_describe_object(classid, objid, objsubid) AS blocker
                   FROM dep
                  WHERE (
                       deptype = 'n'
                       AND NOT EXISTS (
                         SELECT 1 FROM dep auto_edge
                          WHERE auto_edge.deptype = 'a'
                            AND auto_edge.classid = dep.classid
                            AND auto_edge.objid = dep.objid
                       )
                     )
                     OR (
                       deptype = 'a' AND classid = 'pg_class'::regclass
                       AND EXISTS (
                         SELECT 1 FROM pg_depend i
                          WHERE i.classid = 'pg_class'::regclass AND i.objid = dep.objid
                            AND i.deptype = 'i' AND i.refclassid = 'pg_constraint'::regclass
                            AND NOT EXISTS (
                              SELECT 1 FROM dep owner_edge
                               WHERE owner_edge.deptype = 'a'
                                 AND owner_edge.classid = 'pg_constraint'::regclass
                                 AND owner_edge.objid = i.refobjid
                            )
                       )
                     )
                  ORDER BY blocker",
                &[
                    cfg.project_schema.as_str().into(),
                    table.into(),
                    column.into(),
                ],
            )
            .await?;
        rows.iter()
            .map(|row| {
                row.try_get::<_, String>("blocker")
                    .map_err(ApplyError::from)
            })
            .collect()
    }

    /// The retype-blocking predicate, MEASURED against a live server by
    /// `tests/pg_column_retype_dependency_oracle.rs`, which calls THIS function and
    /// attempts a real `ALTER COLUMN ... TYPE` per shape.
    ///
    /// It is NOT the drop predicate with a filter bolted on, even though it reads
    /// the same `pg_depend` join. Three differences, each a counterexample the
    /// server supplied:
    ///
    /// 1. **Constraint-shaped dependents never block a retype.** PostgreSQL
    ///    revalidates a CHECK, and rebuilds a UNIQUE / EXCLUDE / PRIMARY KEY index,
    ///    rather than refusing. That includes the case the drop predicate exists
    ///    for: an EXCLUDE whose expression reads the column BLOCKS a drop and is
    ///    ACCEPTED for a retype. So the drop predicate's whole second leg - the one
    ///    about an index a constraint internally owns - has no subject here and is
    ///    absent, and with `pg_constraint` excluded the drop predicate's
    ///    "does this dependent also hold an AUTO edge" qualifier has no subject
    ///    either. It was a rule ABOUT constraints.
    /// 2. **A FOREIGN KEY pointing AT the column blocks a drop and does not block a
    ///    retype.** Falls out of (1) - the referencing constraint is a
    ///    `pg_constraint` dependent - and it is the row that makes reusing the drop
    ///    assertion an over-refusal rather than a conservative default.
    /// 3. **Two blockers are not dependencies at all.** A column in the table's
    ///    PARTITION KEY, and a column INHERITED from a parent table, are refused by
    ///    `ATPrepAlterColumnType` before any dependency is consulted and leave no
    ///    blocking `pg_depend` edge. They need their own catalog terms, and without
    ///    them the predicate under-refuses on two shapes a live server rejects.
    ///
    /// The partition-key term reads `pg_partitioned_table` two ways because a key
    /// can be spelled two ways. A plain column key lands in `partattrs`; an
    /// EXPRESSION key stores `0` there and records the columns it reads as an
    /// INTERNAL self-dependency of the relation on its own column instead. Both
    /// spellings were measured refused, and `has_partition_attrs` in the server
    /// consults exactly these two sources.
    ///
    /// `pg_describe_object` renders each blocker the way PostgreSQL's own error
    /// DETAIL does, so the refusal names what the server would have named.
    async fn column_type_change_blockers(
        &self,
        cfg: &ExecutorConfig,
        table: &str,
        column: &str,
    ) -> Result<Vec<String>, ApplyError> {
        let rows = self
            .conn
            .query(
                "WITH col AS (
                   SELECT att.attrelid, att.attnum, att.attinhcount
                     FROM pg_attribute att
                     JOIN pg_class c ON c.oid = att.attrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                    WHERE n.nspname = $1 AND c.relname = $2 AND att.attname = $3
                      AND att.attnum > 0 AND NOT att.attisdropped
                 ), dep AS (
                   SELECT d.classid, d.objid, d.objsubid
                     FROM col
                     JOIN pg_depend d
                            ON d.refobjid = col.attrelid
                           AND d.refobjsubid = col.attnum
                           AND d.refclassid = 'pg_class'::regclass
                    WHERE d.deptype = 'n'
                      AND d.classid <> 'pg_constraint'::regclass
                 )
                 SELECT blocker FROM (
                     SELECT pg_describe_object(classid, objid, objsubid) AS blocker
                       FROM dep
                   UNION ALL
                     SELECT 'partition key of ' || col.attrelid::regclass::text
                       FROM col
                      WHERE EXISTS (
                        SELECT 1 FROM pg_partitioned_table pt
                         WHERE pt.partrelid = col.attrelid
                           AND (col.attnum = ANY (pt.partattrs::int2[])
                                OR EXISTS (
                                  SELECT 1 FROM pg_depend pd
                                   WHERE pd.classid = 'pg_class'::regclass
                                     AND pd.objid = col.attrelid
                                     AND pd.objsubid = col.attnum
                                     AND pd.refclassid = 'pg_class'::regclass
                                     AND pd.refobjid = col.attrelid
                                     AND pd.refobjsubid = 0
                                     AND pd.deptype = 'i'
                                ))
                      )
                   UNION ALL
                     SELECT 'inherited column of ' || col.attrelid::regclass::text
                       FROM col
                      WHERE col.attinhcount > 0
                 ) s
                  ORDER BY blocker",
                &[
                    cfg.project_schema.as_str().into(),
                    table.into(),
                    column.into(),
                ],
            )
            .await?;
        rows.iter()
            .map(|row| {
                row.try_get::<_, String>("blocker")
                    .map_err(ApplyError::from)
            })
            .collect()
    }

    async fn record_squash(
        &self,
        cfg: &ExecutorConfig,
        squash_migration: &Migration,
        applied_by: &str,
        supersedes: &[&str],
    ) -> Result<(), ApplyError> {
        journal_sql::record_baseline(
            self.conn,
            cfg,
            zero_migrate_backend::journal::BaselineRecord {
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
        spec: &TableRebuildSpec,
        _m: &Migration,
        _scope: &zero_migrate_backend::approval::ApprovalScope,
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
        checksum: &zero_migrate_ir::migration::Checksum,
        spec: &BackfillSpec,
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: zero_migrate_backend::executor::LockMode,
    ) -> Result<zero_migrate_backend::executor::ApplyOutcome, ApplyError> {
        if let Some(entry) = self
            .applied(cfg)
            .await
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|entry| matches!(entry.phase, zero_migrate_backend::journal::Phase::Completed))
            .find(|entry| entry.version == version.as_str())
        {
            if entry.checksum != checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: version.as_str().to_string(),
                    recorded: entry.checksum,
                    expected: checksum.as_str().to_string(),
                });
            }
            return Ok(zero_migrate_backend::executor::ApplyOutcome {
                applied: Vec::new(),
                skipped: vec![version.as_str().to_string()],
                recovered: Vec::new(),
            });
        }
        if approval != zero_migrate_backend::approval::Approval::Approved {
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
        Ok(zero_migrate_backend::executor::ApplyOutcome {
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
        checksum: &zero_migrate_ir::migration::Checksum,
        name: &str,
        template: &str,
        binds: &[BindValue],
        _target_schema: &str,
        _target_table: &str,
        _conflict_target: Option<&[String]>,
        _mutates_data: bool,
        destructive: bool,
        _owner_app: &str,
        approval: zero_migrate_backend::approval::Approval,
        scope: &zero_migrate_backend::approval::ApprovalScope,
        applied_by: &str,
        _lock_mode: zero_migrate_backend::executor::LockMode,
    ) -> Result<bool, ApplyError> {
        let completed = self
            .applied(cfg)
            .await
            .map_err(ApplyError::Journal)?
            .into_iter()
            .filter(|e| matches!(e.phase, zero_migrate_backend::journal::Phase::Completed))
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
        if destructive && approval != zero_migrate_backend::approval::Approval::Approved {
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

    fn pending_contracts(&self) -> Option<&dyn CrossDeployObligations> {
        Some(self)
    }

    async fn baseline_one(
        &self,
        cfg: &ExecutorConfig,
        m: &Migration,
        applied_by: &str,
    ) -> Result<BaselineOutcome, BaselineError> {
        baseline_sql::baseline(self, self.conn, cfg, &DIALECT, m, applied_by).await
    }
}

impl<D: SqlSession> OnlineSchemaChange for PostgresBackend<'_, D> {
    /// Mirror the pre-existing rows of one online rename into the new column.
    ///
    /// This is the whole of PostgreSQL's online capability now. It used to be the
    /// whole online DRIVE: it applied E1/E2 by calling `apply_with_lock_backend`
    /// - the engine's orchestrator - back across the backend boundary, tripped an
    /// engine fault point, and read the journal to decide whether the marker still
    /// needed running. Those phases were neutral in every line and the engine owns
    /// them now; what is left here is the one phase that is irreducibly Postgres:
    /// naming the managed dual-write trigger this backfill is allowed to run
    /// beneath, and running the paged `UPDATE`.
    ///
    /// The trigger identity is derived from the `intent`, not accepted from the
    /// caller. `run_backfill` refuses to mirror rows under any trigger it was not
    /// told to expect, so deriving it here - from the same intent the trigger was
    /// authored from - is what keeps a caller from pointing the mirror at a
    /// trigger the engine never wrote.
    fn run_online_backfill<'a>(
        &'a self,
        intent: &'a zero_migrate_backend::capability::OnlineIntent,
        marker: &'a Migration,
        backfill: &'a BackfillSpec,
        approval: zero_migrate_backend::approval::Approval,
        scope: &'a zero_migrate_backend::approval::ApprovalScope,
        approval_key: &'a MigrationId,
        cfg: &'a ExecutorConfig,
        applied_by: &'a str,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        zero_migrate_backend::backfill::BackfillOutcome,
                        zero_migrate_backend::capability::OnlineError,
                    >,
                > + 'a,
        >,
    > {
        Box::pin(async move {
            // Executor-layer defense in depth. The engine ran both of these on the
            // same `approval_key` before it applied E1/E2; these are the
            // independent checks that stop a DIRECT seam caller from mirroring data
            // for a rename that was never approved, or never individually reviewed.
            if approval != zero_migrate_backend::approval::Approval::Approved {
                return Err(zero_migrate_backend::capability::OnlineError::Approval);
            }
            if !scope.admits(approval_key.as_str()) {
                return Err(
                    zero_migrate_backend::capability::OnlineError::ApprovalNotScoped {
                        version: approval_key.as_str().to_string(),
                    },
                );
            }
            let zero_migrate_backend::capability::OnlineIntent::RenameColumn {
                table,
                from,
                to,
                ..
            } = intent;
            let allowed_engine_trigger = backfill_sql::AllowedOnlineRenameTrigger::new(
                zero_migrate_backend::capability::dual_write_trg_name(
                    table,
                    from,
                    to,
                    IDENT_MAX_BYTES,
                ),
                zero_migrate_backend::capability::dual_write_fn_name(
                    table,
                    from,
                    to,
                    IDENT_MAX_BYTES,
                ),
                from.clone(),
                to.clone(),
            );
            Ok(backfill_sql::run_backfill(
                self.conn,
                cfg,
                &marker.version,
                &marker.checksum,
                backfill,
                approval,
                Some(&allowed_engine_trigger),
                applied_by,
            )
            .await?)
        })
    }
}

impl<D: SqlSession> CrossDeployObligations for PostgresBackend<'_, D> {
    fn outstanding_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::PendingContract>> {
        Box::pin(async move { journal_sql::outstanding_pending_contracts(self.conn, cfg).await })
    }

    fn resolved_pending_contracts<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
    ) -> JournalFuture<'a, Vec<journal::ResolvedPendingContract>> {
        Box::pin(async move { journal_sql::resolved_pending_contracts(self.conn, cfg).await })
    }

    fn pending_contract_shape<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        contract: &'a journal::PendingContract,
    ) -> JournalFuture<'a, journal::PendingContractShape> {
        Box::pin(async move {
            journal_sql::pending_contract_shape(self.conn, cfg, contract, &crate::fold::POLICY)
                .await
        })
    }

    fn record_pending_contract_with_recovery<'a>(
        &'a self,
        cfg: &'a ExecutorConfig,
        rec: journal::PendingContractRecord<'a>,
        scope: Option<journal::DeployRecoveryScope<'a>>,
    ) -> JournalFuture<'a, bool> {
        Box::pin(async move {
            journal_sql::record_pending_contract_with_recovery(self.conn, cfg, rec, scope).await
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
            journal_sql::resolve_pending_contract(self.conn, cfg, pc, resolution, by).await
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
            journal_sql::mark_deploy_recovery_committed_batch(
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
            journal_sql::mark_deploy_recovery_reconciled(
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
        Box::pin(async move { journal_sql::outstanding_deploy_recoveries(self.conn, cfg).await })
    }
}

/// Genericity proof: the apply path monomorphizes over a
/// **non-compio** [`SqlSession`] driver. An in-crate recording driver records the
/// SQL of every WRITE verb, and - now that the read side is widened to the
/// driver-neutral [`Row`]/[`DbError`] - RETURNS canned `Row`s from
/// its read verbs. This proves `PostgresBackend<'a, D>` is genuinely generic AND
/// that a host driver can build return values without a `compio_postgres::Row`,
/// closing the old `unreachable!("read verbs...")` gap.
#[cfg(test)]
#[cfg(test)]
mod recording_session_genericity {
    use super::*;
    use crate::backend::recording::{canned_journal_row, InFlightGuard, RecordingSession};
    use std::sync::atomic::AtomicBool;

    use zero_migrate_backend::driver::{Bind, Row, Value};
    use zero_migrate_backend::requirements::{DatabaseFeature, DatabaseRequirements};
    use zero_migrate_ir::migration::{Checksum, ChecksumInput, Migration, MigrationFlags};
    use zero_migrate_ir::probe::{GuardDir, GuardProbe};

    fn migration_with_guard_schema(schema: &str) -> Migration {
        let flags = MigrationFlags::default();
        let up = "CREATE TABLE guard_target (id bigint)";
        let checksum = Checksum::of(&ChecksumInput {
            up,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version: MigrationId::generate(),
            name: "guarded table".into(),
            up: up.into(),
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: Some(GuardProbe::Table {
                schema: schema.into(),
                table: "guard_target".into(),
                direction: GuardDir::IfNotExists,
                expect_columns: Vec::new(),
            }),
            effect: None,
        }
    }

    #[compio::test]
    async fn existence_guard_probe_outside_effective_scope_is_refused_before_snapshot() {
        let rec = RecordingSession::new();
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));
        let migration = migration_with_guard_schema("forbidden");
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
            other => panic!(
                "expected ExistenceGuardSchemaOutOfScope for the forged probe, got: {other:?}"
            ),
        }
    }

    #[compio::test]
    async fn existence_guard_probe_in_allowlist_snapshots_its_non_project_schema() {
        let rec = RecordingSession::new();
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let effective = crate::test_fixtures::operator_with_data_security(
            &["proj_x", "reporting"],
            &[],
            false,
            zero_migrate_ir::policy::DestructiveOps::Allow,
        );
        let cfg = ExecutorConfig::new("prj_x", "proj_x", effective);
        let migration = migration_with_guard_schema("reporting");

        let result = backend
            .apply_one(&cfg, &migration, "tester", false, &[], "apply")
            .await;

        assert!(!result.expect("the allowlisted probe must apply"));
        let log = rec.log.borrow();
        assert!(
            log.iter()
                .any(|entry| entry.starts_with("query: SELECT child.relname")),
            "the permitted probe must read the catalog: {log:?}"
        );
        assert!(
            log.iter()
                .any(|entry| entry == &format!("batch: {}", migration.up)),
            "the empty permitted snapshot must let the guarded create run: {log:?}"
        );
        let binds = rec.binds.borrow();
        assert!(
            binds.iter().any(|values| {
                matches!(values.as_slice(), [Bind::Text(schema)] if schema == "reporting")
            }),
            "the catalog snapshot must bind the probe schema: {binds:?}"
        );
        assert!(
            !binds.iter().any(|values| {
                matches!(values.as_slice(), [Bind::Text(schema)] if schema == "proj_x")
            }),
            "the probe snapshot must not fall back to the project schema: {binds:?}"
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
        assert_eq!(
            empty_backend.timeout_setting_names(),
            Some(("statement_timeout", "lock_timeout"))
        );
        assert!(!empty_backend.preserves_authored_logical_columns());
        assert!(!empty_backend.projects_sdk_field_defs());
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

        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        // Lock acquire/release + RESET ROLE - all write/DDL verbs, run through the
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
        // Bind path - the param widening ran, not just the return one.
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
    /// (`Row -> AppliedEntry`) runs end-to-end over a non-compio driver - the
    /// closure of the old `unreachable!("read verbs...")` gap.
    #[compio::test]
    async fn read_path_runs_generically_over_canned_seam_rows() {
        let rec =
            RecordingSession::with_canned_journal(vec![canned_journal_row("mig_0001", "deadbeef")]);
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

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

    /// Journal bootstrap must serve both sides of the rollout: fresh tables carry
    /// the nullable reverse SQL immediately, while an already-existing table from
    /// an older engine gains the same nullable column without rewriting its rows.
    #[compio::test]
    async fn journal_bootstrap_creates_and_upgrades_nullable_down_column() {
        let rec = RecordingSession::new();
        let backend = PostgresBackend::<'_, RecordingSession>::new_generic(&rec);
        let cfg = ExecutorConfig::new("prj_x", "proj_x", crate::test_fixtures::no_inject("proj_x"));

        backend
            .ensure_journal(&cfg)
            .await
            .expect("journal bootstrap succeeds");

        let log = rec.log.borrow();
        let create = log
            .iter()
            .find(|entry| {
                entry.contains("CREATE TABLE IF NOT EXISTS")
                    && entry.contains("schema_migrations (")
            })
            .expect("fresh journal table DDL");
        assert!(
            create.contains("down") && create.contains("down        TEXT"),
            "fresh journal stores nullable reverse SQL: {create}"
        );
        assert!(
            log.iter().any(|entry| {
                entry.contains("ALTER TABLE \"proj_x_migrations\".schema_migrations")
                    && entry.contains("ADD COLUMN IF NOT EXISTS down TEXT")
            }),
            "legacy journal bootstrap must add nullable down idempotently: {log:?}"
        );
    }

    /// The `server_version_num` floors this backend enforces, pinned in the crate
    /// that owns them.
    ///
    /// RELOCATED, not written fresh: `render::lower`'s
    /// `postgres_plan_records_uuid_server_requirements` asserted these two numbers
    /// while the floor table was a method on the neutral `DatabaseFeature`. The engine
    /// cannot reach a private `const fn` here, so the assertions came with the table
    /// rather than being dropped. What stayed in the engine is the half that is
    /// genuinely the engine's: that lowering a UUIDv4/v7 default RECORDS the two
    /// requirements on the plan.
    #[test]
    fn the_uuid_generators_carry_this_servers_own_version_floors() {
        assert_eq!(
            minimum_server_version_num(DatabaseFeature::UuidV4Generation),
            130_000,
            "gen_random_uuid() is core from 13"
        );
        assert_eq!(
            minimum_server_version_num(DatabaseFeature::UuidV7Generation),
            180_000,
            "uuidv7() is core from 18"
        );
        for enforced_throughout in [
            DatabaseFeature::UuidValidation,
            DatabaseFeature::TypeIdValidation,
            DatabaseFeature::UlidValidation,
        ] {
            assert_eq!(
                minimum_server_version_num(enforced_throughout),
                0,
                "this server has enforced CHECK constraints throughout the supported \
                 range, so a validation feature imposes no floor: {enforced_throughout:?}"
            );
        }
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
