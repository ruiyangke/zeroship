//! Baseline an existing project DB (design §5 "Baseline existing db", scenario
//! 31) — the **adoption path**.
//!
//! A project DB may already physically carry its schema (created outside the
//! engine, or a legacy DB being adopted). [`baseline`] records a baseline
//! migration as a `completed` event in the journal **WITHOUT running its `up`**:
//! the schema already exists, so re-running `CREATE TABLE …` would error. The
//! baseline's `up` *documents* the current schema (a FRESH rebuild could run it),
//! but on the existing DB it is recorded-not-run. Future migrations then apply on
//! top normally ([`crate::executor::apply`]).
//!
//! # Safety (design §1, §5)
//!
//! - **Guard-checked.** The baseline SQL is still run through the
//!   [`SqlGuard`](crate::guard::SqlGuard) (defense-in-depth): it represents the
//!   real schema and must not carry a denied/cross-schema construct, even though
//!   it does not execute here.
//! - **First-entry only.** Baseline refuses if the journal already records ANY
//!   net-applied migration — you cannot baseline a DB the engine already manages
//!   ([`BaselineError::AlreadyManaged`]). Re-baselining the *same* baseline
//!   version is an idempotent no-op (so a retried deploy is safe); a *different*
//!   baseline once one exists is refused.
//! - **Privileged.** Baseline is an operator/admin operation (not creator
//!   self-service): it runs as the ADMIN (it journals, which the migrator role has
//!   no grant for) under the project advisory lock, serialized against every other
//!   migration activity exactly like [`apply`](crate::executor::apply).
//! - **Append-only journal preserved.** The baseline event is an ordinary
//!   immutable `completed` row stamped `kind = 'baseline'`; nothing is updated or
//!   deleted.

use compio_postgres::Client;

use crate::conn::ExecutorConfig;
use crate::guard::{GuardConfig, GuardError, SqlGuard};
use crate::apply::journal::{self, JournalError};
use crate::model::migration::Migration;

/// What [`baseline`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineOutcome {
    /// The version recorded as the baseline (`mig_…`).
    pub version: String,
    /// `true` if this was an idempotent re-baseline of the same version (nothing
    /// new was journaled); `false` if the baseline event was newly recorded.
    pub already_present: bool,
}

/// Error from [`baseline`].
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// The baseline SQL was denied by the guard (RCE / priv-esc / cross-tenant /
    /// file / network). A baseline represents the real schema and is held to the
    /// same parse-time deny-list as any `up` (defense in depth), even though it is
    /// recorded-not-run. Nothing was journaled.
    #[error("baseline {version} denied by guard: {source}")]
    Guard {
        /// The baseline migration's version.
        version: String,
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// The journal already records at least one net-applied migration — the engine
    /// already manages this DB, so it cannot be baselined. Baseline is a
    /// first-entry-only operation. Nothing was journaled.
    #[error(
        "cannot baseline project {project}: the journal already records {existing} net-applied \
         migration(s). Baseline is a first-entry operation; a DB the engine already manages \
         cannot be re-baselined."
    )]
    AlreadyManaged {
        /// The project id.
        project: String,
        /// How many net-applied migrations the journal already records.
        existing: i64,
    },
    /// A different baseline version already exists. Baseline is idempotent only for
    /// the SAME version; recording a different baseline once one is present is
    /// refused (it would mean two competing v0 descriptions). Nothing was journaled.
    #[error(
        "cannot baseline project {project} as {requested}: a different baseline ({existing}) \
         already exists. Baseline is idempotent only for the same version."
    )]
    ConflictingBaseline {
        /// The project id.
        project: String,
        /// The version being requested now.
        requested: String,
        /// The baseline version already recorded.
        existing: String,
    },
    /// A dialect-neutral backend failure from a NON-Postgres
    /// [`MigrationBackend::baseline_one`](crate::backend::MigrationBackend::baseline_one)
    /// impl (e.g. the SQLite actor). The Postgres impl never produces this arm —
    /// its errors flow through the typed [`Db`](Self::Db)/[`Journal`](Self::Journal)/
    /// guard/first-entry arms above; only an engine whose internals are not
    /// `compio_postgres`-typed maps its own error string into here, mirroring
    /// [`ApplyError::Backend`](crate::executor::ApplyError::Backend).
    #[error("baseline backend error: {0}")]
    Backend(String),
}

