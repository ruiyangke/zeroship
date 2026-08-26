//! The PostgreSQL adoption baseline: record an existing project DB's schema as a
//! `completed` journal event WITHOUT running its `up`.
//!
//! This body used to live in `zeroship_migrate_backend::baseline`, next to the dialect-neutral
//! [`BaselineOutcome`]/[`BaselineError`] vocabulary it returns - and from there it
//! called `apply::backend::postgres::journal_sql` by name. That made core's baseline
//! verb PostgreSQL's baseline verb: SQLite has its own body in
//! `zeroship_migrate_sqlite::backend::journal_sql` (named in prose, not linked - that
//! crate is below this one in the graph, so this crate's docs cannot resolve into
//! it), MySQL refuses, and neither could ever have been reached through the module
//! that named this one.
//!
//! The neutral half stayed where it was. `BaselineOutcome` and `BaselineError` are
//! the [`MigrationBackend::baseline_one`](zeroship_migrate_backend::backend::MigrationBackend::baseline_one)
//! signature, so every backend still speaks them; only the PostgreSQL IMPLEMENTATION
//! moved down here, where naming `journal_sql` is naming yourself.

use zeroship_migrate_backend::baseline::{BaselineError, BaselineOutcome};
use zeroship_migrate_backend::conn::ExecutorConfig;
use zeroship_migrate_backend::driver::SqlSession;
use zeroship_migrate_backend::journal::{self, JournalError};
use zeroship_migrate_ir::migration::Migration;

use super::journal_sql;

/// Record `baseline_migration` as the project's baseline
/// - a `completed` journal event WITHOUT running its `up`.
///
/// This is the **Postgres impl behind**
/// [`MigrationBackend::baseline_one`](zeroship_migrate_backend::backend::MigrationBackend::baseline_one)
/// (multi-engine abstraction): it is `pub(crate)`, reached only through
/// [`PostgresBackend::baseline_one`](super::PostgresBackend), which keeps the
/// `&Client`/`pg_advisory_lock` confined to the PG backend. There is no longer a
/// top-level PG-`&Client`-typed `baseline` on the public surface; callers go through
/// `backend.baseline_one(...)`.
///
/// Idempotent for the same baseline version (a retried deploy is safe); refuses a
/// *different* baseline once one exists, and refuses entirely if the engine
/// already manages real (non-baseline) history.
///
/// `applied_by` is the actor recorded in the journal (operator / admin).
///
/// # Errors
/// - [`BaselineError::Guard`] - the baseline SQL was denied (held to the same
/// deny-list as any `up`).
/// - [`BaselineError::AlreadyManaged`] - the journal already records net-applied
/// migrations (not a first-entry DB).
/// - [`BaselineError::ConflictingBaseline`] - a different baseline already exists.
/// - [`BaselineError::Db`] / [`BaselineError::Journal`] - infrastructure failures.
pub(crate) async fn baseline<B: zeroship_migrate_backend::backend::MigrationBackend, D: SqlSession>(
    backend: &B,
    conn: &D,
    cfg: &ExecutorConfig,
    dialect: &zeroship_migrate_ir::dialect::DialectId,
    baseline_migration: &Migration,
    applied_by: &str,
) -> Result<BaselineOutcome, BaselineError> {
    // GUARD (defense in depth) - BEFORE the lock, no DB needed. A baseline that
    // carries a denied/cross-schema construct is refused even though it never runs.
    let guard = crate::guard::guard(&cfg.guard_config_for(dialect));
    guard
        .check(&baseline_migration.up)
        .map_err(|source| BaselineError::Guard {
            version: baseline_migration.version.as_str().to_string(),
            source,
        })?;

    // Privileged: serialize against all migration activity, exactly like apply.
    // Held for the whole operation; released on every exit.
    //
    // Through the backend seam rather than inlined SQL, so this acquire gets the
    // grant compensation every other one has: an engine can record a session
    // advisory lock and still fail the acquiring statement, and a caller told the
    // acquisition failed has nothing to release with.
    backend.acquire_project_lock(cfg).await?;
    let result = baseline_locked(conn, cfg, baseline_migration, applied_by).await;
    let unlock = backend.release_project_lock(cfg).await;
    match result {
        Ok(o) => unlock.map(|()| o).map_err(BaselineError::Lock),
        Err(e) => Err(e),
    }
}

/// The baseline body, run while holding the project advisory lock.
async fn baseline_locked<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    baseline_migration: &Migration,
    applied_by: &str,
) -> Result<BaselineOutcome, BaselineError> {
    journal_sql::ensure_journal(conn, cfg).await?;

    let version = baseline_migration.version.as_str();

    // Idempotency + first-entry check. Read net-applied state once.
    let applied = journal_sql::applied(conn, cfg).await?;
    let net_applied: Vec<&str> = applied
        .iter()
        .filter(|e| e.phase == journal::Phase::Completed)
        .map(|e| e.version.as_str())
        .collect();

    // Same baseline already present => idempotent no-op (retried deploy).
    if net_applied.contains(&version) {
        return Ok(BaselineOutcome {
            version: version.to_string(),
            already_present: true,
        });
    }

    // A DIFFERENT net-applied migration exists => the engine already manages this
    // DB. If it is itself a baseline, surface the precise conflict; otherwise the
    // generic already-managed error.
    if !net_applied.is_empty() {
        // Is the existing net-applied entry a baseline? Report the clearer error.
        if let Some(existing_baseline) = first_baseline_version(conn, cfg).await? {
            return Err(BaselineError::ConflictingBaseline {
                project: cfg.project_id.clone(),
                requested: version.to_string(),
                existing: existing_baseline,
            });
        }
        return Err(BaselineError::AlreadyManaged {
            project: cfg.project_id.clone(),
            existing: i64::try_from(net_applied.len()).unwrap_or(i64::MAX),
        });
    }

    // First entry: journal the baseline as a `completed` event WITHOUT running the
    // `up`. ADMIN write (the migrator has no meta-schema grant), `kind='baseline'`.
    journal_sql::record_baseline(
        conn,
        cfg,
        journal::BaselineRecord {
            version,
            name: &baseline_migration.name,
            checksum: baseline_migration.checksum.as_str(),
            applied_by,
            kind: "baseline",
            supersedes: &[],
        },
    )
    .await?;

    Ok(BaselineOutcome {
        version: version.to_string(),
        already_present: false,
    })
}

/// The version of the earliest recorded `kind='baseline'` event, if any.
async fn first_baseline_version<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Option<String>, BaselineError> {
    // Engine-supplied meta schema: route through the ONE shared engine seam so it
    // fails closed on an empty / NUL name, byte-identical to the hand-rolled
    // quoting it replaced. This is a journal-table read, so the fail-closed error
    // is mapped through `JournalError` (which carries `From<IdentQuoteError>`).
    let meta = zeroship_migrate_backend::dml::quote_ident_checked_for_backend(
        &cfg.confinement.meta_schema,
        &crate::dml::RENDERER,
    )
    .map_err(JournalError::from)?;
    let rows = conn
        .query(
            &format!(
                "SELECT version FROM {meta}.schema_migrations
                  WHERE kind = 'baseline'
                  ORDER BY event_seq ASC
                  LIMIT 1"
            ),
            &[],
        )
        .await?;
    Ok(rows
        .first()
        .map(|r| r.try_get::<_, String>("version"))
        .transpose()?)
}
