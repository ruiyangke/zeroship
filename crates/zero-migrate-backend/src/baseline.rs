//! Baseline an existing project DB (design "Baseline existing db", scenario
//! 31) - the **adoption path**, and specifically its DIALECT-NEUTRAL vocabulary.
//!
//! [`BaselineOutcome`] and [`BaselineError`] are the `MigrationBackend::baseline_one`
//! signature, so all three backends speak them. The IMPLEMENTATIONS do not live
//! here and never could have: PostgreSQL's is its `postgres::baseline_sql`,
//! SQLite's is its own `journal_sql::baseline`, and MySQL refuses. What this module
//! used to hold was PostgreSQL's body reaching `postgres::journal_sql` by name,
//! which made "core's baseline" and "PostgreSQL's baseline" the same code.
//!
//! The vocabulary sits HERE, below every vendor, for the reason every other item in
//! this crate does: a type a backend's signature names cannot live in the engine
//! that already depends on every backend. The engine re-exports it at
//! `zero_migrate::apply::baseline`.
//!
//! A project DB may already physically carry its schema (created outside the
//! engine, or a legacy DB being adopted). `baseline` records a baseline
//! migration as a `completed` event in the journal **WITHOUT running its `up`**:
//! the schema already exists, so re-running `CREATE TABLE ...` would error. The
//! baseline's `up` *documents* the current schema (a FRESH rebuild could run it),
//! but on the existing DB it is recorded-not-run. Future migrations then apply on
//! top normally (`MigrationEngine::apply`).
//!
//! # Safety
//!
//! - **Guard-checked.** The baseline SQL is still run through the
//! registered line-1 guard ([`MigrationGuard`](crate::guard::MigrationGuard))
//! (defense-in-depth): it represents the
//! real schema and must not carry a denied/cross-schema construct, even though
//! it does not execute here.
//! - **First-entry only.** Baseline refuses if the journal already records ANY
//! net-applied migration - you cannot baseline a DB the engine already manages
//! ([`BaselineError::AlreadyManaged`]). Re-baselining the *same* baseline
//! version is an idempotent no-op (so a retried deploy is safe); a *different*
//! baseline once one exists is refused.
//! - **Privileged.** Baseline is an operator/admin operation (not creator
//! self-service): it runs as the ADMIN (it journals, which the migrator role has
//! no grant for) under the project advisory lock, serialized against every other
//! migration activity exactly like `MigrationEngine::apply`.
//! - **Append-only journal preserved.** The baseline event is an ordinary
//! immutable `completed` row stamped `kind = 'baseline'`; nothing is updated or
//! deleted.

use crate::guard::GuardError;
use crate::journal::JournalError;

/// What `baseline` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineOutcome {
    /// The version recorded as the baseline (`mig_...`).
    pub version: String,
    /// `true` if this was an idempotent re-baseline of the same version (nothing
    /// new was journaled); `false` if the baseline event was newly recorded.
    pub already_present: bool,
}

/// Error from `baseline`.
#[derive(Debug, thiserror::Error)]
pub enum BaselineError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] crate::driver::DbError),
    /// Taking or releasing the project lock failed.
    ///
    /// Carried as its own variant so baseline can route through the SAME
    /// `MigrationBackend::acquire_project_lock`
    /// every other acquire site uses. That seam compensates for a grant the engine
    /// recorded before failing the acquiring statement; inlining the raw advisory
    /// lock here would take the lock without the compensation, which is what it
    /// used to do.
    #[error(transparent)]
    Lock(#[from] crate::executor::ApplyError),
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
    /// The journal already records at least one net-applied migration - the engine
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
    /// `MigrationBackend::baseline_one`
    /// impl (e.g. the SQLite actor). The Postgres impl never produces this arm -
    /// its errors flow through the typed [`Db`](Self::Db)/[`Journal`](Self::Journal)/
    /// guard/first-entry arms above; only an engine whose internals are not
    /// `compio_postgres`-typed maps its own error string into here, mirroring
    /// [`ApplyError::Backend`](crate::executor::ApplyError::Backend).
    #[error("baseline backend error: {0}")]
    Backend(String),
}