/// Record `baseline_migration` as the project's baseline (design §5, scenario 31)
/// — a `completed` journal event WITHOUT running its `up`.
///
/// This is the **Postgres impl behind**
/// [`MigrationBackend::baseline_one`](crate::backend::MigrationBackend::baseline_one)
/// (multi-engine abstraction L5): it is `pub(crate)`, reached only through
/// [`PostgresBackend::baseline_one`](crate::backend::PostgresBackend), which keeps the
/// `&Client`/`pg_advisory_lock` confined to the PG backend. There is no longer a
/// top-level PG-`&Client`-typed `baseline` on the public surface; callers go through
/// `backend.baseline_one(…)`.
///
/// Idempotent for the same baseline version (a retried deploy is safe); refuses a
/// *different* baseline once one exists, and refuses entirely if the engine
/// already manages real (non-baseline) history.
///
/// `applied_by` is the actor recorded in the journal (operator / admin).
///
/// # Errors
/// - [`BaselineError::Guard`] — the baseline SQL was denied (held to the same
///   deny-list as any `up`).
/// - [`BaselineError::AlreadyManaged`] — the journal already records net-applied
///   migrations (not a first-entry DB).
/// - [`BaselineError::ConflictingBaseline`] — a different baseline already exists.
/// - [`BaselineError::Db`] / [`BaselineError::Journal`] — infrastructure failures.
pub(crate) async fn baseline(
    conn: &Client,
    cfg: &ExecutorConfig,
    baseline_migration: &Migration,
    applied_by: &str,
) -> Result<BaselineOutcome, BaselineError> {
    // GUARD (defense in depth) — BEFORE the lock, no DB needed. A baseline that
    // carries a denied/cross-schema construct is refused even though it never runs.
    let guard = SqlGuard::new(GuardConfig::confined(cfg.project_schema.clone()));
    guard
        .check(&baseline_migration.up)
        .map_err(|source| BaselineError::Guard {
            version: baseline_migration.version.as_str().to_string(),
            source,
        })?;

    // Privileged: serialize against all migration activity (design §2.3 step 1),
    // exactly like apply. Held for the whole operation; released on every exit.
    conn.execute(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[&cfg.project_id],
    )
    .await?;
    let result = baseline_locked(conn, cfg, baseline_migration, applied_by).await;
    let unlock = conn
        .execute(
            "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
            &[&cfg.project_id],
        )
        .await;
    match result {
        Ok(o) => unlock.map(|_| o).map_err(BaselineError::Db),
        Err(e) => Err(e),
    }
}

/// The baseline body, run while holding the project advisory lock.
async fn baseline_locked(
    conn: &Client,
    cfg: &ExecutorConfig,
    baseline_migration: &Migration,
    applied_by: &str,
) -> Result<BaselineOutcome, BaselineError> {
    journal::ensure_journal(conn, cfg).await?;

    let version = baseline_migration.version.as_str();

    // Idempotency + first-entry check. Read net-applied state once.
    let applied = journal::applied(conn, cfg).await?;
    let net_applied: Vec<&str> = applied
        .iter()
        .filter(|e| e.phase == journal::Phase::Completed)
        .map(|e| e.version.as_str())
        .collect();

    // Same baseline already present ⇒ idempotent no-op (retried deploy).
    if net_applied.contains(&version) {
        return Ok(BaselineOutcome {
            version: version.to_string(),
            already_present: true,
        });
    }

    // A DIFFERENT net-applied migration exists ⇒ the engine already manages this
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
    journal::record_baseline(
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
async fn first_baseline_version(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Option<String>, BaselineError> {
    // Engine-supplied meta schema: route through the ONE shared engine seam so it
    // fails closed on an empty / NUL name, byte-identical to the prior
    // `escape_quote_ident`. This is a journal-table read, so the fail-closed error
    // is mapped through `JournalError` (which carries `From<IdentQuoteError>`).
    let meta = crate::render::dml::quote_ident_checked(&cfg.pg.meta_schema).map_err(JournalError::from)?;
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
    Ok(rows.first().map(|r| r.get::<_, String>("version")))
}
