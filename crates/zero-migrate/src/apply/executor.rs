//! The versioned executor — the apply flow.
//!
//! The heart of the engine. Given a connection, an [`ExecutorConfig`], and the
//! project's full migration set, [`apply`]:
//!
//! 1. acquires the project advisory lock `pg_advisory_lock(hashtext(project_id))`
//!    (serialize all migration activity; released at end);
//! 2. bootstraps the journal (idempotent);
//! 3. computes `pending = set − applied`, in `UUIDv7` version order;
//! 4. re-verifies the checksums of already-applied migrations — a mismatch is a
//!    hard abort (drift / tamper);
//! 5. **first pass (static, all-up-front):** runs the dialect-selected
//!    **[`MigrationGuard`](crate::guard::MigrationGuard)** over the
//!    `up` SQL of EVERY pending migration, and runs the backend's non-txn scan
//!    over every `up` taking the two-phase path. That scan is a DENY list of
//!    shapes known to break on a second run (`CREATE INDEX CONCURRENTLY` without
//!    `IF NOT EXISTS`, bare DML) - it is not a proof that what it accepts is
//!    idempotent, and an `up` it does not recognize is admitted. A denial aborts
//!    the whole apply before ANY migration executes - a denied batch applies
//!    *nothing* (no earlier migration half-commits);
//! 6. **second pass (execute):** for each pending migration, applies either:
//!    - **transactionally** (default): `BEGIN; SET LOCAL …; <up>; INSERT
//!      journal; COMMIT` — DDL + journal atomic, so a crash leaves
//!      applied+recorded *or* neither. The `SET LOCAL` timeouts/`search_path` are
//!      transaction-scoped, so they never leak onto the session;
//!    - **non-transactionally** (opt-in, e.g. `CREATE INDEX CONCURRENTLY IF NOT
//!      EXISTS`): two-phase `started` marker → run `<up>` → `completed` row +
//!      marker deletion, the last two in one transaction. A lone `started` marker
//!      on a re-run means the `up` may or may not have committed, and the journal
//!      cannot say which. The backend then classifies the `up`: the shapes it can
//!      prove converge on a second run have their INVALID-index residue dropped
//!      and are **re-run**, and every other one is REFUSED with the marker left
//!      armed and the repair named, rather than replayed on the chance it works.
//!      An armed marker also outranks an `OnUnmet::Skip` precondition, so a
//!      half-run version is never reported as a clean deploy.
//! 7. restores the session GUCs it touched and releases the lock.
//!
//! Runs out-of-band at deploy. The apply futures are driven by the host
//! (the napi `block_on` worker + JS host) — ZERO tokio, ZERO compio.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;

use crate::approval::Approval;
// The generic `PostgresBackend` compiles on the PG seam cfg (`pg_seam`, from
// `host-pg`) — the generic executor entries below are driver-neutral, and the
// dialect SQL leaves the backend drives live in
// [`crate::apply::backend::postgres::session`].
use crate::apply::backend::MigrationBackend;
#[cfg(pg_seam)]
use crate::apply::backend::MysqlBackend;
#[cfg(pg_seam)]
use crate::apply::backend::PostgresBackend;
use crate::apply::journal::{AppliedEntry, JournalError, Phase};
use crate::conn::ExecutorConfig;
#[cfg(pg_seam)]
use crate::driver::SqlSession;
use crate::guard::GuardError;
use crate::model::migration::{Migration, MigrationId};
use crate::render::plan::AppliedPlan;
use crate::render::step::PlanStep;

/// Whether an apply sub-batch must acquire/release the project advisory lock
/// itself, or whether an OUTER caller already holds it for the whole operation.
///
/// A standalone [`apply`] (engine `apply` / `apply_verified` / the versioned
/// path) uses [`LockMode::Acquire`]: it takes the project advisory lock at the
/// start and releases it on every exit path, serializing the whole apply against
/// concurrent deploys for the same project.
///
/// A **declarative** deploy is several sub-batches — the plain set plus one
/// expand per rename — that must be serialized **as a whole** (to
/// "serialize all migration activity"). The outer
/// [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative)
/// therefore acquires the lock ONCE up front and passes [`LockMode::AlreadyHeld`]
/// into every inner sub-batch so they SKIP the per-batch acquire/release — the
/// lock is acquired exactly once and released exactly once for the entire
/// declarative deploy, never freed between sub-batches (where a second deploy
/// could otherwise interleave).
///
/// `AlreadyHeld` gates ONLY the advisory-lock acquire/release. The per-sub-batch
/// session hygiene (GUC snapshot/restore, unconditional `RESET ROLE`) still runs
/// every sub-batch regardless of lock mode — those are session-leak guards,
/// independent of who owns the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// This call owns the lock: acquire at the start, release on every exit.
    Acquire,
    /// An outer caller already holds the project advisory lock for the whole
    /// operation — skip the per-batch acquire and release.
    AlreadyHeld,
}

/// What [`apply`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// Versions applied this run, in apply order. Empty = no-op.
    pub applied: Vec<String>,
    /// Versions that were already applied (skipped). Informational.
    pub skipped: Vec<String>,
    /// Versions recovered via the non-txn recovery path this run.
    pub recovered: Vec<String>,
}

impl ApplyOutcome {
    /// True if nothing was applied or recovered (idempotent re-run).
    #[must_use]
    pub const fn is_noop(&self) -> bool {
        self.applied.is_empty() && self.recovered.is_empty()
    }
}

/// Opaque driver/transport error carried by the dialect-neutral executor.
///
/// Backends store their original driver or transport error inside this wrapper,
/// so callers that need backend-specific details can downcast without forcing
/// `ApplyError` or `RollbackError` to name a concrete driver in their public shape.
#[derive(Debug)]
pub struct BackendError(Box<dyn Error + Send + Sync + 'static>);

impl BackendError {
    /// Wrap any backend driver/transport error without stringifying it.
    pub fn new<E>(error: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self(Box::new(error))
    }

    /// Recover a concrete backend error type when a caller intentionally needs
    /// backend-specific details, such as a Postgres SQLSTATE in tests.
    #[must_use]
    pub fn downcast_ref<E>(&self) -> Option<&E>
    where
        E: Error + Send + Sync + 'static,
    {
        self.0.downcast_ref::<E>()
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for BackendError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[cfg(pg_seam)]
impl From<crate::driver::DbError> for BackendError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::new(error)
    }
}

// All network (PG/MySQL) DB errors now funnel through the dialect-neutral
// `driver::DbError` seam: the only off-seam concrete-`Client` reader was the
// retired Rust CLI's standalone trailer/status path, so
// `BackendError` boxes the single `DbError` shape above. It still `downcast_ref`s
// to a concrete backend error type when a test needs SQLSTATE details.

/// Error from [`apply`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A database/driver error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[source] BackendError),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// MySQL records that this version's `down` began, but no successful outcome
    /// was journaled. The live schema may therefore be partially reverted, so an
    /// already-applied version must not be reported as a clean skip.
    #[error(
        "mysql migration {version} has a rollback marker from an interrupted unwind; its \
         `down` auto-committed an unknown number of statements, so the live shape is not the \
         one this `up` expects. Inspect the live schema against the migration's `down`, finish \
         or undo the partial revert yourself, then clear the marker with DELETE FROM \
         `{meta_schema}`.schema_migrations_rollback_inflight WHERE version = '{version}' and \
         run apply again"
    )]
    UnresolvedRollbackMarker {
        /// The supplied migration version carrying the unresolved marker.
        version: String,
        /// The MySQL meta database containing the rollback marker table.
        meta_schema: String,
    },
    /// A dialect-level backend error whose message is already the intended
    /// operator-facing text. Use [`ApplyError::Db`] /
    /// [`ApplyError::MigrationFailed`] for structured driver/transport failures.
    #[error("backend error: {0}")]
    Backend(String),
    /// A migration requested the **non-transactional** path (`transaction:false`)
    /// on a dialect that has no non-txn DDL to recover (SQLite).
    /// Rejected at the dialect boundary, before any apply. Postgres never returns
    /// this — its non-txn path is real.
    #[error("migration {version} is transaction:false but the {dialect} backend has no non-transactional DDL path")]
    NonTxnUnsupportedOnDialect {
        /// The rejected migration's version.
        version: String,
        /// The dialect that lacks a non-txn path (`"sqlite"`).
        dialect: &'static str,
    },
    /// The pending batch contains a destructive migration (`flags.destructive`)
    /// but the caller passed [`Approval::None`]. This is the executor's OWN
    /// defense-in-depth approval gate — independent of (and additional to) the
    /// engine's gate ([`crate::engine::MigrationEngine::apply`]), so a caller that
    /// drives [`apply`] directly, bypassing the engine, still cannot run a
    /// destructive batch without explicit approval. Nothing was applied.
    #[error("apply contains a destructive migration but Approval::Approved was not given")]
    ApprovalRequired,
    /// **Per-version approval scoping (anti-bypass).** The batch is approved
    /// ([`crate::Approval::Approved`]) but it carries a DESTRUCTIVE migration whose
    /// version-id is NOT in the operator's reviewed
    /// [`ApprovalScope::Versions`](crate::ApprovalScope::Versions) set. The
    /// executor's OWN defense-in-depth scope gate (mirrors
    /// [`ApprovalRequired`](Self::ApprovalRequired)): a direct caller cannot run a
    /// co-bundled destructive op the operator never individually reviewed, even with
    /// blanket [`Approved`](crate::Approval::Approved). Nothing was applied.
    #[error(
        "apply contains destructive migration '{version}' that is not in the approved \
         version scope (per-version approval required)"
    )]
    ApprovalNotScoped {
        /// The destructive migration version-id the scope refused.
        version: String,
    },
    /// The SQL guard denied a pending migration's `up` SQL — the whole apply is
    /// aborted and the migration never executed.
    #[error("migration {version} denied by guard: {source}")]
    Guard {
        /// The denied migration's version.
        version: String,
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// A non-transactional migration's `up` contains a statement that is not
    /// safe to re-run idempotently — e.g. `CREATE INDEX CONCURRENTLY` without
    /// `IF NOT EXISTS`, or `ALTER TYPE … ADD VALUE` without `IF NOT EXISTS`.
    ///
    /// The two-phase non-txn path's crash-recovery re-runs `<up>` verbatim,
    /// so a non-idempotent op would wedge the migration permanently on
    /// a success-then-crash (`already exists` / `label already exists`). We
    /// reject such a migration **before any execution** with this clear error;
    /// the author must write the `IF NOT EXISTS` form.
    #[error(
        "migration {version} is non-transactional but its `up` is not idempotent: {reason}. \
         Non-transactional migrations may be re-run by crash recovery, so each statement \
         must use the `IF NOT EXISTS` form."
    )]
    NonIdempotentNonTxn {
        /// The offending migration's version.
        version: String,
        /// What specifically is not idempotent.
        reason: String,
    },
    /// A non-transactional migration left an armed inflight marker behind, and
    /// recovery cannot prove its `up` is safe to re-run verbatim. Nothing is
    /// replayed, the marker is preserved, and the operator is handed the repair.
    ///
    /// This is the fail-closed arm of the two-phase recovery path. The alternative,
    /// measured against a live server: a non-transactional `CREATE TABLE` whose
    /// `up` committed before the crash replays into `relation ... already exists`,
    /// re-arms the marker it just cleared, and reports the identical failure on
    /// every deploy from then on. The version never lands and nothing about
    /// re-running the deploy changes that.
    ///
    /// The refusal is deliberately NOT a fresh-apply gate: an `up` outside the
    /// replay-safe set still applies, and only a crash that interrupted it lands
    /// here.
    #[error(
        "migration {version} has an inflight marker from an interrupted non-transactional \
         apply, and zero-migrate cannot prove its `up` is safe to re-run ({reason}), so it \
         will not replay statements that may already have committed. Inspect the live schema \
         against the migration SQL, restore and verify the complete pre-migration shape \
         yourself, clear the marker with DELETE FROM \"{meta_schema}\".schema_migrations_inflight \
         WHERE version = '{version}', then run apply again. Clearing the marker inspects \
         nothing: it records your assertion about the shape. Do not hand-write a completed \
         event into the append-only journal"
    )]
    NonTxnRecoveryUnsafe {
        /// The version whose marker is preserved.
        version: String,
        /// Why the `up` is not provably re-runnable.
        reason: String,
        /// The meta schema holding the inflight side-table, so the repair the
        /// message spells out is the one this deploy would actually read.
        meta_schema: String,
    },
    /// An online rename would end by dropping a column other objects depend on,
    /// so its contract could never complete. Refused before the expand half runs.
    ///
    /// The check is deliberately NOT a CASCADE: `DROP COLUMN ... CASCADE` removes
    /// the dependents too, which for a rename would destroy objects the operator
    /// never named inside a step approved for one column.
    #[error(
        "renaming {table:?}.{column:?} cannot complete: dropping the old column at the \
         end of the rename would be refused because {blockers:?} depend on it. The \
         expand half is not started, because finishing it would leave a contract \
         obligation that can never be discharged. Drop or redefine the dependents \
         first, or rename the column by hand with the dependents rebuilt around it"
    )]
    RenameSourceHasDependents {
        /// The table carrying the column being renamed.
        table: String,
        /// The old column name, the one the contract step would drop.
        column: String,
        /// What the database says depends on it, rendered by `pg_describe_object`
        /// so the wording matches the server's own error DETAIL.
        blockers: Vec<String>,
    },
    /// A `setColumnType` in this plan names a column the database will not let it
    /// retype. Refused before ANY step of the plan runs.
    ///
    /// The refusal is a whole-plan one rather than a per-migration one because the
    /// damage this prevents is not the failed statement. Each lowered unit commits
    /// in its own transaction, so an envelope carrying `addColumn` before the
    /// retype leaves the added column COMMITTED when the retype dies against the
    /// server, and the operator is left with a schema that is neither the old shape
    /// nor the new one. Asking under the lock before the first step runs is what
    /// makes the plan all-or-nothing at this boundary.
    ///
    /// Deliberately NOT a CASCADE and deliberately not a repair: the blockers are
    /// views, rules, generated columns, policies, triggers and publications the
    /// operator never named in a step approved for one column's type, and dropping
    /// them silently is a larger change than the one authored.
    #[error(
        "changing the type of {table:?}.{column:?} would be refused by the database because \
         {blockers:?} block it. No step of this plan has run, because a plan that dies at \
         this statement would leave the steps before it committed against a schema that is \
         neither the old shape nor the new one. Redefine or drop the blockers first, or \
         change the type by hand with them rebuilt around it"
    )]
    ColumnTypeChangeBlocked {
        /// The table carrying the column being retyped.
        table: String,
        /// The column whose type the plan would change.
        column: String,
        /// What the database says blocks the retype, rendered by
        /// `pg_describe_object` so the wording matches the server's own error
        /// DETAIL, plus the two structural blockers that are not dependencies at
        /// all (partition-key membership, column inheritance).
        blockers: Vec<String>,
    },
    /// An already-applied migration's recorded checksum no longer matches the
    /// migration in the set — drift / tamper. Hard abort.
    ///
    /// The recorded side is not always a completed event: an inflight marker
    /// records the checksum of the body that half-ran, so editing a
    /// non-transactional migration in place after an interrupted apply lands
    /// here BEFORE the backend's interrupted-marker refusal can explain the
    /// repair. The message names that possibility so the operator knows to look
    /// at the marker.
    #[error(
        "checksum drift on {version}: journal has {recorded}, set has {expected}. \
         If this version was interrupted mid-apply, {recorded} may be an inflight \
         marker recording the body that half-ran rather than a completed event: \
         restore the reviewed migration source unedited and resolve the marker, \
         instead of editing the migration to match"
    )]
    ChecksumDrift {
        /// The drifting migration's version.
        version: String,
        /// The checksum recorded in the journal.
        recorded: String,
        /// The checksum of the migration now in the set.
        expected: String,
    },
    /// A pending migration's `depends_on` names a version that is neither already
    /// applied nor present in the supplied set — the dependency graph is
    /// unsatisfiable, so no ordering exists. Hard abort before any execution.
    #[error(
        "migration {version} depends on unknown migration {missing} (not in the set or journal)"
    )]
    MissingDependency {
        /// The migration with the dangling dependency.
        version: String,
        /// The unknown version it depends on.
        missing: String,
    },
    /// The pending migrations' `depends_on` edges form a cycle, so no topological
    /// order exists. Hard abort before any execution.
    #[error("dependency cycle among pending migrations: {0}")]
    DependencyCycle(String),
    /// A `phase: Contract` online migration was about to be applied
    /// while a depended-on `phase: Expand` migration is **not net-applied in the
    /// journal**. The contract (drop trigger/function, drop old column) must never
    /// land before its expand (add column, dual-write, backfill) is fully done and
    /// recorded — otherwise old/new shapes stop coexisting and concurrent writes
    /// are lost. Refused before any execution; nothing is applied.
    ///
    /// The single source of truth is the JOURNAL: the gate reads net-applied
    /// expand completions there, so the expand and contract can land in SEPARATE
    /// deploys (cross-deploy partition) and the gate still enforces ordering.
    #[error(
        "contract migration {version} requires its expand migration {expand} to be \
         net-applied in the journal first (it is not); apply the expand phase before the contract"
    )]
    ExpandNotApplied {
        /// The contract migration being refused.
        version: String,
        /// The depended-on expand migration that is not net-applied.
        expand: String,
    },
    /// A pending squash migration (`supersedes = [v1..vN]`) was about to run its
    /// `up`, but ALL of `[v1..vN]` are already net-applied — running `S.up` would
    /// re-create existing objects (double-apply). On an existing DB the squash must
    /// be recorded WITHOUT running its `up` via [`crate::ops::squash`]; apply refuses
    /// here before any execution. Nothing was applied.
    #[error(
        "squash migration {version} cannot be applied: all the versions it supersedes are already \
         applied. Use squash() to record the supersession without re-running its up."
    )]
    SquashAlreadyApplied {
        /// The squash migration being refused.
        version: String,
    },
    /// A pending squash migration (`supersedes = [v1..vN]`) has a PARTIAL overlap
    /// with the journal: some but not all of `[v1..vN]` are net-applied. A squash
    /// may only run on a FRESH set (none applied) or be recorded on a fully-applied
    /// set (all applied via [`crate::ops::squash`]); a partial set is an inconsistent
    /// state. Refused before any execution; nothing was applied.
    #[error(
        "squash migration {version} has a partial overlap: {applied} of {total} superseded \
         versions are already applied. A squash requires either NONE applied (fresh: run its up) \
         or ALL applied (existing: record via squash())."
    )]
    SquashPartialOverlap {
        /// The squash migration being refused.
        version: String,
        /// How many of its superseded versions are net-applied.
        applied: usize,
        /// The total number of versions it supersedes.
        total: usize,
    },
    /// Two distinct squash migrations IN THE SAME apply set both supersede the same
    /// version — a malformed bundle (a version may be collapsed by at most one
    /// squash). If both ran, the second's `up` would re-create what the first's
    /// already built (double-apply); the fresh-path all-or-none gate cannot catch
    /// this because neither squash is net-applied yet, so it is refused up-front,
    /// before any execution. Nothing was applied.
    #[error(
        "squash migrations {first} and {second} both supersede {shared}: a version may be \
         collapsed by at most one squash — split them across deploys or merge them"
    )]
    OverlappingSquashes {
        /// One squash superseding the shared version.
        first: String,
        /// The other squash superseding the shared version.
        second: String,
        /// The version both squashes supersede.
        shared: String,
    },
    /// A pending migration carried a precondition with
    /// [`OnUnmet::Halt`](crate::model::precondition::OnUnmet::Halt) that was UNMET (it
    /// evaluated false), or a precondition that could not be evaluated at all (a
    /// guard-denied / malformed `SqlBoolean`, an invalid identifier). Fail-closed:
    /// the whole apply is aborted before this migration's `up` runs, and NOTHING
    /// is applied for this migration (and the batch stops). Preconditions are
    /// evaluated read-only, under the advisory lock, immediately before the `up`.
    #[error("migration {version} precondition not met / not evaluable: {which}")]
    PreconditionFailed {
        /// The migration whose precondition failed.
        version: String,
        /// Which precondition failed and why (the unmet assertion, or the
        /// evaluation error — e.g. a guard denial or invalid identifier).
        which: String,
    },
    /// A `repeatable=true` migration ALSO carried a non-empty `supersedes` — a
    /// repeatable cannot be a squash. A repeatable has a
    /// stable identity and re-applies on change; a squash collapses once-only
    /// history. The two are mutually exclusive. Refused in the pre-flight over the
    /// FULL supplied set, before partition/apply; nothing was applied.
    #[error(
        "migration {version} is repeatable but also declares `supersedes`: a repeatable \
         cannot be a squash — remove `supersedes` or make it a once-only squash"
    )]
    RepeatableCannotSquash {
        /// The malformed repeatable-with-supersedes migration.
        version: String,
    },
    /// A VERSIONED (once-only) migration's `depends_on` names a REPEATABLE in the
    /// same supplied set. Repeatables run AFTER all
    /// versioned migrations, so a once-only migration can never have a repeatable
    /// dependency satisfied in order. This is a DEDICATED error (not the misleading
    /// `MissingDependency` the partition would otherwise raise). Refused in the
    /// pre-flight before any execution; nothing was applied.
    #[error(
        "once-only migration {version} may not depend on repeatable {dependency}: a repeatable \
         applies after all versioned migrations, so the dependency can never be ordered before it"
    )]
    OnceOnlyDependsOnRepeatable {
        /// The once-only migration with the illegal dependency.
        version: String,
        /// The repeatable it depends on.
        dependency: String,
    },
    /// A `repeatable=true` migration declared a `down`.
    /// A repeatable is replace-style (`CREATE OR REPLACE …`) with no true reverse,
    /// so its `down` MUST be `None` (the stated invariant). Refused in the pre-flight
    /// before any execution; nothing was applied.
    #[error(
        "repeatable migration {version} must not declare a `down`: a repeatable is \
         replace-style and has no true reverse"
    )]
    RepeatableHasDown {
        /// The repeatable that wrongly declared a `down`.
        version: String,
    },
    /// Applying a migration's `up` SQL failed (after the guard passed). The
    /// transaction (txn path) was rolled back; nothing was journaled.
    #[error("migration {version} failed to apply: {source}")]
    MigrationFailed {
        /// The failing migration's version.
        version: String,
        /// The backend driver/transport error from the failed statement.
        #[source]
        source: BackendError,
    },
    /// A guarded migration's existence probe found a shape that diverges from, or
    /// cannot be proven equal to, the declared object. This is a fail-closed drift
    /// error. The backend reads the catalog under its held apply lock and returns
    /// this error before the guarded SQL or completion journal entry runs. It also
    /// carries backend-specific refusals where proving equality is not implemented.
    #[error(
        "existence-guard drift on migration {version}: {object} field `{field}` \
         declared {expected} but the live database has {actual} — the guarded op \
         was refused fail-closed (an `ifNotExists` op never silently runs over, nor \
         skips, a divergent existing object)"
    )]
    ExistenceGuardDrift {
        /// The guarded migration's version.
        version: String,
        /// The diverging object (e.g. `column users.email`).
        object: String,
        /// The attribute that diverged (`data_type`, `nullable`, `kind`, …).
        field: String,
        /// The DECLARED value.
        expected: String,
        /// The LIVE value.
        actual: String,
    },
    /// A migration carried an existence-guard probe for a schema outside the
    /// effective policy scope. Refused before the privileged catalog snapshot,
    /// so a directly constructed probe cannot turn the executor into a
    /// cross-schema reader.
    // The wording carries the REMEDY because the common cause is not an exclusion.
    // The scope is built only from `schema.cross_schema` grant includes
    // (`owned_schemas_from_effective`), so a policy that never grants that key
    // yields `SchemaScope::Single("")` and permits NO schema — including the
    // project's own. "Does not permit" alone sends an operator hunting for an
    // exclusion that was never authored.
    #[error(
        "existence-guard probe on migration {version} names schema {probe_schema:?}, \
         which the effective policy schema scope does not permit. That scope is built \
         only from `schema.cross_schema` grant includes, so a policy that never grants \
         that key permits no schema at all, the project's own included. Grant \
         `schema.cross_schema` with a scope that includes {probe_schema:?}"
    )]
    ExistenceGuardSchemaOutOfScope {
        /// The guarded migration's version.
        version: String,
        /// The schema the forged or stale probe attempted to snapshot.
        probe_schema: String,
    },
    /// An engine-supplied identifier (project schema / migrator role / meta schema)
    /// was not quotable (empty or NUL-bearing) at a render seam — fail-closed
    /// rather than interpolate it. Maps [`crate::render::dml::IdentQuoteError`]; the
    /// meta-schema journal-write seams route the same byte-logic through
    /// [`JournalError`] (which also carries this `From`).
    #[error("apply: {0}")]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
    /// A migration's effective `statement_timeout` / `lock_timeout` budget
    /// resolved to `0`, which the database reads as "no limit", the opposite of
    /// the mandatory finite budget the executor advertises. Refused before the
    /// migration's SQL reaches the server; nothing ran. See
    /// [`crate::apply::timeout`].
    #[error(transparent)]
    IndefiniteTimeout(#[from] crate::apply::timeout::IndefiniteTimeoutError),
}

/// Authorize an existence-guard catalog read against the effective policy scope.
pub(crate) fn authorize_existence_guard_schema(
    cfg: &ExecutorConfig,
    migration: &Migration,
    probe_schema: &str,
) -> Result<(), ApplyError> {
    if cfg
        .guard_config()
        .schema_scope()
        .is_some_and(|scope| scope.permits(probe_schema))
    {
        return Ok(());
    }
    Err(ApplyError::ExistenceGuardSchemaOutOfScope {
        version: migration.version.as_str().to_string(),
        probe_schema: probe_schema.to_string(),
    })
}

#[cfg(pg_seam)]
impl From<crate::driver::DbError> for ApplyError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}

/// Apply the project's pending migrations. Idempotent: a re-run
/// with no new migrations is a no-op.
///
/// `applied_by` is the actor recorded in the journal (`app/actor/AI`).
///
/// `approval` is the caller's approval decision. This is the executor's OWN
/// defense-in-depth approval gate: if any pending migration is
/// flagged [`destructive`](crate::model::migration::MigrationFlags::destructive) and
/// `approval != Approval::Approved`, the apply is refused with
/// [`ApplyError::ApprovalRequired`] before any migration executes — independent
/// of (and additional to) the engine's gate, so a caller driving [`apply`]
/// directly cannot bypass approval. A non-destructive batch runs with
/// [`Approval::None`].
///
/// # Errors
/// - [`ApplyError::ApprovalRequired`] — a destructive migration without approval;
///   aborts before any migration runs.
/// - [`ApplyError::Guard`] — a pending migration's `up` SQL was denied; the
///   whole apply aborts (all-up-front, before any migration runs).
/// - [`ApplyError::NonIdempotentNonTxn`] — a non-transactional migration's `up`
///   is not idempotent (missing `IF NOT EXISTS`); aborts before any run.
/// - [`ApplyError::ChecksumDrift`] — an already-applied migration was tampered.
/// - [`ApplyError::MigrationFailed`] — a migration's SQL failed (rolled back).
/// - [`ApplyError::Db`] / [`ApplyError::Journal`] — infrastructure failures.
#[cfg(pg_seam)]
pub async fn apply<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    // Standalone callers own the lock: acquire it here, release it on exit.
    apply_with_lock(
        conn,
        cfg,
        migrations,
        approval,
        applied_by,
        LockMode::Acquire,
    )
    .await
}

/// [`apply`] with an explicit [`LockMode`].
///
/// Identical to [`apply`] except the caller chooses whether this sub-batch takes
/// the project advisory lock itself ([`LockMode::Acquire`], the standalone case)
/// or whether an OUTER operation already holds it ([`LockMode::AlreadyHeld`], the
/// declarative-deploy sub-batches driven by
/// [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative)).
///
/// `AlreadyHeld` skips ONLY the per-batch acquire/release; the per-batch session
/// hygiene (GUC snapshot/restore + unconditional `RESET ROLE`) still runs, so an
/// inner sub-batch never leaks its `search_path` / timeouts / role onto the
/// session even though the lock is owned outside it.
///
/// # Errors
/// Same as [`apply`].
#[cfg(pg_seam)]
pub async fn apply_with_lock<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
    lock_mode: LockMode,
) -> Result<ApplyOutcome, ApplyError> {
    // Drive the whole apply through the dialect seam. Postgres is the only
    // backend; the public entry is now generic over the `SqlSession` seam (`&D`)
    // so a host (napi) driver can drive it, while the apply body below is generic
    // over `MigrationBackend` (no concrete client reaches `apply_locked`).
    // The defense-in-depth approval gate lives in `apply_with_lock_backend` so it
    // runs IDENTICALLY for both this entry and the generic engine path — a
    // single source of the executor-layer gate.
    //
    // `new_generic` (not `new`) is used here: the apply path never reads the
    // backend's `online`/`shadow` harnesses (those are exercised only via the
    // separate `run_expand`/`dry_run` entries), so the
    // constructed backend is behaviorally identical to a plain `new(conn)` —
    // only the two unused `Option` fields differ (both stay `None`). This is what
    // makes the `&Client → &D` generalization behavior-preserving.
    //
    // This entry keeps the BLANKET scope ([`ApprovalScope::All`]) — its
    // callers (the expand-contract EXPAND apply, the flat `engine.apply` path) carry
    // their own scope check at the engine layer when one is in play. A direct caller
    // of `apply_with_lock` is the trusted single-actor `.sql` surface (no co-bundling
    // of distinct reviewed version-ids), so `All` preserves byte-identical behavior.
    let backend = PostgresBackend::new_generic(conn);
    apply_with_lock_backend(
        &backend,
        cfg,
        migrations,
        approval,
        &crate::approval::ApprovalScope::All,
        applied_by,
        lock_mode,
    )
    .await
}

/// The **MySQL** counterpart of [`apply_with_lock`]: drive the apply through the
/// [`MysqlBackend`], which rides the SAME `driver::SqlSession` seam as Postgres but
/// renders MySQL dialect SQL (`GET_LOCK` project lock, MySQL journal DDL, `?`
/// placeholders, auto-committing two-phase apply). This is the dialect-selection
/// entry: a caller that knows the target is MySQL constructs the MySQL backend
/// here and reuses the identical generic `apply_with_lock_backend` orchestration
/// shell — so the executor holds no dialect SQL and MySQL rides the same seam.
///
/// # Errors
/// Same as [`apply`].
#[cfg(pg_seam)]
pub async fn apply_with_lock_mysql<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
    lock_mode: LockMode,
) -> Result<ApplyOutcome, ApplyError> {
    let backend = MysqlBackend::new_generic(conn);
    apply_with_lock_backend(
        &backend,
        cfg,
        migrations,
        approval,
        &crate::approval::ApprovalScope::All,
        applied_by,
        lock_mode,
    )
    .await
}

/// The lock + session-hygiene shell around [`apply_locked`], generic over the
/// dialect seam. Both [`apply_with_lock`] (PG) and the generic engine declarative
/// path construct/forward their backend and call this; the body is byte-identical to
/// the pre-seam PG flow, now routed through [`MigrationBackend`].
pub(crate) async fn apply_with_lock_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    scope: &crate::approval::ApprovalScope,
    applied_by: &str,
    lock_mode: LockMode,
) -> Result<ApplyOutcome, ApplyError> {
    // Defense-in-depth approval gate — refuse a destructive batch
    // without explicit approval BEFORE doing anything (not even the lock). The
    // engine has its own gate; this is the independent executor-layer check so a
    // direct caller cannot bypass it. It is dialect-agnostic (reads only
    // `flags.destructive`), so it sits in the generic core — running identically for
    // PG and the engine path.
    if approval != Approval::Approved
        && migrations
            .iter()
            .any(|m| m.flags.destructive || m.flags.requires_approval)
    {
        return Err(ApplyError::ApprovalRequired);
    }
    // **Per-version approval scope (anti-bypass), defense in depth.** Even
    // under blanket `Approval::Approved`, a destructive migration runs ONLY if its
    // version-id is admitted by the operator's reviewed scope. Under
    // `ApprovalScope::All` (the default for every existing caller) this is vacuously
    // true. Under `ApprovalScope::Versions`, a
    // co-bundled destructive op the operator did NOT individually review is refused
    // here too, so a direct executor caller cannot bypass the engine-layer scope
    // check. Checked per-element (a coalesced DDL batch carries per-`Migration`
    // versions). Fail-closed: the FIRST un-scoped destructive migration aborts the
    // whole batch before the lock or any DDL.
    if approval == Approval::Approved {
        if let Some(m) = migrations.iter().find(|m| {
            (m.flags.destructive || m.flags.requires_approval) && !scope.admits(m.version.as_str())
        }) {
            return Err(ApplyError::ApprovalNotScoped {
                version: m.version.as_str().to_string(),
            });
        }
    }
    // Acquire the project advisory lock only when WE own it. Under `AlreadyHeld`
    // the outer `apply_declarative` holds it for the whole declarative deploy, so
    // this sub-batch inherits that hold and takes none of its own.
    //
    // The skip is not what keeps a concurrent deploy out. Session advisory locks
    // stack by depth: measured on PostgreSQL 18.4, two acquires followed by ONE
    // unlock leave the lock still held, and only the matching second unlock
    // releases it. A balanced re-acquire and release here would therefore keep the
    // outer hold intact the whole way through. What the skip buys is a round trip
    // per sub-batch, and one fewer place an error path can leave the depth
    // unbalanced. The hold that actually excludes a second deploy is the outer
    // one, acquired once by `apply_declarative`.
    if lock_mode == LockMode::Acquire {
        backend.acquire_project_lock(cfg).await?;
    }
    // Capture the session GUCs we will override so we can restore them on exit
    // — the executor's search_path / statement_timeout / lock_timeout must NOT
    // leak onto the (pooled / long-lived) connection after apply. This runs
    // regardless of lock mode: every sub-batch is responsible for its own session
    // hygiene even when the lock is owned outside it.
    let snapshot = match backend.snapshot_session().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            // A snapshot is the prerequisite for every session mutation below.
            // Continuing without it can leak settings, and on MySQL pinning
            // autocommit could commit caller-owned work. Nothing author-controlled
            // may run after this failure.
            backend.reset_role_best_effort().await;
            if lock_mode == LockMode::Acquire {
                if let Err(unlock) = backend.release_project_lock(cfg).await {
                    tracing::warn!(
                        error = %unlock,
                        "zero-migrate: failed to release project lock after session snapshot error"
                    );
                }
            }
            return Err(error);
        }
    };
    let result = apply_locked(backend, cfg, migrations, applied_by).await;
    // RESET ROLE UNCONDITIONALLY — regardless of whether `snapshot_session`
    // succeeded. The non-txn path's `SET ROLE` mutates the session; if the
    // snapshot had failed we would otherwise skip `restore_session` entirely and
    // leak the migrator role onto the pooled/long-lived connection. So drop the
    // role back to admin on EVERY exit path first, then restore the GUCs if we
    // have a snapshot. (Harmless no-op when no `SET ROLE` ran.)
    backend.reset_role_best_effort().await;
    // Restore before releasing the lock. Preserve an apply failure when cleanup
    // also fails, but never report a successful apply if its session restoration
    // failed: returning that connection to a pool with altered settings is an
    // observable failure of the operation's contract.
    let restored = backend.restore_session(&snapshot).await;
    let result = match (result, restored) {
        (Err(error), Err(restore)) => {
            tracing::warn!(
                error = %restore,
                "zero-migrate: failed to restore session settings after apply error"
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(outcome), Ok(())) => Ok(outcome),
    };
    // Release the lock only when WE acquired it. Under `AlreadyHeld` the outer
    // `apply_declarative` releases it once, after every sub-batch. Always release
    // on the `Acquire` path, even on error. Surface the original error first.
    if lock_mode == LockMode::AlreadyHeld {
        return result;
    }
    let unlock = backend.release_project_lock(cfg).await;
    // Surface the apply error first if there was one; otherwise surface any
    // unlock failure. (The lock auto-releases on session end regardless.)
    match result {
        Ok(o) => unlock.map(|()| o),
        Err(e) => Err(e),
    }
}

/// Pre-flight over the FULL supplied set: reject malformed
/// repeatable/versioned combinations BEFORE the partition or any apply, so a
/// dropped or misrouted facet can never silently apply. Fail-closed per the
/// no-back-compat stance — these shapes are author errors, not legacy inputs.
///
/// Three rejections, each before any execution (nothing applied):
///
/// - a `repeatable=true` migration with a non-empty `supersedes`: a
///   repeatable cannot be a squash ([`ApplyError::RepeatableCannotSquash`]). Without
///   this, the partition routes it into the repeatable phase and its `supersedes` is
///   silently dropped (never gated).
/// - a `repeatable=true` migration with `down.is_some()`: a repeatable is
///   replace-style with no true reverse ([`ApplyError::RepeatableHasDown`]).
/// - a VERSIONED (once-only) migration whose `depends_on` names a
///   REPEATABLE in the same set: a once-only migration may not depend on a
///   repeatable (repeatables run AFTER all versioned migrations), so the dependency
///   can never be ordered ([`ApplyError::OnceOnlyDependsOnRepeatable`]). This is the
///   DEDICATED error, raised before `order_pending` would otherwise produce the
///   misleading `MissingDependency` (the repeatable is partitioned out of the
///   versioned set the ordering sees).
///
/// # Errors
/// One of the three [`ApplyError`] variants above on the first malformed migration
/// found (deterministic order: the rejections are checked in version order).
fn check_repeatable_wellformed(migrations: &[Migration]) -> Result<(), ApplyError> {
    use std::collections::BTreeSet;

    // The set of versions whose SUPPLIED flag marks them repeatable — used by the
    // once-only-depends-on-repeatable check.
    let repeatable_versions: BTreeSet<&str> = migrations
        .iter()
        .filter(|m| m.flags.repeatable)
        .map(|m| m.version.as_str())
        .collect();

    // Deterministic iteration order (version order) so the first-found rejection is
    // stable across runs.
    let mut ordered: Vec<&Migration> = migrations.iter().collect();
    ordered.sort_by(|a, b| a.version.as_str().cmp(b.version.as_str()));

    for m in ordered {
        if m.flags.repeatable {
            // A repeatable cannot be a squash.
            if !m.supersedes.is_empty() {
                return Err(ApplyError::RepeatableCannotSquash {
                    version: m.version.as_str().to_string(),
                });
            }
            // A repeatable must not declare a down.
            if m.down.is_some() {
                return Err(ApplyError::RepeatableHasDown {
                    version: m.version.as_str().to_string(),
                });
            }
        } else {
            // A once-only migration may not depend on a repeatable in the set.
            for dep in &m.depends_on {
                if repeatable_versions.contains(dep.as_str()) {
                    return Err(ApplyError::OnceOnlyDependsOnRepeatable {
                        version: m.version.as_str().to_string(),
                        dependency: dep.as_str().to_string(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// The apply body, run while holding the project advisory lock.
///
/// Generic over the dialect seam ([`MigrationBackend`]): the orchestration here
/// — partition, drift/tamper gate, squash/expand gates, `order_pending`, the
/// FIRST/SECOND pass, the repeatable phase — is dialect-agnostic; every
/// dialect-coupled leaf (journal reads, the checksum-drift report, the confined
/// `up`, the non-txn idempotency parse) goes through `backend`.
async fn apply_locked<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    backend.ensure_journal(cfg).await?;

    // MySQL `down` DDL auto-commits, so an interrupted unwind can leave both an
    // applied journal event and an unverified partially-reverted live shape. The
    // ordinary pending calculation would call that version `skipped`. Refuse it
    // before partitioning whenever its durable marker intersects the FULL set
    // supplied to this apply. PostgreSQL and SQLite inherit the empty hook.
    let supplied_versions: HashSet<&str> = migrations.iter().map(|m| m.version.as_str()).collect();
    if let Some(version) = backend
        .unresolved_rollback_markers(cfg)
        .await?
        .into_iter()
        .find(|version| supplied_versions.contains(version.as_str()))
    {
        return Err(ApplyError::UnresolvedRollbackMarker {
            version,
            meta_schema: cfg.pg.meta_schema.clone(),
        });
    }

    // PRE-FLIGHT over the FULL supplied set, before the
    // partition or any apply, rejecting malformed repeatable/versioned combinations
    // fail-closed (a dropped/misrouted facet must never silently apply). Refusing
    // here, before the partition, means the rejected shapes never reach the
    // versioned pipeline or the repeatable phase.
    check_repeatable_wellformed(migrations)?;

    // Partition the supplied set into VERSIONED (run-once) and
    // REPEATABLE migrations. The entire versioned pipeline below (drift/tamper
    // abort, expand-contract gate, squash gates, pending ordering, execute) sees
    // ONLY the versioned migrations: a repeatable has a stable identity and a
    // changed-checksum-means-re-run rule, so it must NEVER participate in the
    // once-only drift abort, the orphan check, or the squash/expand machinery.
    // Repeatables are applied AFTER all versioned pending migrations, in their own
    // phase (`apply_repeatables`) with their own re-run-on-change rule.
    // The FULL set is retained as `all_migrations` for the drift check, which must
    // SEE the repeatables (so it recognizes their journaled versions and EXEMPTS
    // them — see `check_checksum_drift` — rather than flagging them as orphans).
    //
    // The partition routes by the SUPPLIED `flags.repeatable`, but a version whose
    // supplied flag DISAGREES with its journaled kind (the flip-flag tamper class)
    // is aborted by the kind-mismatch arm of `check_checksum_drift` BELOW — which
    // runs before any re-run — so a mis-routed (flipped) version can never reach the
    // repeatable re-apply phase. The drift check is the single fail-closed gate; the
    // partition does not need to (and must not) silently re-route by the flag.
    let all_migrations = migrations;
    let (versioned, repeatables): (Vec<&Migration>, Vec<&Migration>) =
        migrations.iter().partition(|m| !m.flags.repeatable);
    // Owned slice of the versioned originals so the existing versioned pipeline
    // (which takes `&[Migration]`) is unchanged: the squash / expand-contract /
    // pending machinery sees ONLY versioned migrations. The partition is by
    // reference, so we re-collect the (small) versioned set here. Migrations are
    // cheap value types (a few Strings).
    let versioned_owned: Vec<Migration> = versioned.iter().map(|m| (*m).clone()).collect();
    let migrations: &[Migration] = &versioned_owned;

    // Index the journal by version for the drift check + pending computation.
    let journal_rows: Vec<AppliedEntry> = backend.applied(cfg).await?;
    let mut completed: HashMap<&str, &AppliedEntry> = HashMap::new();
    let mut started: HashMap<&str, &AppliedEntry> = HashMap::new();
    for e in &journal_rows {
        match e.phase {
            Phase::Completed => {
                completed.insert(e.version.as_str(), e);
            }
            Phase::Started => {
                started.insert(e.version.as_str(), e);
            }
        }
    }

    // Drift / tamper check: every migration in the set that
    // the journal records as net-applied must still match its recorded checksum.
    // This is the SHARED comparison — `crate::apply::drift::check_checksum_drift` builds
    // the full report (used read-only by the status/drift API), and apply aborts
    // on the FIRST checksum mismatch it surfaces. One implementation, two callers:
    // the report and the abort-on-drift gate cannot diverge.
    // Pass the FULL set (`all_migrations`): the drift check exempts repeatables
    // (their changed checksum is the re-run signal, not tamper) and must recognize
    // their journaled versions so they are NOT reported as orphans.
    let drift_report = backend
        .check_checksum_drift(cfg, all_migrations)
        .await
        .map_err(|e| match e {
            crate::apply::drift::DriftError::Db(db) => ApplyError::Db(db),
            crate::apply::drift::DriftError::Journal(j) => ApplyError::Journal(j),
            crate::apply::drift::DriftError::Snapshot(s) => ApplyError::Backend(s),
            crate::apply::drift::DriftError::Backend(b) => ApplyError::Backend(b),
        })?;
    if let Some(d) = drift_report.checksum_drift.into_iter().next() {
        return Err(ApplyError::ChecksumDrift {
            version: d.version,
            recorded: d.recorded,
            expected: d.expected,
        });
    }

    // `drift_report.orphan_journal` is deliberately NOT read here. It names every
    // net-applied version absent from `migrations`, and `migrations` is the batch
    // THIS call was handed, not the operator's whole set: the plan path hands one
    // coalesced DDL batch per call, so applying the second of two migrations would
    // name the first -- still present in the operator's directory. No boundary
    // inside the executor knows the difference, so the diagnosis is made where the
    // supplied set can be attested: `require_applied_prefix` for a host deploy,
    // and `status`, whose `unexpected_journal` is computed against the full
    // supplied manifest set and trips `status --strict`.

    // EXPAND/CONTRACT GATE — refuse a pending contract whose expand
    // is not net-applied in the journal. Run BEFORE `order_pending` so it beats
    // the generic MissingDependency with a precise error.
    check_expand_contract_gate(migrations, &completed)?;

    // Supersession: a version superseded by a net-applied squash (read
    // from the journal) OR by an in-set squash that will run this batch is SATISFIED
    // — it must not (re-)run. `compute_superseded` unions both sources; the squash
    // `S` itself is never in this set (it runs / is already applied). Computed
    // BEFORE the all-or-none gate so the gate can classify each squash's superseded
    // set against the SAME satisfied set the pending computation uses.
    let journal_superseded = backend.superseded_versions(cfg).await?;
    let superseded_owned = compute_superseded(migrations, &journal_superseded);

    // SQUASH ALL-OR-NONE GATE — a pending squash may run its `up` only
    // when NONE of its superseded versions are SATISFIED (fresh DB). All-satisfied
    // => use squash() (record without running); partial => inconsistent. A version
    // is satisfied when it is directly net-applied (`completed`) OR covered by a
    // net-applied squash (`journal_superseded`): a chained/overlapping
    // squash over a prefix already covered by an EARLIER net-applied squash (whose
    // members were superseded-not-journaled) was miscounted `applied=0` and re-ran
    // its `up`, double-applying. We classify against `completed ∪ journal_superseded`
    // (net-applied coverage only — NOT in-set pending edges, which would wrongly mark
    // a squash's own targets as satisfied on the genuine fresh path). Refused before
    // any execution, before order_pending hides the superseded versions.
    let satisfied: std::collections::HashSet<&str> = completed
        .keys()
        .copied()
        .chain(journal_superseded.iter().map(String::as_str))
        .collect();
    // Two PENDING in-set squashes superseding the same version is malformed (a
    // version may be collapsed by at most one squash). Neither is net-applied yet,
    // so the all-or-none gate cannot catch it — refuse up-front, fail-closed. An
    // ALREADY-APPLIED squash re-supplied alongside a new one (the legitimate
    // chained case) is excluded — that is handled by the all-or-none gate.
    check_no_overlapping_squashes(migrations, &completed)?;
    check_squash_all_or_none(migrations, &completed, &satisfied)?;
    let superseded: std::collections::HashSet<&str> =
        superseded_owned.iter().map(String::as_str).collect();

    // Pending = set − completed − superseded. Ordered by `depends_on` when present
    // (topological, version-tiebroken & stable), else pure UUIDv7 version order.
    let pending: Vec<&Migration> = order_pending(migrations, &completed, &superseded)?;

    // Multi-engine — run the **per-engine** first-line
    // guard through the [`MigrationGuard`] seam, NOT an `if dialect == Sqlite`
    // branch. The guard is selected for `cfg`'s dialect (which equals
    // `backend.dialect()`) via [`guard_for`], so it carries the apply's project +
    // trust profile from `cfg.guard_config()` (the trust profile lives on
    // `ExecutorConfig`, not the backend):
    //   - Postgres → `PgGuard` (libpg_query deny-list) — byte-identical to the
    //     pre-seam `SqlGuard::new(cfg.guard_config())`;
    //   - SQLite → `SqliteDescriptorGuard` — the trusted descriptor-diff path
    //     (`check` returns the empty clean outcome: `libpg_query` cannot vet SQLite,
    //     the first-line vet is the descriptor emitter at the author boundary and the
    //     second-line defense is the backend authorizer applied per statement at apply).
    // The non-txn idempotency check still runs through the trait (`validate_non_txn`),
    // which for SQLite rejects `transaction:false` at the dialect boundary.
    let guard = crate::guard::guard_for(&cfg.guard_config().for_dialect(backend.dialect()));

    // FIRST PASS — static validation over EVERY pending migration BEFORE any
    // execution. The guard runs per-migration inside the apply loop in the
    // original design, which means an earlier migration could commit before a
    // later one is denied (a half-applied batch). Hoisting the static checks
    // (guard deny-list + non-txn idempotency) up front makes a denial apply
    // NOTHING. (A migration failing at EXECUTION still legitimately leaves the
    // earlier ones applied — standard migration semantics; only the STATIC
    // checks are all-or-nothing.)
    for m in &pending {
        let version = m.version.as_str();
        // GUARD GATE — first-line per engine: PG denies RCE / priv-esc / cross-tenant /
        // file / network; SQLite trusts the descriptor-diff DDL (vetted by the
        // descriptor emitter + the backend authorizer).
        guard.check(&m.up).map_err(|source| ApplyError::Guard {
            version: version.to_string(),
            source,
        })?;
        // The backend's own scan over an `up` taking the two-phase path. Behind the
        // seam: PG denies the shapes it knows break on a second run and admits the
        // rest; SQLite rejects `transaction:false` at the dialect boundary; MySQL
        // accepts everything, because auto-committing DDL puts EVERY MySQL
        // migration on this path and its recovery refuses to replay instead. What
        // an admitted `up` gets is an apply, not a guarantee that crash recovery
        // can replay it - the backend decides that when a marker is actually armed.
        if backend.uses_two_phase_path(m) {
            backend.validate_non_txn(m)?;
        }
    }

    let mut outcome = ApplyOutcome {
        applied: Vec::new(),
        // Report only versions from this supplied batch. The journal can contain
        // completed steps outside `migrations` (for example DML siblings when the
        // plan executor submits a consecutive DDL sub-batch); including the whole
        // journal here duplicates those sibling ids when their own plan arms report
        // them. Iterate the supplied set once so a version that is both completed
        // and superseded is still reported exactly once and in caller order.
        skipped: supplied_skipped_versions(migrations, &completed, &superseded),
        recovered: Vec::new(),
    };

    // SECOND PASS — execute (precondition gate + apply). All static checks have
    // already passed.
    execute_pending(backend, cfg, &pending, &started, applied_by, &mut outcome).await?;

    // REPEATABLE PHASE. Runs AFTER every versioned pending migration
    // has applied (the versioned schema the repeatables' views/functions reference
    // is now present). Each repeatable re-applies iff its checksum differs from the
    // latest journaled `completed` checksum for its identity (or it was never
    // applied); an unchanged checksum is skipped.
    apply_repeatables(backend, cfg, &repeatables, applied_by, &mut outcome).await?;

    Ok(outcome)
}

fn supplied_skipped_versions(
    migrations: &[Migration],
    completed: &HashMap<&str, &AppliedEntry>,
    superseded: &std::collections::HashSet<&str>,
) -> Vec<String> {
    migrations
        .iter()
        .filter(|migration| {
            let version = migration.version.as_str();
            completed.contains_key(version) || superseded.contains(version)
        })
        .map(|migration| migration.version.as_str().to_string())
        .collect()
}

/// The execute pass: for each pending migration, evaluate
/// its preconditions read-only under the advisory lock, then apply
/// the txn / non-txn path. Splits out of [`apply_locked`] so each stays focused.
///
/// Precondition outcomes:
/// - all met => apply normally;
/// - an `OnUnmet::Skip` check unmet => skip this migration (not applied, not
///   journaled — stays pending); a SKIPPED migration's dependents are also
///   skipped this batch (their `up`'s object was never created);
/// - an `OnUnmet::Halt` check unmet, or ANY inevaluable check => fail-closed via
///   [`ApplyError::PreconditionFailed`] (the `?` propagates, aborting the batch
///   with nothing applied for this migration).
///
/// # Errors
/// Propagates [`ApplyError::PreconditionFailed`] (Halt/inevaluable) and any
/// apply-path error ([`ApplyError::MigrationFailed`], journal/db errors).
async fn execute_pending<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    pending: &[&Migration],
    started: &HashMap<&str, &AppliedEntry>,
    applied_by: &str,
    outcome: &mut ApplyOutcome,
) -> Result<(), ApplyError> {
    // Versions SKIPPED this run because an `OnUnmet::Skip` precondition was unmet.
    // A skipped migration is NOT applied and NOT journaled — it stays
    // pending for the next deploy. Its dependents must also not run this batch: a
    // dependent's depended-on object does not exist (the dep did not run), so we
    // transitively skip any pending migration whose `depends_on` includes a
    // skipped version. `pending` is in topological order, so a dependent is always
    // visited after the dep it would skip on.
    let mut skipped_this_run: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for &m in pending {
        let version = m.version.as_str();
        // An inflight marker is the crash-recovery key for a half-run migration, and
        // it records the checksum of the body that half-ran. The tamper gate cannot
        // vet it: `compare_applied_to_set` deliberately skips every non-completed
        // entry, because a lone marker is not a settled state. So this is the only
        // place the marker's identity can be checked before its migration is
        // re-executed, and reducing the entry to a bool skipped that check entirely.
        //
        // Editing a `transaction:false` migration in place after it half-applied
        // would otherwise replay a DIFFERENT body than the one that ran, and then
        // overwrite the marker, destroying the evidence. MySQL already refuses this
        // (`MysqlInflightRecoveryError::MarkerMismatch`); the generic path did not.
        //
        // An empty recorded checksum is a marker predating the field, not a
        // mismatch.
        let inflight = started.get(version).copied();
        if let Some(entry) = inflight {
            if !entry.checksum.is_empty() && entry.checksum != m.checksum.as_str() {
                return Err(ApplyError::ChecksumDrift {
                    version: version.to_string(),
                    recorded: entry.checksum.clone(),
                    expected: m.checksum.as_str().to_string(),
                });
            }
        }
        let had_inflight = inflight.is_some();

        // A dependent of a Skip'd (still-pending) migration cannot run
        // this batch — the object its `up` needs was never created. Transitively
        // skip it (and record it so ITS dependents skip too).
        let dep_skipped = m
            .depends_on
            .iter()
            .any(|d| skipped_this_run.contains(d.as_str()));
        // Evaluate preconditions (read-only) BEFORE the `up`, under the advisory
        // lock so the checked state is stable. Skipped-by-dependency short-circuits
        // evaluation (we already know this won't run).
        //
        // An armed inflight marker outranks every route to a skip. The half-run
        // `up` is itself what stops such a precondition holding - "run this while
        // the table is absent" goes unmet the moment the crashed attempt created
        // it - so the Skip arm fires precisely on the versions that most need an
        // operator. And a skipped version is reported as a clean deploy with
        // nothing applied, so the deploy goes green on every run from then on
        // while the migration never lands: quieter than the Halt arm, and worse.
        // The marker sends the version to `apply_one` instead, which either
        // recovers it or refuses loudly. Preconditions are still evaluated under a
        // marker, so an unmet Halt check aborts from the `?` exactly as it would
        // without one.
        let skip = if had_inflight {
            let _ = backend.evaluate_preconditions(cfg, m).await?;
            false
        } else {
            dep_skipped
                || matches!(
                    backend.evaluate_preconditions(cfg, m).await?,
                    PreconditionVerdict::Skip
                )
        };
        if skip {
            skipped_this_run.insert(version);
            outcome.skipped.push(version.to_string());
            continue;
        }

        // Fresh-DB squash: a squash whose `up` RUNS this batch records its
        // supersession edges so future pending computations know `S` satisfies
        // `[v1..vN]`. The all-or-none gate already proved NONE of the superseded
        // versions were satisfied, so this is the fresh path. The edges are
        // written in the SAME transaction that journals `S`'s `completed` row (not a
        // separate post-commit statement) — a crash between would otherwise leave `S`
        // net-applied with edges missing, re-entering `v1..vN` into pending and
        // re-running them on top of `S`'s schema (double-apply).
        let sups: Vec<&str> = m.supersedes.iter().map(MigrationId::as_str).collect();
        // Versioned once-only path: `'squash'` for a fresh-path squash (non-empty
        // supersedes), else the ordinary `'apply'`. Never `'repeatable'` here — a
        // repeatable never reaches the versioned pipeline (it is partitioned out).
        let kind = if sups.is_empty() { "apply" } else { "squash" };

        // The backend owns the atomicity strategy. On today's PG/SQLite backends,
        // `ddl_is_transactional() == true`, so this routes exactly like the old
        // executor branch: `transactional:false` uses two-phase, everything else
        // uses the atomic apply.
        let recovered = backend
            .apply_one(cfg, m, applied_by, had_inflight, &sups, kind)
            .await?;
        if recovered {
            outcome.recovered.push(version.to_string());
        }
        outcome.applied.push(version.to_string());
    }

    Ok(())
}

/// The REPEATABLE PHASE (Flyway `R__` / Liquibase `runOnChange`).
///
/// Runs AFTER every versioned pending migration has applied (so the schema the
/// repeatables' views/functions/triggers reference exists). For each repeatable,
/// in dependency order ([`order_repeatables`]):
///
/// 1. read the LATEST journaled `completed` checksum for its identity (its stable
///    `version`); if it equals the migration's current checksum ⇒ **SKIP** (no
///    change since the last apply) — appended to `outcome.skipped`;
/// 2. otherwise (never applied, OR checksum DIFFERS) ⇒ **RE-APPLY**: run the SQL
///    guard over `up` (cross-schema / RCE / priv-esc denials — a repeatable's `up`
///    is held to the SAME security bar as a versioned one), evaluate its
///    preconditions read-only under the lock, then run `up` under the
///    least-privilege migrator role inside a transaction and append a NEW
///    `completed` event carrying the new checksum (via [`MigrationBackend::apply_one`],
///    whose transactional leg the repeatable always takes).
///
/// A repeatable is ALWAYS transactional (replace-style `CREATE OR REPLACE …`,
/// `down: None`), so it never takes the non-txn two-phase path. Its `supersedes`
/// is always empty, and the `completed` event is stamped `kind='repeatable'`.
///
/// The destructive/approval gate is enforced uniformly at the top of [`apply`]
/// over the FULL set, so a (rare) destructive repeatable without approval is
/// already refused before the lock — this phase does not need to re-check it.
///
/// # Errors
/// - [`ApplyError::Guard`] — a repeatable's `up` was denied by the SQL guard.
/// - [`ApplyError::MissingDependency`] / [`ApplyError::DependencyCycle`] — the
///   repeatables' `depends_on` edges are unsatisfiable.
/// - [`ApplyError::PreconditionFailed`] — a repeatable's precondition was unmet
///   (Halt) or inevaluable.
/// - [`ApplyError::MigrationFailed`] / journal / db errors from the apply itself.
async fn apply_repeatables<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    repeatables: &[&Migration],
    applied_by: &str,
    outcome: &mut ApplyOutcome,
) -> Result<(), ApplyError> {
    if repeatables.is_empty() {
        return Ok(());
    }

    // The latest journaled `completed` checksum per identity — the re-run oracle.
    let latest = backend.latest_completed_checksums(cfg).await?;

    // Re-read satisfied state after the versioned phase. A supplied versioned
    // migration may have just applied, been covered by a squash, or remained
    // pending because an `OnUnmet::Skip` precondition did not pass. Only durable
    // journal completion or a durable supersession edge satisfies an external
    // repeatable dependency.
    let entries = backend.applied(cfg).await?;
    // Inflight markers left by an interrupted apply. A repeatable rides the same
    // two-phase path as a versioned migration on MySQL, where DDL auto-commits, and
    // the refusal that keeps the engine from replaying possibly-applied CREATE/ALTER
    // statements is driven entirely by this flag. Passing a hardcoded `false` here
    // meant a repeatable interrupted mid-DDL silently replayed its `up` on the next
    // deploy instead of reaching the marker-identity recovery flow.
    let started = entries
        .iter()
        .filter(|entry| matches!(entry.phase, Phase::Started))
        .map(|entry| entry.version.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut satisfied = entries
        .into_iter()
        .filter(|entry| matches!(entry.phase, Phase::Completed))
        .map(|entry| entry.version)
        .collect::<std::collections::HashSet<_>>();
    satisfied.extend(backend.superseded_versions(cfg).await?);

    // Order repeatables among themselves by `depends_on` topo
    // (version-tiebroken), and reject every external dependency that is not
    // durably satisfied.
    let ordered = order_repeatables(repeatables, &satisfied)?;

    // FIRST PASS — guard EVERY repeatable's `up` before any execution, mirroring
    // the versioned all-up-front static gate: a denial applies NOTHING.
    guard_repeatable_batch(cfg, backend.dialect(), &ordered)?;

    // SECOND PASS — re-apply each changed repeatable; skip the unchanged ones.
    for &m in &ordered {
        let version = m.version.as_str();
        let current = m.checksum.as_str();
        // Re-run rule: never applied OR checksum DIFFERS ⇒ re-apply; MATCHES ⇒ skip.
        let unchanged = latest.get(version).is_some_and(|prev| prev == current);
        if unchanged {
            outcome.skipped.push(version.to_string());
            continue;
        }

        // Preconditions: a repeatable may gate its re-apply too. An
        // unmet Skip leaves it unchanged this deploy (re-evaluated next time); an
        // unmet/inevaluable Halt fails closed.
        if matches!(
            backend.evaluate_preconditions(cfg, m).await?,
            PreconditionVerdict::Skip
        ) {
            outcome.skipped.push(version.to_string());
            continue;
        }

        // Replace-style: always transactional on today's PG/SQLite backends, never
        // superseding. `apply_one` runs `up` under the migrator role and appends a
        // fresh `completed` event with the NEW checksum — exactly the re-apply record
        // the next deploy compares against.
        // Stamped `kind='repeatable'`: the journaled kind is the
        // tamper anchor, so the drift exemption can distinguish a genuine repeatable
        // re-run from a flipped once-only, and `latest_completed_checksums` reads only
        // `kind='repeatable'` rows for the re-run oracle.
        backend
            .apply_one(
                cfg,
                m,
                applied_by,
                started.contains(version),
                &[],
                "repeatable",
            )
            .await?;
        outcome.applied.push(version.to_string());
    }

    Ok(())
}

/// Run repeatable DDL through the same dialect-selected line-1 guard as the
/// versioned phase. In particular, MySQL descriptor-generated SQL must not be
/// handed to the PostgreSQL parser merely because it entered the later
/// repeatable phase.
fn guard_repeatable_batch(
    cfg: &ExecutorConfig,
    dialect: crate::schema::query::SqlDialect,
    migrations: &[&Migration],
) -> Result<(), ApplyError> {
    let guard = crate::guard::guard_for(&cfg.guard_config().for_dialect(dialect));
    for migration in migrations {
        guard
            .check(&migration.up)
            .map_err(|source| ApplyError::Guard {
                version: migration.version.as_str().to_string(),
                source,
            })?;
    }
    Ok(())
}

/// Topologically order the repeatables among THEMSELVES, honoring
/// `depends_on` edges between repeatables, version-tiebroken for determinism.
///
/// A repeatable's `depends_on` may name a version outside the current repeatable
/// set only when that version is durably satisfied by a completed journal event or
/// a recorded supersession edge. An edge to another supplied repeatable constrains
/// the topological order. With no inter-repeatable edges this degrades to pure
/// version order.
///
/// # Errors
/// - [`ApplyError::MissingDependency`]: an external dependency is not durably
///   satisfied.
/// - [`ApplyError::DependencyCycle`] — the inter-repeatable edges form a cycle.
fn order_repeatables<'a>(
    repeatables: &[&'a Migration],
    satisfied: &std::collections::HashSet<String>,
) -> Result<Vec<&'a Migration>, ApplyError> {
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    let rep_versions: BTreeSet<&str> = repeatables.iter().map(|m| m.version.as_str()).collect();
    let by_version: HashMap<&str, &Migration> = repeatables
        .iter()
        .map(|m| (m.version.as_str(), *m))
        .collect();

    // in-degree over the repeatable subgraph; adj[dep] = repeatables after `dep`.
    let mut indeg: BTreeMap<&str, usize> = repeatables
        .iter()
        .map(|m| (m.version.as_str(), 0usize))
        .collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in repeatables {
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            if rep_versions.contains(dep_v) {
                adj.entry(dep_v).or_default().push(m.version.as_str());
                *indeg.get_mut(m.version.as_str()).expect("repeatable node") += 1;
            } else if !satisfied.contains(dep_v) {
                return Err(ApplyError::MissingDependency {
                    version: m.version.as_str().to_string(),
                    missing: dep_v.to_string(),
                });
            }
        }
    }

    // Kahn with a version-ordered ready set — deterministic, version-tiebroken.
    let mut ready: BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&v, _)| v)
        .collect();
    let mut ordered: Vec<&Migration> = Vec::with_capacity(repeatables.len());
    while let Some(&v) = ready.iter().next() {
        ready.remove(v);
        ordered.push(by_version[v]);
        if let Some(succs) = adj.get(v) {
            for &s in succs {
                let e = indeg.get_mut(s).expect("successor node");
                *e -= 1;
                if *e == 0 {
                    ready.insert(s);
                }
            }
        }
    }

    if ordered.len() != repeatables.len() {
        let mut cyclic: Vec<&str> = indeg
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&v, _)| v)
            .collect();
        cyclic.sort_unstable();
        return Err(ApplyError::DependencyCycle(cyclic.join(", ")));
    }
    Ok(ordered)
}

/// The verdict of evaluating a migration's preconditions.
///
/// `pub` because it is the return type of
/// [`MigrationBackend::evaluate_preconditions`]
/// — the preconditions seam rides through the (public) trait so the generic
/// apply body never holds a concrete connection. The variants carry no data; a
/// consumer can only match on the apply/skip decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreconditionVerdict {
    /// Every precondition held — apply the migration normally.
    AllMet,
    /// An `OnUnmet::Skip` precondition was unmet — skip this migration this run
    /// (leave it pending, do not journal). The batch continues.
    Skip,
}

/// The per-migration precondition verdict loop now lives in
/// [`crate::apply::precondition::evaluate_all`] — the **Postgres** leaf reached only via
/// [`MigrationBackend::evaluate_preconditions`]
/// (multi-engine abstraction). The generic apply body calls the backend method
/// (`backend.evaluate_preconditions(cfg, m)`); it holds no `&Client` and runs no
/// `pg_query` / `information_schema` query directly.
///
/// The EXPAND/CONTRACT gate. A `phase: Contract` online migration
/// may apply only when every `phase: Expand` migration it `depends_on` is
/// NET-APPLIED (`completed`) in the journal — the single source of truth.
///
/// The contract tears down the dual-write trigger + drops the old column; if it
/// landed before the expand was fully applied + recorded, old/new shapes would
/// stop coexisting and concurrent writes would be lost. Because the gate reads
/// net-applied state from the JOURNAL (not from the in-batch set), the expand and
/// contract partition across SEPARATE deploys for free: deploy N applies+journals
/// the expand; deploy N+1 supplies only the contract and the gate sees the expand
/// net-applied. Conversely, a deploy supplying only a contract whose expand is
/// NOT journaled is refused here with a precise [`ApplyError::ExpandNotApplied`].
///
/// Rule per depended-on version `dep` of a PENDING contract `m`:
/// - `dep` net-applied in the journal               ⇒ OK (the expand is done);
/// - `dep` is an Expand in THIS set, not completed   ⇒ refuse (expand pending);
/// - `dep` absent from this set AND not completed    ⇒ refuse (cross-deploy
///   contract whose expand has not landed).
///
/// A `dep` that is a NON-expand present in the set imposes no expand/contract
/// ordering (the topo sort handles ordinary deps); it is skipped.
///
/// A pending Contract with an EMPTY `depends_on` is malformed and refused
/// fail-closed (it declares no expand to gate on, so it would otherwise pass
/// vacuously).
///
/// # Errors
/// [`ApplyError::ExpandNotApplied`] — a pending contract's expand dependency is
/// not net-applied, or the contract declares no dependency at all.
fn check_expand_contract_gate(
    migrations: &[Migration],
    completed: &HashMap<&str, &AppliedEntry>,
) -> Result<(), ApplyError> {
    use crate::model::migration::OnlinePhase;
    let phase_by_version: HashMap<&str, Option<OnlinePhase>> = migrations
        .iter()
        .map(|m| (m.version.as_str(), m.flags.phase))
        .collect();
    for m in migrations {
        // Only PENDING contract migrations are gated.
        if m.flags.phase != Some(OnlinePhase::Contract)
            || completed.contains_key(m.version.as_str())
        {
            continue;
        }
        // Fail closed: a Contract migration MUST declare the expand it depends on.
        // With an empty `depends_on` the loop below would check nothing and the
        // contract would vacuously pass — dropping a column/trigger with no
        // journaled expand. A contract that declares no expand is malformed.
        if m.depends_on.is_empty() {
            return Err(ApplyError::ExpandNotApplied {
                version: m.version.as_str().to_string(),
                expand: "<none declared: a contract must declare an expand dependency>".to_string(),
            });
        }
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            if completed.contains_key(dep_v) {
                continue; // expand net-applied — OK.
            }
            let dep_is_expand_or_absent = match phase_by_version.get(dep_v) {
                Some(Some(OnlinePhase::Expand)) => true, // expand in set, not done.
                Some(_) => false, // a non-expand dep in the set — not our concern.
                None => true,     // absent from the set AND not completed.
            };
            if dep_is_expand_or_absent {
                return Err(ApplyError::ExpandNotApplied {
                    version: m.version.as_str().to_string(),
                    expand: dep_v.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Order the pending migrations honoring `depends_on` (cross-slice
/// ordering) when set, falling back to pure version order otherwise.
///
/// The default order is UUIDv7 version (time-ordered), but `depends_on` can pull a
/// *higher*-version migration to run **after** a lower-version one it depends on —
/// or, the converse the task calls out: a later-version migration whose
/// `depends_on` is empty may still need to run *after* an earlier-version one
/// because that earlier one depends on **it**. We therefore topologically sort the
/// dependency DAG (edge `dep -> m` for each `dep` in `m.depends_on`), using a
/// version-ordered worklist so the result is **stable** and version-tiebroken
/// (among nodes with no outstanding deps, the lowest version goes first).
///
/// Dependencies already satisfied by the journal (a `completed` version not in the
/// pending set) are treated as pre-met edges — they impose no ordering on the
/// pending batch but must still resolve to a real version (set or journal),
/// otherwise the graph is unsatisfiable.
///
/// # Errors
/// - [`ApplyError::MissingDependency`] — a `depends_on` names a version absent
///   from both the supplied set and the journal.
/// - [`ApplyError::DependencyCycle`] — the pending edges form a cycle.
///
/// `pub(crate)` so the read-only status API ([`crate::ops::status`]) computes its
/// `pending` list in the **exact same topo order** apply uses — there is one
/// pending-ordering implementation, never a re-derived one.
pub(crate) fn order_pending<'a>(
    migrations: &'a [Migration],
    completed: &HashMap<&str, &AppliedEntry>,
    satisfied: &std::collections::HashSet<&str>,
) -> Result<Vec<&'a Migration>, ApplyError> {
    use std::collections::HashSet;

    // The pending set, indexed by version, plus the set of all known versions
    // (pending ∪ completed) for dependency-existence checks.
    //
    // `satisfied` (squash) is the set of versions made redundant by a
    // SUPERSESSION — a version `v_i` superseded by a squash `S` that is net-applied
    // OR being applied in this batch. Such a version is treated like a completed
    // one: it is EXCLUDED from pending (its `up` must never run — `S` covers it),
    // and it counts as a pre-met dependency (a later migration `depends_on v_i` is
    // satisfied by `S`). `S` itself is NOT in `satisfied` (it is pending and runs).
    let pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| {
            !completed.contains_key(m.version.as_str()) && !satisfied.contains(m.version.as_str())
        })
        .collect();
    // Dependency edges to versions already satisfied by the journal/squash are
    // pre-met: they must RESOLVE to a real version (set or journal) but impose no
    // ordering on the batch. The shared Kahn core orders the PENDING subgraph;
    // pre-met deps are supplied as `pre_satisfied`.
    let pre_satisfied: HashSet<&str> = completed
        .keys()
        .copied()
        .chain(satisfied.iter().copied())
        .collect();
    topo_order_version_tiebroken(&pending, &pre_satisfied)
}

/// The SHARED canonical ordering core: a deterministic, **version-tiebroken
/// topological sort** of `nodes` over their `depends_on` edges. Both the apply
/// path ([`order_pending`]) and the integrity manifest ([`canonical_set_order`],
/// folded by [`crate::plan::manifest::compute_manifest`]) order through this one
/// implementation, so the order the manifest blesses can NEVER diverge from the
/// order the executor runs.
///
/// `pre_satisfied` is the set of versions that resolve a dependency WITHOUT being
/// in `nodes` (already net-applied / superseded in the journal). An edge to such a
/// version is pre-met (no ordering constraint) but must still resolve; an edge to
/// a version in neither `nodes` nor `pre_satisfied` is a dangling dependency.
///
/// Kahn with a version-ordered (`BTreeSet`) ready set: among nodes with no
/// remaining unmet dep, the lowest `UUIDv7` version emits first. With no edges this
/// degrades to pure ascending version order.
///
/// # Errors
/// - [`ApplyError::MissingDependency`] — an edge names a version absent from both
///   `nodes` and `pre_satisfied`.
/// - [`ApplyError::DependencyCycle`] — the edges among `nodes` form a cycle.
fn topo_order_version_tiebroken<'a>(
    nodes: &[&'a Migration],
    pre_satisfied: &std::collections::HashSet<&str>,
) -> Result<Vec<&'a Migration>, ApplyError> {
    use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

    let node_versions: HashSet<&str> = nodes.iter().map(|m| m.version.as_str()).collect();

    // adj[dep] = nodes that must run AFTER `dep`; indeg[m] = unmet in-set deps.
    let mut indeg: BTreeMap<&str, usize> =
        nodes.iter().map(|m| (m.version.as_str(), 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in nodes {
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            if !node_versions.contains(dep_v) && !pre_satisfied.contains(dep_v) {
                return Err(ApplyError::MissingDependency {
                    version: m.version.as_str().to_string(),
                    missing: dep_v.to_string(),
                });
            }
            // Only edges to another in-set node constrain order; a pre-satisfied
            // dep imposes none.
            if node_versions.contains(dep_v) {
                adj.entry(dep_v).or_default().push(m.version.as_str());
                *indeg.get_mut(m.version.as_str()).expect("node") += 1;
            }
        }
    }

    let by_version: HashMap<&str, &Migration> =
        nodes.iter().map(|m| (m.version.as_str(), *m)).collect();
    let mut ready: BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&v, _)| v)
        .collect();
    let mut ordered: Vec<&Migration> = Vec::with_capacity(nodes.len());
    while let Some(&v) = ready.iter().next() {
        ready.remove(v);
        ordered.push(by_version[v]);
        if let Some(succs) = adj.get(v) {
            for &s in succs {
                let e = indeg.get_mut(s).expect("successor node");
                *e -= 1;
                if *e == 0 {
                    ready.insert(s);
                }
            }
        }
    }

    if ordered.len() != nodes.len() {
        let mut cyclic: Vec<&str> = indeg
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&v, _)| v)
            .collect();
        cyclic.sort_unstable();
        return Err(ApplyError::DependencyCycle(cyclic.join(", ")));
    }
    Ok(ordered)
}

/// The CANONICAL EXECUTED ORDER of a FULL supplied set, used by
/// [`crate::plan::manifest::compute_manifest`] to fold the manifest over the order the
/// executor will actually run — NOT the cosmetic slice order.
///
/// This is [`topo_order_version_tiebroken`] over the WHOLE set with NO journal
/// context (the control plane stamps the manifest before any apply, over the raw
/// authored set), so it is exactly the order [`order_pending`] produces for an
/// all-pending set. Two consequences the manifest relies on:
///
/// - a pure **slice reorder** of an additive set (no `depends_on`) sorts back to
///   the SAME version order ⇒ the SAME manifest (no false mismatch);
/// - a `depends_on` change that REORDERS execution sorts differently ⇒ a DIFFERENT
///   manifest (also independently caught by the checksum fold).
///
/// On an unorderable set (a `depends_on` cycle, or a dangling dependency that
/// names a version outside the set) there is no executed order — such a set never
/// applies (the executor refuses it at [`order_pending`]). For the manifest we
/// fall back to a DETERMINISTIC ascending-version order so the hash is still a
/// stable function of the set (identical when the control plane stamps and when
/// the engine verifies); the real cycle/dangling error surfaces at apply, not as a
/// manifest mismatch. The manifest's job is integrity of the SET, not graph
/// validation.
#[must_use]
pub(crate) fn canonical_set_order(migrations: &[Migration]) -> Vec<&Migration> {
    let nodes: Vec<&Migration> = migrations.iter().collect();
    let empty = std::collections::HashSet::new();
    topo_order_version_tiebroken(&nodes, &empty).unwrap_or_else(|_| {
        // Unorderable (cycle / dangling dep): deterministic version-sorted fallback.
        let mut v: Vec<&Migration> = migrations.iter().collect();
        v.sort_by(|a, b| a.version.as_str().cmp(b.version.as_str()));
        v
    })
}

/// Compute the set of versions made redundant by a SUPERSESSION (squash),
/// for the supplied set + the journal's net state.
///
/// A version `v_i` is satisfied-by-supersession when a squash `S` (with `v_i ∈
/// S.supersedes`) is either:
/// - **net-applied in the journal** (`journal_superseded`, read via
///   [`crate::apply::journal::superseded_versions`]); or
/// - **present in the supplied set** — whether already net-applied OR pending. A
///   pending `S` will run its `up` THIS batch, so its superseded versions must not
///   also run (`order_pending` excludes them); an already-applied `S` is also
///   covered by `journal_superseded`, so adding the in-set edges is at worst
///   redundant.
///
/// The squash `S` itself is never added to the result (it is not superseded by
/// itself; it runs or is already applied). Used by both [`apply_locked`] and the
/// read-only [`crate::ops::status`] so their "pending" views agree.
pub(crate) fn compute_superseded(
    migrations: &[Migration],
    journal_superseded: &[String],
) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> = journal_superseded.iter().cloned().collect();
    for m in migrations {
        if m.supersedes.is_empty() {
            continue;
        }
        // An in-set squash covers its superseded versions whether it is already
        // net-applied OR will be applied this batch (pending). Either way, the
        // superseded versions must not (re-)run, so they enter the satisfied set.
        for dep in &m.supersedes {
            out.insert(dep.as_str().to_string());
        }
    }
    out
}

/// Refuse a malformed set in which two distinct squashes both supersede the same
/// version. A version may be collapsed by at most one
/// squash; two in-set squashes over an overlapping prefix would both be pending
/// on a fresh DB (neither net-applied → the all-or-none gate sees nothing
/// satisfied and lets both run), so the second's `up` would re-create what the
/// first's already built. Caught here, before any execution — fail-closed on
/// nonsensical authoring rather than erroring mid-batch.
///
/// # Errors
/// - [`ApplyError::OverlappingSquashes`] — two squashes in the set supersede the
///   same version.
fn check_no_overlapping_squashes(
    migrations: &[Migration],
    completed: &HashMap<&str, &AppliedEntry>,
) -> Result<(), ApplyError> {
    // version superseded -> the first PENDING squash version seen superseding it.
    let mut owner: HashMap<&str, &str> = HashMap::new();
    for m in migrations {
        // Only PENDING squashes conflict; an already-net-applied squash re-supplied
        // in the set is settled (its supersession is recorded) — the all-or-none
        // gate routes a new overlapping squash to SquashAlreadyApplied.
        if m.supersedes.is_empty() || completed.contains_key(m.version.as_str()) {
            continue;
        }
        for dep in &m.supersedes {
            let dep_s = dep.as_str();
            if let Some(&prev) = owner.get(dep_s) {
                if prev != m.version.as_str() {
                    return Err(ApplyError::OverlappingSquashes {
                        first: prev.to_string(),
                        second: m.version.as_str().to_string(),
                        shared: dep_s.to_string(),
                    });
                }
            } else {
                owner.insert(dep_s, m.version.as_str());
            }
        }
    }
    Ok(())
}

/// Validate the squash all-or-none rule for every PENDING
/// squash in the set, BEFORE any execution.
///
/// A squash `S` (`supersedes = [v1..vN]`) that is about to RUN its `up` (it is in
/// the set and NOT net-applied) requires that NONE of `[v1..vN]` are SATISFIED —
/// the fresh-DB path, where `S.up` builds the schema and the superseded versions
/// are skipped. If ALL of `[v1..vN]` are satisfied, `S.up` would re-create existing
/// objects (double-apply): the correct path is [`crate::ops::squash`] (record the
/// supersession WITHOUT running `up`), so apply refuses with
/// [`ApplyError::SquashAlreadyApplied`]. A PARTIAL set (some but not all satisfied)
/// is an inconsistent state refused with [`ApplyError::SquashPartialOverlap`].
///
/// `satisfied` is the SAME set the pending computation uses: a version is satisfied
/// when it is directly net-applied (`completed`) OR covered by a net-applied squash
/// (`journal::superseded_versions`): a version covered by an
/// EARLIER net-applied squash was superseded-not-journaled, so it lives only as a
/// supersession edge (in `satisfied`, NOT in `completed`). Counting against
/// `completed` alone miscounted `applied=0` for a chained/overlapping squash and
/// re-ran its `up`, double-applying. Classifying against `satisfied` sees the prefix
/// as already built and routes to [`ApplyError::SquashAlreadyApplied`].
///
/// A squash that is itself already net-applied imposes no rule here (its
/// supersession is settled; `compute_superseded` already covers its versions).
///
/// # Errors
/// - [`ApplyError::SquashAlreadyApplied`] — a pending squash whose superseded set
///   is fully satisfied (use [`crate::ops::squash`] instead of apply).
/// - [`ApplyError::SquashPartialOverlap`] — a pending squash whose superseded set
///   is partially satisfied.
fn check_squash_all_or_none(
    migrations: &[Migration],
    completed: &HashMap<&str, &AppliedEntry>,
    satisfied: &std::collections::HashSet<&str>,
) -> Result<(), ApplyError> {
    for m in migrations {
        if m.supersedes.is_empty() || completed.contains_key(m.version.as_str()) {
            continue; // not a squash, or an already-applied squash (settled).
        }
        let total = m.supersedes.len();
        let applied = m
            .supersedes
            .iter()
            .filter(|d| satisfied.contains(d.as_str()))
            .count();
        if applied == 0 {
            continue; // fresh path: S runs, supersedes skipped — OK.
        }
        if applied == total {
            return Err(ApplyError::SquashAlreadyApplied {
                version: m.version.as_str().to_string(),
            });
        }
        return Err(ApplyError::SquashPartialOverlap {
            version: m.version.as_str().to_string(),
            applied,
            total,
        });
    }
    Ok(())
}

// ===========================================================================
// Rollback - apply `down` SQL in reverse to a target.
//
// NO ORCHESTRATOR SHIPS. These types describe the request and the refusals a
// rollback driver would make, but nothing in this crate consumes a
// `RollbackRequest`, and there is no `MigrationEngine::rollback`. The only
// reachable rollback is the per-migration backend leaf
// `MigrationBackend::rollback_one_transactional`, which appends the `rolled_back`
// event for ONE migration and performs none of the selection-time gating named
// below: it does not refuse an irreversible migration, does not classify a
// non-transactional `down`, does not run the guard over the `down` SQL, and does
// not enforce reverse-topological order.
//
// So a host driving that leaf per migration gets none of these checks. See the
// operations docs, which state the roll-forward stance plainly.
// ===========================================================================

/// How far a rollback should unwind the applied migrations.
///
/// Every variant here is resolved in **apply order** — the journal's `event_seq`
/// — and never by how version strings sort. The distinction is not cosmetic:
/// [`AppliedEntry::event_seq`](crate::apply::journal::AppliedEntry::event_seq)
/// records that `MigrationId::derive` stamps the high bits with an `0xFF` marker
/// and fills the rest from a SHA-256, so derived ids sort in hash order among
/// themselves and above every generated id. Version order carries no authoring
/// or apply order at all, and a target resolved against it would name an
/// arbitrary set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackTarget {
    /// Roll back every net-applied migration applied **strictly after** this one
    /// — i.e. unwind *down to* (and keeping) this version. The target itself is
    /// NOT rolled back. "After" is by apply order; the target's own version
    /// string may sort above or below the migrations that come back.
    ToVersion(MigrationId),
    /// Roll back the `n` most-recently-applied migrations — most recent by apply
    /// order, which is not the same as the `n` highest version strings.
    /// `Steps(0)` is a no-op; `Steps(k)` with `k` ≥ the applied count behaves
    /// like [`RollbackTarget::All`].
    Steps(usize),
    /// Roll back **all** net-applied migrations.
    All,
}

/// A complete rollback request.
///
/// How far to unwind ([`RollbackTarget`]) plus the irreversible-handling
/// [`RollbackOptions`], bundled so a rollback driver can carry one parameter
/// rather than two.
#[derive(Debug, Clone)]
pub struct RollbackRequest {
    /// How far to unwind.
    pub target: RollbackTarget,
    /// Irreversible (`down: None`) handling.
    pub options: RollbackOptions,
}

impl RollbackRequest {
    /// A request for `target` with default (refuse-irreversible) options.
    #[must_use]
    pub fn new(target: RollbackTarget) -> Self {
        Self {
            target,
            options: RollbackOptions::default(),
        }
    }

    /// Set the irreversible-handling options (builder convenience).
    #[must_use]
    pub const fn with_options(mut self, options: RollbackOptions) -> Self {
        self.options = options;
        self
    }
}

/// Options controlling `rollback` over irreversible (`down: None`) migrations.
#[derive(Debug, Clone, Copy, Default)]
pub struct RollbackOptions {
    /// Proceed across a migration with `down: None` (irreversible) by **skipping**
    /// its down step instead of refusing. Off by default: rollback refuses to
    /// cross an irreversible migration and directs the operator to roll-forward
    /// (author a compensating migration). Requires [`backup_acknowledged`] too.
    ///
    /// [`backup_acknowledged`]: RollbackOptions::backup_acknowledged
    pub force: bool,
    /// The operator's acknowledgement that a backup exists. `force` is honored
    /// ONLY when this is also set — forcing past an irreversible step is a
    /// data-loss operation, so it requires both a deliberate force and a backup
    /// acknowledgement.
    pub backup_acknowledged: bool,
}

/// What `rollback` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackOutcome {
    /// Versions whose `down` ran + were journaled `rolled_back`, in the order they
    /// were rolled back: **reverse topological order of `depends_on`** (each
    /// migration's `down` ran before the downs of everything it `depends_on`). This
    /// is the transpose of apply's topo order and degrades to strict
    /// reverse-version order when there are no `depends_on` edges (the
    /// version-aligned case), so it is ≈ reverse apply order.
    pub rolled_back: Vec<String>,
    /// Versions skipped because they are irreversible (`down: None`) and `force`
    /// was given. Empty unless forcing.
    pub skipped_irreversible: Vec<String>,
}

impl RollbackOutcome {
    /// True if nothing was rolled back (and nothing force-skipped).
    #[must_use]
    pub const fn is_noop(&self) -> bool {
        self.rolled_back.is_empty() && self.skipped_irreversible.is_empty()
    }
}

/// Error from `rollback`.
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    /// A database/driver error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[source] BackendError),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// An engine-supplied identifier (migrator role / project schema / meta schema)
    /// was not quotable (empty or NUL-bearing) at a render seam — fail-closed
    /// rather than interpolate it. Maps [`crate::render::dml::IdentQuoteError`].
    #[error("rollback: {0}")]
    IdentQuote(#[from] crate::render::dml::IdentQuoteError),
    /// A migration's effective `statement_timeout` / `lock_timeout` budget
    /// resolved to `0` while rendering the `down` session. Same rule as
    /// [`ApplyError::IndefiniteTimeout`]: a `down` is author SQL that takes locks
    /// too, so it never runs on an unbounded budget. Nothing was rolled back.
    #[error(transparent)]
    IndefiniteTimeout(#[from] crate::apply::timeout::IndefiniteTimeoutError),
    /// A dialect-level backend error whose message is already the intended
    /// operator-facing text. See [`ApplyError::Backend`].
    #[error("backend error: {0}")]
    Backend(String),
    /// Reserved for a future rollback orchestrator that would reject
    /// [`Approval::None`] and require [`Approval::Approved`] before any `down`.
    ///
    /// Current per-migration backend rollback leaves do not accept approval and
    /// never construct this variant.
    #[error(
        "rollback requires Approval::Approved (every down is destructive) but it was not given"
    )]
    ApprovalRequired,
    /// The [`RollbackTarget::ToVersion`] target is not a currently net-applied
    /// migration (never applied, or already rolled back). Nothing was rolled back.
    #[error("rollback target {version} is not currently applied")]
    UnknownTarget {
        /// The requested target version.
        version: String,
    },
    /// A version selected for rollback is not present in the supplied migration
    /// set, so its `down` SQL is unavailable. Nothing was rolled back.
    /// A version selected for rollback sits inside an online rename whose contract
    /// is still outstanding.
    ///
    /// The expand half creates the destination column and both columns stay alive
    /// until the operator resolves the contract. Rolling back a version in that set
    /// drops the destination, and `resolve` then refuses BOTH ways: its
    /// `columns_compatible` check runs ahead of the commit arm and the abort arm
    /// alike, and it needs both columns present. The table would be wedged behind
    /// the interlock with no reachable repair.
    #[error(
        "migration {version} belongs to the outstanding online rename on {table} (contract {plan_version}); \
         rolling it back would drop the destination column and leave that rename unresolvable in both \
         directions. Abort the rename first with the resolve abort protocol, then roll back"
    )]
    PendingContractOutstanding {
        /// The selected version that sits inside the obligation.
        version: String,
        /// The table the rename targets.
        table: String,
        /// The obligation's plan-level identity, the one `status` reports.
        plan_version: String,
    },
    #[error("migration {version} is applied but absent from the supplied set; cannot roll back (its `down` is unavailable)")]
    MissingFromSet {
        /// The applied-but-absent version.
        version: String,
    },
    /// A selected migration's `down` contains a statement that cannot run inside
    /// a transaction block (`CREATE INDEX CONCURRENTLY`, `DROP INDEX
    /// CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `VACUUM`, …), but the rollback
    /// executor has ONLY a transactional down path (each `down` runs inside
    /// `BEGIN … COMMIT` under `SET LOCAL ROLE migrator`). Such a `down` would
    /// otherwise fail LATE inside the transaction with Postgres `25001`
    /// ("cannot run inside a transaction block"), surfacing as a confusing
    /// [`DownFailed`](RollbackError::DownFailed). We detect it up-front (same
    /// classifier apply uses, [`crate::classify()`]) and refuse the WHOLE rollback
    /// before any `down` runs — nothing is rolled back. The safe path is
    /// **roll-forward**: author a compensating migration (its own non-transactional
    /// `up` goes through apply's two-phase non-txn path).
    #[error(
        "migration {version} has a non-transactional `down` ({reason}); the rollback executor only \
         runs each down inside a transaction, so this would fail at execution. Prefer ROLL-FORWARD: \
         author a compensating migration instead of rolling this one back."
    )]
    NonTransactionalDown {
        /// The migration whose `down` is non-transactional.
        version: String,
        /// What specifically cannot run in a transaction.
        reason: String,
    },
    /// The rollback would cross a migration with `down: None` (irreversible) and
    /// neither `force` nor `backup_acknowledged` was given. The default guidance
    /// is **roll-forward**: author a new compensating migration. Nothing was
    /// rolled back (refuse-by-default, before ANY down runs).
    #[error(
        "migration {version} ('{name}') is irreversible (down: None); rollback refuses by default. \
         Prefer ROLL-FORWARD: author a compensating migration. To override, pass \
         RollbackOptions {{ force: true, backup_acknowledged: true }} (skips the irreversible step; data loss)."
    )]
    Irreversible {
        /// The irreversible migration's version.
        version: String,
        /// Its human-readable name.
        name: String,
    },
    /// A recorded inverse reached rollback in a shape the atomic inverse executor
    /// cannot safely run. The current contract is deliberately narrow:
    /// transactional parameterized DML only. Refused during the all-up-front
    /// planning pass, before an earlier `down` can commit.
    #[error(
        "migration {version} has a recorded inverse that rollback cannot execute safely: {reason}"
    )]
    RecordedInverseUnsupported {
        /// The forward journal identity being unwound.
        version: String,
        /// The unsupported step/transactionality detail.
        reason: String,
    },
    /// The rollback would `force`-SKIP an irreversible SQUASH. Skipping any other
    /// irreversible migration only forgoes its own undo; skipping a squash leaves
    /// its supersession standing over versions this same rollback then unwinds, so
    /// the journal ends up claiming they are covered by a squash while none of
    /// them are present. Nothing was rolled back (refused before ANY down runs).
    ///
    /// `superseded_versions` only honours edges of a NET-APPLIED squash precisely
    /// so that rolling the squash back releases its supersession. A skip keeps the
    /// squash net-applied, which is the one way to get the covered-but-absent
    /// state that release was designed to prevent.
    #[error(
        "migration {version} ('{name}') is an irreversible SQUASH superseding {superseded} \
         version(s); rollback will not skip it, because skipping leaves its supersession \
         standing over versions this rollback unwinds - they would be journaled as covered \
         by a squash while none of them are present, and a later apply would skip them and \
         report success having created nothing. Prefer ROLL-FORWARD: author a compensating \
         migration. To unwind the squash itself, give it a `down` that reverses the prefix \
         it collapsed."
    )]
    IrreversibleSquash {
        /// The squash migration's version.
        version: String,
        /// Its human-readable name.
        name: String,
        /// How many versions it supersedes.
        superseded: usize,
    },
    /// The SQL guard denied a `down`'s SQL — the down is SQL too and goes through
    /// the SAME defenses as an up. The whole rollback aborts before
    /// any down runs (all-up-front, mirroring apply).
    #[error("rollback of {version} denied by guard: {source}")]
    Guard {
        /// The denied migration's version.
        version: String,
        /// The underlying guard rejection.
        #[source]
        source: GuardError,
    },
    /// An already-applied migration's recorded checksum no longer matches the
    /// migration in the set — drift / tamper. Hard abort (mirrors apply).
    ///
    /// Deliberately NOT worded like [`ApplyError::ChecksumDrift`]. That variant
    /// points at a possibly-stranded inflight marker, because apply can reach it
    /// while the recorded side is a marker recording a body that half-ran.
    /// Rollback compares only against a record it already selected as applied, so
    /// `recorded` here is always a completed event and no marker is involved. Do
    /// not copy the apply wording across: the marker pointer would be false here.
    #[error(
        "checksum drift on {version}: journal has {recorded}, set has {expected}. \
         The applied migration and the one now in the set are not the same migration, \
         so its `down` is not the reverse of what ran: restore the reviewed migration \
         source unedited instead of editing it to match the journal"
    )]
    ChecksumDrift {
        /// The drifting migration's version.
        version: String,
        /// The checksum recorded in the journal.
        recorded: String,
        /// The checksum of the migration now in the set.
        expected: String,
    },
    /// Running a migration's `down` SQL failed (after the guard passed). The
    /// transaction (txn path) was rolled back; no `rolled_back` event was written.
    #[error("rollback of {version} failed: {source}")]
    DownFailed {
        /// The failing migration's version.
        version: String,
        /// The backend driver/transport error from the failed `down`.
        #[source]
        source: BackendError,
    },
    /// The `depends_on` edges among the versions selected for rollback form a
    /// cycle, so no reverse-topological order exists. This should be impossible if
    /// apply enforced acyclicity, but rollback defends anyway (fail-closed before
    /// any down runs). Nothing was rolled back.
    #[error("rollback dependency cycle among selected migrations: {0}")]
    DependencyCycle(String),
    /// A force-skipped irreversible (`down: None`) migration `kept` `depends_on`
    /// `dependency` (directly or transitively), but `dependency` is selected for
    /// ACTUAL rollback. Tearing `dependency` down would leave `kept` (still applied,
    /// because its down never runs) referencing a dropped object — a dangling FK or
    /// a mid-batch `DownFailed`. Refused even under `force`+`backup_acknowledged`;
    /// nothing was rolled back. Roll-forward instead (author a compensating
    /// migration).
    #[error(
        "cannot force-skip irreversible migration {kept} while rolling back {dependency}: \
         {kept} depends_on {dependency} (directly or transitively), so the kept migration would \
         be left referencing a torn-down object. Roll-forward instead (author a compensating migration)."
    )]
    ForceSkipDependencyConflict {
        /// The irreversible migration being force-skipped (kept applied).
        kept: String,
        /// The depended-on version that would be rolled back beneath it.
        dependency: String,
    },
    /// A net-applied migration `kept` is BELOW the rollback's version threshold
    /// (a [`RollbackTarget::ToVersion`]/[`RollbackTarget::Steps`] cut), so it is
    /// kept applied — but it `depends_on` `dependency` (directly or transitively),
    /// and `dependency` IS selected for rollback (above the cut). Tearing
    /// `dependency` down would leave `kept` referencing a dropped object — a
    /// dangling FK or a mid-batch `DownFailed`. This is the same hazard as
    /// [`RollbackError::ForceSkipDependencyConflict`] reached via the
    /// version-threshold keep-path instead of force-skip. Refused before any
    /// `down` runs; nothing was rolled back. Roll-forward instead (author a
    /// compensating migration), or roll back far enough to include `kept`.
    #[error(
        "cannot roll back {dependency} while keeping {kept}: {kept} is below the rollback \
         threshold (kept applied) but depends_on {dependency} (directly or transitively), so it \
         would be left referencing a torn-down object. Roll back far enough to include {kept}, or \
         roll-forward instead (author a compensating migration)."
    )]
    KeptDependsOnRolledBack {
        /// The below-threshold net-applied migration that is kept.
        kept: String,
        /// The selected-for-rollback version it depends on.
        dependency: String,
    },
    /// **SQLite, additive-only.** The migration's `down` requires the 12-step
    /// table REBUILD to reverse (a column TYPE-change reversal, a constraint
    /// add/drop, or any `ALTER` SQLite cannot perform natively). Only
    /// the ADDITIVE reversals SQLite ≥ 3.35 supports natively are implemented —
    /// `DROP TABLE` / `DROP COLUMN` / `DROP INDEX` / `RENAME`. A rebuild-needing
    /// `down` is REFUSED here (not half-rebuilt): the rebuild path is not built.
    /// Nothing was rolled back.
    #[error(
        "migration {version} has a SQLite `down` requiring the 12-step table rebuild ({reason}); \
         the rebuild path is not implemented. Rollback reverses only the operations SQLite \
         supports natively (DROP TABLE/COLUMN/INDEX, RENAME). Author a compensating migration \
         instead."
    )]
    SqliteRebuildRequired {
        /// The migration whose `down` needs a table rebuild.
        version: String,
        /// What specifically requires the rebuild.
        reason: String,
    },
}

#[cfg(pg_seam)]
impl From<crate::driver::DbError> for RollbackError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use crate::apply::journal::{self, Phase};
    use crate::model::migration::{Checksum, MigrationFlags, MigrationId};
    use std::collections::HashMap;

    fn m(version: MigrationId, depends_on: Vec<MigrationId>) -> Migration {
        let up = format!("CREATE TABLE t_{}()", version.as_str());
        let flags = MigrationFlags::default();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: &up,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &depends_on,
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up,
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on,
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
            effect: None,
        }
    }

    fn pos(ordered: &[&Migration], v: &str) -> usize {
        ordered
            .iter()
            .position(|x| x.version.as_str() == v)
            .expect("present")
    }

    #[test]
    fn skipped_versions_are_limited_to_the_supplied_batch() {
        let a = MigrationId::generate();
        let b = MigrationId::generate();
        let unrelated = MigrationId::generate();
        let supplied = vec![m(a.clone(), vec![]), m(b.clone(), vec![])];
        let entries = [
            AppliedEntry {
                down: None,
                version: a.as_str().to_string(),
                checksum: "checksum-a".into(),
                phase: Phase::Completed,
                kind: None,
                event_seq: 0,
            },
            AppliedEntry {
                down: None,
                version: unrelated.as_str().to_string(),
                checksum: "checksum-unrelated".into(),
                phase: Phase::Completed,
                kind: None,
                event_seq: 0,
            },
        ];
        let completed = HashMap::from([
            (entries[0].version.as_str(), &entries[0]),
            (entries[1].version.as_str(), &entries[1]),
        ]);
        let superseded = std::collections::HashSet::from([a.as_str(), b.as_str()]);

        assert_eq!(
            supplied_skipped_versions(&supplied, &completed, &superseded),
            vec![a.as_str().to_string(), b.as_str().to_string()]
        );
    }

    #[test]
    fn no_depends_on_is_pure_version_order() {
        // Three migrations, no edges: result is strict ascending version order.
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = MigrationId::generate();
        let set = vec![
            m(c.clone(), vec![]),
            m(a.clone(), vec![]),
            m(b.clone(), vec![]),
        ];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let ordered =
            order_pending(&set, &completed, &std::collections::HashSet::new()).expect("order");
        let vs: Vec<&str> = ordered.iter().map(|x| x.version.as_str()).collect();
        assert_eq!(vs, vec![a.as_str(), b.as_str(), c.as_str()]);
    }

    #[test]
    fn later_version_runs_before_earlier_when_earlier_depends_on_it() {
        // The task's case: the EARLIER-version migration depends on the
        // LATER-version one, so topo order must run the later one FIRST, inverting
        // pure version order.
        let earlier = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let later = MigrationId::generate();
        assert!(
            later.as_str() > earlier.as_str(),
            "later must sort after earlier"
        );
        // earlier depends_on later; later depends_on nothing.
        let set = vec![
            m(earlier.clone(), vec![later.clone()]),
            m(later.clone(), vec![]),
        ];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let ordered =
            order_pending(&set, &completed, &std::collections::HashSet::new()).expect("order");
        assert!(
            pos(&ordered, later.as_str()) < pos(&ordered, earlier.as_str()),
            "the depended-on (later-version) migration must run first"
        );
    }

    #[test]
    fn cycle_is_a_clear_error() {
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        // a -> b -> a
        let set = vec![m(a.clone(), vec![b.clone()]), m(b.clone(), vec![a.clone()])];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let err = order_pending(&set, &completed, &std::collections::HashSet::new()).unwrap_err();
        match err {
            ApplyError::DependencyCycle(members) => {
                assert!(members.contains(a.as_str()) && members.contains(b.as_str()));
            }
            other => panic!("expected DependencyCycle, got {other:?}"),
        }
    }

    #[test]
    fn missing_dependency_is_an_error() {
        let a = MigrationId::generate();
        let ghost = MigrationId::generate();
        let set = vec![m(a, vec![ghost])];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let err = order_pending(&set, &completed, &std::collections::HashSet::new()).unwrap_err();
        assert!(
            matches!(err, ApplyError::MissingDependency { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn dependency_already_in_journal_is_pre_satisfied() {
        // A dep that is already completed (in the journal, not in the pending set)
        // resolves fine and imposes no batch ordering.
        let done = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let pend = MigrationId::generate();
        let entry = AppliedEntry {
            down: None,
            version: done.as_str().to_string(),
            checksum: String::new(),
            phase: Phase::Completed,
            kind: Some(journal::JournaledKind::Apply),
            event_seq: 0,
        };
        let mut completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        completed.insert(done.as_str(), &entry);
        // Only `pend` is in the supplied set (depends on the completed `done`).
        let set = vec![m(pend.clone(), vec![done.clone()])];
        let ordered =
            order_pending(&set, &completed, &std::collections::HashSet::new()).expect("order");
        let vs: Vec<&str> = ordered.iter().map(|x| x.version.as_str()).collect();
        assert_eq!(vs, vec![pend.as_str()], "only the pending one is ordered");
    }

    #[test]
    fn mysql_repeatable_uses_the_mysql_descriptor_guard() {
        let mut repeatable = m(MigrationId::generate(), Vec::new());
        repeatable.up =
            "CREATE OR REPLACE VIEW `project_acme`.`active_users` AS SELECT `id` FROM `project_acme`.`users`"
                .to_string();
        repeatable.flags.repeatable = true;
        repeatable.down = None;
        let cfg = ExecutorConfig::new(
            "project_acme",
            "project_acme",
            crate::test_fixtures::no_inject("project_acme"),
        );

        guard_repeatable_batch(
            &cfg,
            crate::schema::query::SqlDialect::Mysql,
            &[&repeatable],
        )
        .expect("descriptor-generated MySQL repeatable DDL bypasses the PostgreSQL parser");
    }

    #[test]
    fn repeatable_with_unknown_dependency_is_rejected() {
        let ghost = MigrationId::generate();
        let mut repeatable = m(MigrationId::generate(), vec![ghost.clone()]);
        repeatable.flags.repeatable = true;
        repeatable.down = None;

        let error =
            order_repeatables(&[&repeatable], &std::collections::HashSet::new()).unwrap_err();
        assert!(
            matches!(
                error,
                ApplyError::MissingDependency { ref missing, .. }
                    if missing == ghost.as_str()
            ),
            "got {error:?}"
        );
    }

    #[test]
    fn repeatable_accepts_a_durably_satisfied_external_dependency() {
        let dependency = MigrationId::generate();
        let mut repeatable = m(MigrationId::generate(), vec![dependency.clone()]);
        repeatable.flags.repeatable = true;
        repeatable.down = None;
        let satisfied = std::collections::HashSet::from([dependency.as_str().to_string()]);

        let ordered = order_repeatables(&[&repeatable], &satisfied).expect("dependency is met");
        assert_eq!(ordered[0].version, repeatable.version);
    }
}

// ===========================================================================
// Rollback selection - decide and refuse BEFORE anything executes.
// ===========================================================================

/// The ordered work a [`RollbackRequest`] resolves to, once every gate has passed.
///
/// Produced by [`plan_rollback`], which is TOTAL over its refusals: every reason a
/// rollback can be refused is decided here, before a single `down` runs. A rollback
/// that gets three migrations deep and then discovers the fourth is irreversible
/// leaves a worse state than one that never started, so selection is all-or-nothing
/// exactly like apply's static first pass.
#[derive(Debug)]
pub struct RollbackPlan<'a> {
    /// The `down` steps to run, in reverse topological order of `depends_on`.
    pub steps: Vec<&'a Migration>,
    /// Irreversible versions the operator explicitly forced past.
    pub skipped_irreversible: Vec<String>,
}

/// One net-applied migration as rollback selection needs to see it.
///
/// Selection needs three things the journal has and a bare version string does not:
/// which migrations are net-applied, the order they were applied in, and the
/// checksum each was applied under.
///
/// `event_seq` is the journal's monotonic append counter and is the ONLY sound
/// apply order. Version order is not: `MigrationId::derive` stamps the high 48 bits
/// with an `0xFF` marker and fills the rest from a SHA-256, so every IR-authored
/// step id sorts above every `UUIDv7` file id and derived ids sort among themselves
/// in hash order. Sorting version strings would make `Steps(1)` pick an arbitrary
/// step of an IR plan rather than the last thing applied, and would make
/// `ToVersion` on a file id select every IR migration ever applied. The apply path
/// avoids this by chaining explicit `depends_on`; selection has to read the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRecord {
    /// The migration version.
    pub version: String,
    /// The checksum recorded when it was applied.
    pub checksum: String,
    /// The journal's monotonic sequence number for that apply event.
    pub event_seq: i64,
}

/// Resolve a [`RollbackTarget`] to the net-applied records it unwinds, newest first
/// by journal sequence.
fn select_rollback_versions<'a>(
    target: &RollbackTarget,
    applied: &'a [AppliedRecord],
) -> Result<Vec<&'a AppliedRecord>, RollbackError> {
    let mut newest_first: Vec<&AppliedRecord> = applied.iter().collect();
    newest_first.sort_by(|a, b| {
        b.event_seq
            .cmp(&a.event_seq)
            .then_with(|| b.version.cmp(&a.version))
    });

    Ok(match target {
        RollbackTarget::All => newest_first,
        RollbackTarget::Steps(n) => newest_first.into_iter().take(*n).collect(),
        RollbackTarget::ToVersion(v) => {
            let target_v = v.as_str();
            let Some(anchor) = applied.iter().find(|a| a.version == target_v) else {
                return Err(RollbackError::UnknownTarget {
                    version: target_v.to_string(),
                });
            };
            // Strictly after, by APPLY order: the anchor itself is KEPT.
            newest_first
                .into_iter()
                .filter(|a| a.event_seq > anchor.event_seq)
                .collect()
        }
    })
}

/// Plan a rollback: select, gate, and order. Runs NO SQL.
///
/// Every refusal in [`RollbackError`] that is a selection-time decision is made here.
/// The gates run in a deliberate order - cheapest and most categorical first, so an
/// operator sees the most actionable message rather than the first one a scan
/// happens to hit:
///
/// 1. approval, which is unconditional (every `down` tears structure down);
/// 2. target resolution, so an unknown target fails before anything else is read;
/// 3. availability of each selected migration's `down` in the supplied set;
/// 4. checksum agreement between the journal and the supplied migration;
/// 5. reversibility, then transactionality, then the guard over the `down` SQL;
/// 6. dependency coherence for everything left applied;
/// 7. ordering, which can still surface a cycle.
///
/// # Errors
/// Any [`RollbackError`] selection variant. Never a `DownFailed` - nothing ran.
pub fn plan_rollback<'a>(
    request: &RollbackRequest,
    migrations: &'a [Migration],
    applied: &[AppliedRecord],
    outstanding: &[crate::apply::journal::PendingContract],
    non_txn_downs: &std::collections::BTreeMap<String, String>,
    approval: Approval,
    guard: &dyn crate::guard::MigrationGuard,
) -> Result<RollbackPlan<'a>, RollbackError> {
    plan_rollback_with_inverse_plans(
        request,
        migrations,
        &std::collections::BTreeMap::new(),
        applied,
        outstanding,
        non_txn_downs,
        approval,
        guard,
    )
}

/// The structured-inverse peer of [`plan_rollback`].
///
/// `inverse_plans` is keyed by the FORWARD journal version. Its values retain
/// parameterized DML templates and native bind values; they never become textual
/// `Migration.down` strings. Selection, checksum, dependency, approval and
/// ordering gates remain identical to ordinary rollback.
///
/// # Errors
/// As [`plan_rollback`], plus [`RollbackError::RecordedInverseUnsupported`] when
/// a recorded inverse contains anything other than transactional DML.
pub fn plan_rollback_with_inverse_plans<'a>(
    request: &RollbackRequest,
    migrations: &'a [Migration],
    inverse_plans: &std::collections::BTreeMap<String, AppliedPlan>,
    applied: &[AppliedRecord],
    outstanding: &[crate::apply::journal::PendingContract],
    non_txn_downs: &std::collections::BTreeMap<String, String>,
    approval: Approval,
    guard: &dyn crate::guard::MigrationGuard,
) -> Result<RollbackPlan<'a>, RollbackError> {
    // (1) Approval. A `down` is destructive by construction, so this is not a
    //     per-migration flag question: rollback always requires it.
    if !matches!(approval, Approval::Approved) {
        return Err(RollbackError::ApprovalRequired);
    }

    // (2) Which versions does the target name?
    let selected = select_rollback_versions(&request.target, applied)?;
    if selected.is_empty() {
        return Ok(RollbackPlan {
            steps: Vec::new(),
            skipped_irreversible: Vec::new(),
        });
    }
    let selected_set: std::collections::HashSet<&str> =
        selected.iter().map(|r| r.version.as_str()).collect();

    let by_version: HashMap<&str, &Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    // (3) Every selected version must be in the supplied set: the `down` lives in
    //     the migration file, not the journal, so an absent file means the reverse
    //     SQL simply does not exist to run.
    // (4) And it must be the SAME migration the journal applied. Without this,
    //     rollback launders the drift gate: edit an applied migration's `up` and
    //     `down`, roll it back so the edited `down` runs, and the `rolled_back`
    //     event returns the version to pending - where the next apply runs the
    //     edited `up` with no drift abort, because drift only compares versions
    //     whose latest event is `applied`.
    let mut chosen: Vec<&Migration> = Vec::with_capacity(selected.len());
    for record in &selected {
        let Some(m) = by_version.get(record.version.as_str()) else {
            return Err(RollbackError::MissingFromSet {
                version: record.version.clone(),
            });
        };
        if !record.checksum.is_empty() && record.checksum != m.checksum.as_str() {
            return Err(RollbackError::ChecksumDrift {
                version: record.version.clone(),
                recorded: record.checksum.clone(),
                expected: m.checksum.as_str().to_string(),
            });
        }
        chosen.push(m);
    }

    // (4)-(6) Per-migration gates, plus the forced-skip set.
    let mut skipped_irreversible: Vec<String> = Vec::new();
    let mut steps: Vec<&Migration> = Vec::with_capacity(chosen.len());
    for m in &chosen {
        let version = m.version.as_str();
        let inverse = inverse_plans.get(version);

        // (5a) Reversible? `force` alone is not enough - forcing past an
        //      irreversible step discards data, so it also takes an explicit backup
        //      acknowledgement.
        let down = m.down.as_deref();
        if down.is_none() && inverse.is_none() {
            // A SQUASH is never skippable. For any other migration a skip only
            // forgoes that migration's own undo; for a squash it leaves the
            // supersession standing over versions this same rollback unwinds, and
            // `superseded_versions` honours a net-applied squash's edges — so the
            // covered versions are journaled as satisfied while none of them are
            // present, and a later apply skips them and reports success having
            // created nothing. Refused ahead of the force check: this is not a
            // data-loss trade the operator can acknowledge their way past, because
            // the damage is to what the journal MEANS, not to any one table.
            if !m.supersedes.is_empty() {
                return Err(RollbackError::IrreversibleSquash {
                    version: version.to_string(),
                    name: m.name.clone(),
                    superseded: m.supersedes.len(),
                });
            }
            if request.options.force && request.options.backup_acknowledged {
                skipped_irreversible.push(version.to_string());
                continue;
            }
            return Err(RollbackError::Irreversible {
                version: version.to_string(),
                name: m.name.clone(),
            });
        }

        if let Some(inverse) = inverse {
            for (index, step) in inverse.steps.iter().enumerate() {
                match step {
                    PlanStep::Dml {
                        transactional: true,
                        ..
                    } => {}
                    PlanStep::Dml {
                        transactional: false,
                        ..
                    } => {
                        return Err(RollbackError::RecordedInverseUnsupported {
                            version: version.to_string(),
                            reason: format!(
                                "inverse step {} declares transaction:false; rollback must commit the complete inverse and rolled_back journal event atomically",
                                index + 1
                            ),
                        });
                    }
                    other => {
                        return Err(RollbackError::RecordedInverseUnsupported {
                            version: version.to_string(),
                            reason: format!(
                                "inverse step {} lowers to {}; recorded inverse rollback currently supports transactional DML only",
                                index + 1,
                                rollback_inverse_step_kind(other)
                            ),
                        });
                    }
                }
            }
        }

        // (5b) Transactional? The executor runs each `down` inside a transaction, so
        //      a migration marked non-transactional would fail at execution. Refuse
        //      now, with the roll-forward alternative, rather than there.
        if !m.flags.transactional {
            return Err(RollbackError::NonTransactionalDown {
                version: version.to_string(),
                reason: "the migration declares transaction:false".to_string(),
            });
        }
        //      The flag above is what the author DECLARED. This is what the dialect
        //      found in the reverse SQL itself, which catches the migration that
        //      declares `transaction: true` and then reverses itself with a statement
        //      the server will not run inside a transaction block.
        if inverse.is_none() {
            if let Some(reason) = non_txn_downs.get(version) {
                return Err(RollbackError::NonTransactionalDown {
                    version: version.to_string(),
                    reason: reason.clone(),
                });
            }
        }

        // (5c) A textual `down` is author-supplied SQL and gets the line-1 guard.
        // A structured inverse was already validated and guard-lowered by the same
        // IrAuthor as its forward plan; its DML values remain native binds.
        if let (None, Some(down)) = (inverse, down) {
            guard.check(down).map_err(|source| RollbackError::Guard {
                version: version.to_string(),
                source,
            })?;
        }

        steps.push(m);
    }

    // (6) Dependency coherence. Anything that stays applied must not depend on
    //     anything being torn down, or it is left referencing an object that no
    //     longer exists. This covers both shapes the error surface names: a kept
    //     migration below the threshold, and a migration force-skipped as
    //     irreversible while its dependency is rolled back.
    let rolled_back_set: std::collections::HashSet<&str> =
        steps.iter().map(|m| m.version.as_str()).collect();
    let forced_skips: std::collections::HashSet<&str> =
        skipped_irreversible.iter().map(String::as_str).collect();
    for kept_record in applied {
        let kept = kept_record.version.as_str();
        if rolled_back_set.contains(kept) {
            continue;
        }
        let Some(kept_m) = by_version.get(kept) else {
            // Applied but not supplied, and not being rolled back: it imposes no
            // constraint we can see, and its own `down` is not needed.
            continue;
        };
        for dep in &kept_m.depends_on {
            let dep_v = dep.as_str();
            if !rolled_back_set.contains(dep_v) {
                continue;
            }
            if forced_skips.contains(kept) {
                return Err(RollbackError::ForceSkipDependencyConflict {
                    kept: kept.to_string(),
                    dependency: dep_v.to_string(),
                });
            }
            return Err(RollbackError::KeptDependsOnRolledBack {
                kept: kept.to_string(),
                dependency: dep_v.to_string(),
            });
        }
    }

    // (7) Outstanding rename obligations. An online rename creates its destination
    //     column in the expand half and keeps BOTH columns alive until the operator
    //     resolves the contract. Rolling back a version inside that set drops the
    //     destination, after which the obligation cannot be discharged either way:
    //     `resolve` evaluates `columns_compatible` ahead of its commit arm and its
    //     abort arm alike, and that check needs both columns present. The table would
    //     sit wedged behind the interlock with no reachable repair, so the unwind is
    //     refused here and the operator is sent to the protocol that owns this
    //     transition.
    //
    //     This reads `steps`, not the raw selection: a version force-skipped as
    //     irreversible above is not being rolled back and must not trip the gate.
    //     It matches on the obligation identities a rollback can actually see - the
    //     plan version and the contract versions - never the deep `pending_version`,
    //     which no plan-level supplied set exposes.
    for m in &steps {
        let version = m.version.as_str();
        if let Some(contract) = outstanding.iter().find(|contract| {
            contract.plan_version == version
                || contract
                    .contract_versions
                    .iter()
                    .any(|held| held == version)
        }) {
            return Err(RollbackError::PendingContractOutstanding {
                version: version.to_string(),
                table: contract.table.clone(),
                plan_version: contract.plan_version.clone(),
            });
        }
    }

    // (8) Reverse topological order: the transpose of apply's order. Every selected
    //     dependency is in-set, so nothing is pre-satisfied; reversing an
    //     ascending-version tiebreak yields the documented reverse-version
    //     degradation when there are no edges.
    let pre_satisfied: std::collections::HashSet<&str> = selected_set
        .iter()
        .copied()
        .chain(applied.iter().map(|r| r.version.as_str()))
        .collect();
    let mut ordered =
        topo_order_version_tiebroken(&steps, &pre_satisfied).map_err(|e| match e {
            ApplyError::DependencyCycle(c) => RollbackError::DependencyCycle(c),
            other => RollbackError::Backend(other.to_string()),
        })?;
    ordered.reverse();

    Ok(RollbackPlan {
        steps: ordered,
        skipped_irreversible,
    })
}

fn rollback_inverse_step_kind(step: &PlanStep) -> &'static str {
    match step {
        PlanStep::Ddl(_) => "DDL",
        PlanStep::Dml { .. } => "DML",
        PlanStep::Backfill { .. } => "backfill",
        PlanStep::AlterPrimaryKey(_) => "alterPrimaryKey",
        PlanStep::AlterColumnType(_) => "setColumnType",
        PlanStep::SynchronizeIdentity(_) => "synchronizeIdentity",
        PlanStep::OnlineRename(_) => "onlineRename",
    }
}

/// Roll back migrations: plan every refusal, then run each `down` in order.
///
/// The counterpart to [`apply`], and the driver [`plan_rollback`] was written for.
/// Selection is all-or-nothing: [`plan_rollback`] decides approval, target
/// resolution, `down` availability, checksum agreement, reversibility,
/// transactionality, the guard over the `down` SQL, dependency coherence and
/// ordering BEFORE a single statement runs, so a rollback that would be refused
/// four migrations deep is refused before the first one touches the database.
///
/// Execution then walks the planned steps in reverse topological order of
/// `depends_on`, handing each to
/// [`MigrationBackend::rollback_one_transactional`], which commits the `down` and
/// its `rolled_back` journal event atomically. A rolled-back version becomes
/// re-pending, because the journal's net state for it is no longer `completed`.
///
/// Generic over [`MigrationBackend`] rather than the `SqlSession` seam, because the
/// per-migration leaf lives on that trait: this runs on PostgreSQL, MySQL and
/// SQLite through the same code.
///
/// Takes the project advisory lock for the whole unwind, so a rollback and a
/// concurrent deploy cannot interleave. Use [`rollback_with_lock`] when an outer
/// operation already holds it.
///
/// # Errors
/// Any [`RollbackError`]. A selection variant means nothing ran at all. A
/// [`RollbackError::DownFailed`] names the migration whose `down` failed, and every
/// migration ahead of it in the plan is already rolled back and journaled.
pub async fn rollback<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    request: &RollbackRequest,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
    guard: &dyn crate::guard::MigrationGuard,
) -> Result<RollbackOutcome, RollbackError> {
    rollback_with_lock(
        backend,
        cfg,
        request,
        migrations,
        approval,
        applied_by,
        guard,
        LockMode::Acquire,
    )
    .await
}

/// [`rollback`] with an explicit [`LockMode`], for a caller that already holds the
/// project advisory lock.
///
/// The lock is released on the `Acquire` path even when the unwind fails, and the
/// rollback error is surfaced ahead of any unlock error, matching what `apply` does:
/// the caller needs the reason the `down` failed, not the reason the unlock did.
///
/// Pass [`LockMode::AlreadyHeld`] when nesting under an outer holder. It matters more
/// on SQLite than on the server dialects: `SqliteBackend::acquire_project_lock`
/// refuses re-entry on the SAME backend instance outright with "sqlite project lock
/// is already held", where PostgreSQL and MySQL wait on the server. A nested
/// [`LockMode::Acquire`] therefore fails immediately on SQLite rather than blocking,
/// and the message reads like a defect rather than a caller mistake.
///
/// # Errors
/// As [`rollback`], plus [`RollbackError::Backend`] if the lock cannot be taken.
pub async fn rollback_with_lock<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    request: &RollbackRequest,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
    guard: &dyn crate::guard::MigrationGuard,
    lock_mode: LockMode,
) -> Result<RollbackOutcome, RollbackError> {
    rollback_with_lock_and_inverse_plans(
        backend,
        cfg,
        request,
        migrations,
        &std::collections::BTreeMap::new(),
        approval,
        applied_by,
        guard,
        lock_mode,
    )
    .await
}

/// Roll back with structured inverse plans keyed by their FORWARD journal id.
/// Existing SQL-only callers use [`rollback_with_lock`]; host IR rollback uses
/// this entry so parameterized inverse DML never crosses a text `down` seam.
#[allow(clippy::too_many_arguments)]
pub async fn rollback_with_lock_and_inverse_plans<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    request: &RollbackRequest,
    migrations: &[Migration],
    inverse_plans: &std::collections::BTreeMap<String, AppliedPlan>,
    approval: Approval,
    applied_by: &str,
    guard: &dyn crate::guard::MigrationGuard,
    lock_mode: LockMode,
) -> Result<RollbackOutcome, RollbackError> {
    if lock_mode == LockMode::Acquire {
        backend
            .acquire_project_lock(cfg)
            .await
            .map_err(|e| RollbackError::Backend(e.to_string()))?;
    }
    let result = rollback_locked(
        backend,
        cfg,
        request,
        migrations,
        inverse_plans,
        approval,
        applied_by,
        guard,
    )
    .await;
    if lock_mode == LockMode::AlreadyHeld {
        return result;
    }
    let unlock = backend
        .release_project_lock(cfg)
        .await
        .map_err(|e| RollbackError::Backend(e.to_string()));
    match result {
        Ok(o) => unlock.map(|()| o),
        Err(e) => Err(e),
    }
}

/// The rollback body, run while holding the project advisory lock.
async fn rollback_locked<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    request: &RollbackRequest,
    migrations: &[Migration],
    inverse_plans: &std::collections::BTreeMap<String, AppliedPlan>,
    approval: Approval,
    applied_by: &str,
    guard: &dyn crate::guard::MigrationGuard,
) -> Result<RollbackOutcome, RollbackError> {
    backend.ensure_journal(cfg).await?;

    // A lone `started` marker is inflight work, not an applied migration, so it is
    // not something a rollback unwinds - it is the recovery path's business. Only
    // net-`completed` versions are candidates.
    let applied: Vec<AppliedRecord> = backend
        .applied(cfg)
        .await?
        .into_iter()
        .filter(|e| e.phase == Phase::Completed)
        .map(|e| AppliedRecord {
            version: e.version,
            checksum: e.checksum,
            event_seq: e.event_seq,
        })
        .collect();

    // Read under the same lock as the journal, so the obligation set and the applied
    // set describe one coherent moment. A backend with no cross-deploy obligation
    // capability has none to observe, which is why the empty arm is a fact and not a
    // fallback.
    let outstanding = match backend.pending_contracts() {
        Some(capability) => capability.outstanding_pending_contracts(cfg).await?,
        None => Vec::new(),
    };

    // Only migrations the journal actually holds can be selected, so only those are
    // worth parsing. The verdict is the dialect's, read here where the backend is, and
    // handed to the planner as data so every refusal stays in one pure function.
    let applied_versions: std::collections::HashSet<&str> =
        applied.iter().map(|r| r.version.as_str()).collect();
    let non_txn_downs: std::collections::BTreeMap<String, String> = migrations
        .iter()
        .filter(|m| applied_versions.contains(m.version.as_str()))
        .filter_map(|m| {
            backend
                .non_transactional_down_reason(m)
                .map(|reason| (m.version.as_str().to_string(), reason))
        })
        .collect();

    let plan = plan_rollback_with_inverse_plans(
        request,
        migrations,
        inverse_plans,
        &applied,
        &outstanding,
        &non_txn_downs,
        approval,
        guard,
    )?;

    // Feature probes and inverse-shape validation finish before the first down,
    // preserving rollback's all-up-front refusal contract.
    for m in &plan.steps {
        if let Some(inverse) = inverse_plans.get(m.version.as_str()) {
            backend
                .verify_database_requirements(&inverse.database_requirements)
                .await
                .map_err(|error| RollbackError::Backend(error.to_string()))?;
        }
    }

    let mut rolled_back = Vec::with_capacity(plan.steps.len());
    for m in &plan.steps {
        if let Some(inverse) = inverse_plans.get(m.version.as_str()) {
            backend
                .rollback_plan_transactional(cfg, m, &inverse.steps, applied_by)
                .await?;
        } else {
            backend
                .rollback_one_transactional(cfg, m, applied_by)
                .await?;
        }
        rolled_back.push(m.version.as_str().to_string());
    }

    Ok(RollbackOutcome {
        rolled_back,
        skipped_irreversible: plan.skipped_irreversible,
    })
}

// ===========================================================================
// The Trusted profile applies arbitrary SQL on a REAL Postgres.
//
// These MUST be in-crate because `ExecutorConfig::trusted` is `pub(crate)` AND
// `#[cfg(test)]`, so an integration test - a separate crate - cannot construct a
// Trusted config. `OperatorCapability::for_test` is NOT what stops it: that is `pub`
// under an additive feature a downstream can turn on, and the token authorises
// nothing anyway. The external boundary that is genuinely pinned is the unforgeable
// `EffectivePolicy`, held by the T8 `compile_fail` doctests in
// `zero_migrate_guard::guard`.
//
// They run the FULL `executor::apply` path under a Trusted `ExecutorConfig` and
// prove (a) SQL the Confined guard hard-denies APPLIES, and (b) a destructive op
// still carries the approval flag (so the CLI `--yes` gate holds).
// ===========================================================================

#[cfg(test)]
mod rollback_selection_tests {
    use super::*;
    use crate::model::migration::{Checksum, MigrationFlags, MigrationId};

    /// Every gate test below predates the rename-obligation gate and exercises a
    /// database with no outstanding obligation, so each supplies an empty set. The
    /// shim shadows the real function rather than each call repeating `&[]`, which
    /// keeps the "no obligations here" premise stated once where it can be read.
    /// `outstanding_obligation_blocks_the_rollback` calls `super::plan_rollback`
    /// directly, because supplying an obligation is the whole point of that one.
    fn plan_rollback<'a>(
        request: &RollbackRequest,
        migrations: &'a [Migration],
        applied: &[AppliedRecord],
        approval: Approval,
        guard: &dyn crate::guard::MigrationGuard,
    ) -> Result<RollbackPlan<'a>, RollbackError> {
        super::plan_rollback(
            request,
            migrations,
            applied,
            &[],
            &std::collections::BTreeMap::new(),
            approval,
            guard,
        )
    }

    /// A guard that admits everything, so gate tests isolate the gate under test.
    struct PermissiveGuard;
    impl crate::guard::MigrationGuard for PermissiveGuard {
        fn check(&self, _up: &str) -> Result<crate::guard::GuardOutcome, crate::guard::GuardError> {
            Ok(crate::guard::GuardOutcome::default())
        }
    }

    /// A guard that refuses everything, to prove the `down` is guarded at all.
    struct DenyingGuard;
    impl crate::guard::MigrationGuard for DenyingGuard {
        fn check(&self, _up: &str) -> Result<crate::guard::GuardOutcome, crate::guard::GuardError> {
            Err(crate::guard::GuardError::MysqlRawSqlRejected)
        }
    }

    fn mig(version: MigrationId, down: Option<&str>, depends_on: Vec<MigrationId>) -> Migration {
        let up = format!("CREATE TABLE t_{}()", version.as_str());
        let flags = MigrationFlags::default();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: &up,
            down,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &depends_on,
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up,
            down: down.map(str::to_string),
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on,
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
            effect: None,
        }
    }

    fn req(target: RollbackTarget) -> RollbackRequest {
        RollbackRequest {
            target,
            options: RollbackOptions::default(),
        }
    }

    /// Applied records in APPLY order: seq 1, 2, 3 ... matching the slice order,
    /// deliberately independent of how the version strings sort.
    fn versions(ms: &[Migration]) -> Vec<AppliedRecord> {
        ms.iter()
            .enumerate()
            .map(|(i, m)| AppliedRecord {
                version: m.version.as_str().to_string(),
                checksum: m.checksum.as_str().to_string(),
                event_seq: i as i64 + 1,
            })
            .collect()
    }

    /// An outstanding online-rename obligation naming `plan_version` as the identity
    /// a rollback can see, plus whatever contract versions it still owes.
    fn obligation(
        plan_version: &str,
        contract_versions: Vec<String>,
    ) -> crate::apply::journal::PendingContract {
        crate::apply::journal::PendingContract {
            owner_app: Some("app_test".to_string()),
            table: "accounts".to_string(),
            from_col: "email".to_string(),
            to_col: "email_address".to_string(),
            ty: "text".to_string(),
            // The deep expand sub-step id. Deliberately unlike any version the
            // supplied set carries, so a gate keying on it would never fire.
            pending_version: "mig_deep_expand_substep".to_string(),
            plan_version: plan_version.to_string(),
            contract_versions,
        }
    }

    /// Three migrations with no edges, oldest first.
    fn three() -> Vec<Migration> {
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let c = MigrationId::generate();
        vec![
            mig(a, Some("DROP TABLE a"), vec![]),
            mig(b, Some("DROP TABLE b"), vec![]),
            mig(c, Some("DROP TABLE c"), vec![]),
        ]
    }

    #[test]
    fn a_transactional_parameterized_inverse_makes_a_data_identity_reversible() {
        let version = MigrationId::generate();
        let migration = mig(version.clone(), None, vec![]);
        let inverse = AppliedPlan {
            version: version.clone(),
            name: migration.name.clone(),
            steps: vec![PlanStep::Dml {
                version: version.clone(),
                checksum: migration.checksum.clone(),
                name: "delete seeded row".to_string(),
                template: "DELETE FROM acct WHERE id = $1".to_string(),
                binds: vec![crate::render::step::BindValue::Int(1)],
                target_schema: "app".to_string(),
                target_table: "acct".to_string(),
                conflict_target: None,
                mutates_data: true,
                transactional: true,
                destructive: true,
                requires_approval: true,
                owner_app: migration.owner_app.clone(),
            }],
            database_requirements: crate::render::plan::DatabaseRequirements::default(),
            checksum: migration.checksum.clone(),
            flags: migration.flags,
            dialect_scope: crate::render::step::DialectScope::Portable,
            rollbackable: false,
            owner_app: migration.owner_app.clone(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
        };
        let inverses = std::collections::BTreeMap::from([(version.as_str().to_string(), inverse)]);
        let applied = versions(std::slice::from_ref(&migration));

        let plan = super::plan_rollback_with_inverse_plans(
            &req(RollbackTarget::All),
            std::slice::from_ref(&migration),
            &inverses,
            &applied,
            &[],
            &std::collections::BTreeMap::new(),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("a structured inverse supplies reversibility without text down SQL");

        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].version, version);
        assert!(plan.steps[0].down.is_none(), "no text down was fabricated");
    }

    #[test]
    fn rollback_without_approval_is_refused_before_anything_else() {
        let set = three();
        let err = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &versions(&set),
            Approval::None,
            &PermissiveGuard,
        )
        .expect_err("every down is destructive, so approval is unconditional");
        assert!(matches!(err, RollbackError::ApprovalRequired), "{err:?}");
    }

    #[test]
    fn unwinding_runs_newest_first() {
        let set = three();
        let plan = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &versions(&set),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("plan");
        let got: Vec<&str> = plan.steps.iter().map(|m| m.version.as_str()).collect();
        let mut want: Vec<&str> = set.iter().map(|m| m.version.as_str()).collect();
        want.reverse();
        assert_eq!(
            got, want,
            "with no edges, rollback is reverse-version order"
        );
    }

    #[test]
    fn to_version_keeps_the_named_migration() {
        let set = three();
        let vs = versions(&set);
        let plan = plan_rollback(
            &req(RollbackTarget::ToVersion(set[0].version.clone())),
            &set,
            &vs,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("plan");
        let got: Vec<&str> = plan.steps.iter().map(|m| m.version.as_str()).collect();
        assert_eq!(
            got,
            vec![set[2].version.as_str(), set[1].version.as_str()],
            "unwind down TO the target, keeping it"
        );
    }

    // ---------------------------------------------------------------------
    // An online rename keeps both columns alive until its contract resolves.
    // Rolling back a version inside that obligation drops the destination and
    // wedges the table: `resolve` checks `columns_compatible` ahead of BOTH its
    // commit arm and its abort arm, and that check needs both columns present.
    // So the unwind is refused before it runs, and the refusal names the table
    // and the contract rather than only the version.
    // ---------------------------------------------------------------------
    #[test]
    fn outstanding_obligation_blocks_the_rollback() {
        let set = three();
        let vs = versions(&set);
        let expand = set[2].version.as_str().to_string();

        let err = super::plan_rollback(
            &req(RollbackTarget::Steps(1)),
            &set,
            &vs,
            &[obligation(&expand, vec![])],
            &std::collections::BTreeMap::new(),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("a version inside an outstanding rename must not be rolled back");
        match err {
            RollbackError::PendingContractOutstanding {
                version,
                table,
                plan_version,
            } => {
                assert_eq!(version, expand);
                assert_eq!(table, "accounts");
                assert_eq!(plan_version, expand);
            }
            other => panic!("expected PendingContractOutstanding, got {other:?}"),
        }

        // The contract half is covered too: an obligation still owing C1/C2 names
        // those versions, and rolling one back strands the same interlock.
        let contract = set[1].version.as_str().to_string();
        let err = super::plan_rollback(
            &req(RollbackTarget::Steps(2)),
            &set,
            &vs,
            &[obligation("mig_some_other_plan", vec![contract.clone()])],
            &std::collections::BTreeMap::new(),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("a contract version inside an outstanding rename is equally unsafe");
        assert!(
            matches!(err, RollbackError::PendingContractOutstanding { ref version, .. } if *version == contract),
            "expected the contract version named, got {err:?}"
        );

        // The gate is scoped to the obligation, not to rollback in general: an
        // obligation over versions nobody selected leaves the unwind alone.
        let plan = super::plan_rollback(
            &req(RollbackTarget::Steps(1)),
            &set,
            &vs,
            &[obligation(
                "mig_unrelated_plan",
                vec!["mig_unrelated_c1".to_string()],
            )],
            &std::collections::BTreeMap::new(),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("an unrelated obligation must not block an unwind");
        assert_eq!(plan.steps.len(), 1);
    }

    // ---------------------------------------------------------------------
    // The `transaction` flag is what the author DECLARED. A migration can declare
    // `transaction: true` and still reverse itself with a statement PostgreSQL
    // refuses inside a transaction block, and the leaf opens one unconditionally.
    // The dialect's reading of the reverse SQL is therefore a second, independent
    // input to the same gate.
    // ---------------------------------------------------------------------
    #[test]
    fn a_down_the_dialect_cannot_run_in_a_transaction_is_refused() {
        let set = three();
        let vs = versions(&set);
        let offender = set[2].version.as_str().to_string();
        let verdicts: std::collections::BTreeMap<String, String> = [(
            offender.clone(),
            "its `down` runs `DROP INDEX CONCURRENTLY`".to_string(),
        )]
        .into_iter()
        .collect();

        let err = super::plan_rollback(
            &req(RollbackTarget::Steps(1)),
            &set,
            &vs,
            &[],
            &verdicts,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("a down the dialect refuses inside a transaction must not be planned");
        match err {
            RollbackError::NonTransactionalDown { version, reason } => {
                assert_eq!(version, offender);
                // The reason is the dialect's own words, not a restatement of the flag.
                assert!(reason.contains("DROP INDEX CONCURRENTLY"), "{reason}");
                assert!(!reason.contains("transaction:false"), "{reason}");
            }
            other => panic!("expected NonTransactionalDown, got {other:?}"),
        }

        // Scoped to the version it names: a verdict about a migration nobody selected
        // leaves the unwind alone.
        let elsewhere: std::collections::BTreeMap<String, String> =
            [("mig_not_selected".to_string(), "irrelevant".to_string())]
                .into_iter()
                .collect();
        let plan = super::plan_rollback(
            &req(RollbackTarget::Steps(1)),
            &set,
            &vs,
            &[],
            &elsewhere,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("a verdict about an unselected version must not block an unwind");
        assert_eq!(plan.steps.len(), 1);
    }

    #[test]
    fn steps_takes_the_most_recent_n() {
        let set = three();
        let plan = plan_rollback(
            &req(RollbackTarget::Steps(2)),
            &set,
            &versions(&set),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("plan");
        let got: Vec<&str> = plan.steps.iter().map(|m| m.version.as_str()).collect();
        assert_eq!(got, vec![set[2].version.as_str(), set[1].version.as_str()]);
    }

    /// `ToVersion` resolves its anchor by APPLY order, never by how version
    /// strings sort.
    ///
    /// `selection_uses_apply_order_not_version_order` already pins this for
    /// `Steps`, and pins it well. It does not reach `ToVersion`, which is a
    /// SEPARATE code path: `Steps` takes a prefix of the sorted list, while
    /// `ToVersion` finds an anchor and filters on `event_seq > anchor.event_seq`.
    /// Changing only that filter to compare versions leaves the sibling test
    /// green, so without this the gap is unwatched.
    ///
    /// The failure it admits is the quiet kind. Anchoring on the first-applied
    /// migration when that migration also holds the HIGHEST version means nothing
    /// sorts after it, so a version-ordered filter selects the empty set and the
    /// rollback reports success having unwound nothing.
    ///
    /// Version order carries no ordering information at all — `AppliedEntry::
    /// event_seq` documents that `MigrationId::derive` stamps the high bits with
    /// an `0xFF` marker and fills the rest from a SHA-256, so derived ids sort in
    /// hash order among themselves and above every generated id. Three migrations
    /// applied in exactly the reverse of their version order make that concrete.
    #[test]
    fn to_version_anchors_by_apply_order_not_version_order() {
        let mut ids = [
            MigrationId::derive("rb", b"one"),
            MigrationId::derive("rb", b"two"),
            MigrationId::derive("rb", b"three"),
        ];
        ids.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        assert!(
            ids[0].as_str() < ids[1].as_str() && ids[1].as_str() < ids[2].as_str(),
            "the ids must be in ascending version order for the reversal below to mean anything"
        );

        // Applied HIGHEST version first, so the last applied is the lowest version.
        let set: Vec<Migration> = ids
            .iter()
            .rev()
            .map(|id| mig(id.clone(), Some("DROP TABLE t"), vec![]))
            .collect();
        let applied = versions(&set);

        // Anchored on the FIRST applied, which is also the highest version:
        // everything applied after it must be selected. A version-ordered filter
        // finds nothing "after" the highest version and unwinds the empty set.
        let after_first = plan_rollback(
            &req(RollbackTarget::ToVersion(ids[2].clone())),
            &set,
            &applied,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("plan to version");
        let mut reached: Vec<&str> = after_first
            .steps
            .iter()
            .map(|m| m.version.as_str())
            .collect();
        reached.sort_unstable();
        let mut expected = vec![ids[0].as_str(), ids[1].as_str()];
        expected.sort_unstable();
        assert_eq!(
            reached, expected,
            "ToVersion keeps its anchor and unwinds what was applied AFTER it, by apply order"
        );
    }

    #[test]
    fn an_unknown_target_is_refused() {
        let set = three();
        let stranger = MigrationId::generate();
        let err = plan_rollback(
            &req(RollbackTarget::ToVersion(stranger)),
            &set,
            &versions(&set),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("a target that is not applied cannot anchor an unwind");
        assert!(
            matches!(err, RollbackError::UnknownTarget { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_applied_migration_absent_from_the_set_is_refused() {
        let set = three();
        let mut vs = versions(&set);
        vs.push(AppliedRecord {
            version: MigrationId::generate().as_str().to_string(),
            checksum: "deadbeef".into(),
            event_seq: 99,
        });
        let err = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &vs,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("the down lives in the file, so an absent file has no reverse SQL");
        assert!(
            matches!(err, RollbackError::MissingFromSet { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn an_irreversible_migration_is_refused_unless_forced_with_a_backup() {
        let a = MigrationId::generate();
        let set = vec![mig(a, None, vec![])];
        let vs = versions(&set);

        let err = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &vs,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("down: None refuses by default");
        assert!(matches!(err, RollbackError::Irreversible { .. }), "{err:?}");

        // force alone is not enough - skipping an irreversible step loses data.
        let force_only = RollbackRequest {
            target: RollbackTarget::All,
            options: RollbackOptions {
                force: true,
                backup_acknowledged: false,
            },
        };
        let err = plan_rollback(&force_only, &set, &vs, Approval::Approved, &PermissiveGuard)
            .expect_err("force without a backup acknowledgement still refuses");
        assert!(matches!(err, RollbackError::Irreversible { .. }), "{err:?}");

        let forced = RollbackRequest {
            target: RollbackTarget::All,
            options: RollbackOptions {
                force: true,
                backup_acknowledged: true,
            },
        };
        let plan = plan_rollback(&forced, &set, &vs, Approval::Approved, &PermissiveGuard)
            .expect("both flags together skip the step");
        assert!(plan.steps.is_empty(), "nothing to run");
        assert_eq!(plan.skipped_irreversible.len(), 1);
    }

    #[test]
    fn a_non_transactional_down_is_refused() {
        let a = MigrationId::generate();
        let mut m = mig(a, Some("DROP INDEX CONCURRENTLY i"), vec![]);
        m.flags.transactional = false;
        let set = vec![m];
        let err = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &versions(&set),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("the executor only runs a down inside a transaction");
        assert!(
            matches!(err, RollbackError::NonTransactionalDown { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn the_down_sql_goes_through_the_guard() {
        let set = three();
        let err = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &versions(&set),
            Approval::Approved,
            &DenyingGuard,
        )
        .expect_err("a down is author SQL reaching the database, so it is guarded");
        assert!(matches!(err, RollbackError::Guard { .. }), "{err:?}");
    }

    #[test]
    fn a_kept_migration_may_not_depend_on_a_rolled_back_one() {
        // b depends on a. Rolling back only `a` would leave `b` applied and
        // referencing an object that no longer exists.
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        let set = vec![
            mig(a.clone(), Some("DROP TABLE a"), vec![]),
            mig(b, Some("DROP TABLE b"), vec![a]),
        ];
        let vs = versions(&set);

        // Steps(1) takes only the newest (b) - coherent, because nothing left
        // applied depends on what came out.
        plan_rollback(
            &req(RollbackTarget::Steps(1)),
            &set,
            &vs,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("rolling back the dependent alone is coherent");

        // The incoherent shape needs an OLDER migration depending on a NEWER one,
        // so the threshold can keep the dependent while tearing down its dependency.
        // `depends_on` usually points backwards in time, which is why the ordinary
        // unwind is safe; this is the case that is not.
        let old = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let new = MigrationId::generate();
        let inverted = vec![
            mig(old.clone(), Some("DROP TABLE old"), vec![new.clone()]),
            mig(new, Some("DROP TABLE new"), vec![]),
        ];
        let err = plan_rollback(
            &req(RollbackTarget::ToVersion(old)),
            &inverted,
            &versions(&inverted),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("keeping `old` while rolling back what it depends on is incoherent");
        assert!(
            matches!(err, RollbackError::KeptDependsOnRolledBack { .. }),
            "{err:?}"
        );

        // Force-skip b as irreversible while rolling back its dependency a.
        let a2 = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b2 = MigrationId::generate();
        let set2 = vec![
            mig(a2.clone(), Some("DROP TABLE a"), vec![]),
            mig(b2, None, vec![a2]),
        ];
        let forced = RollbackRequest {
            target: RollbackTarget::All,
            options: RollbackOptions {
                force: true,
                backup_acknowledged: true,
            },
        };
        let err = plan_rollback(
            &forced,
            &set2,
            &versions(&set2),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("a force-skipped migration still depends on what is torn down");
        assert!(
            matches!(err, RollbackError::ForceSkipDependencyConflict { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_dependent_runs_its_down_before_the_dependency() {
        // b depends_on a, so apply order is a then b; rollback is the transpose.
        let a = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MigrationId::generate();
        let set = vec![
            mig(a.clone(), Some("DROP TABLE a"), vec![]),
            mig(b.clone(), Some("DROP TABLE b"), vec![a.clone()]),
        ];
        let plan = plan_rollback(
            &req(RollbackTarget::All),
            &set,
            &versions(&set),
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("plan");
        let got: Vec<&str> = plan.steps.iter().map(|m| m.version.as_str()).collect();
        assert_eq!(
            got,
            vec![b.as_str(), a.as_str()],
            "the dependent tears down first"
        );
    }
}

#[cfg(test)]
mod rollback_selection_ordering_tests {
    use super::*;

    /// These ordering tests observe no rename obligation, so they supply none. Same
    /// shim as the selection module above, for the same reason.
    fn plan_rollback<'a>(
        request: &RollbackRequest,
        migrations: &'a [Migration],
        applied: &[AppliedRecord],
        approval: Approval,
        guard: &dyn crate::guard::MigrationGuard,
    ) -> Result<RollbackPlan<'a>, RollbackError> {
        super::plan_rollback(
            request,
            migrations,
            applied,
            &[],
            &std::collections::BTreeMap::new(),
            approval,
            guard,
        )
    }
    use crate::model::migration::{Checksum, MigrationFlags, MigrationId};

    struct PermissiveGuard;
    impl crate::guard::MigrationGuard for PermissiveGuard {
        fn check(&self, _up: &str) -> Result<crate::guard::GuardOutcome, crate::guard::GuardError> {
            Ok(crate::guard::GuardOutcome::default())
        }
    }

    fn mig(version: MigrationId) -> Migration {
        let up = "CREATE TABLE t()".to_string();
        let down = Some("DROP TABLE t".to_string());
        let flags = MigrationFlags::default();
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: &up,
            down: down.as_deref(),
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up,
            down,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
            effect: None,
        }
    }

    /// Selection follows the journal, not the version string.
    ///
    /// `MigrationId::derive` stamps the high 48 bits with an `0xFF` marker and fills
    /// the rest from a SHA-256, so every derived (IR-authored) id sorts ABOVE every
    /// `UUIDv7` file id. A selector that sorted versions would treat the derived id
    /// as newest no matter when it was actually applied.
    #[test]
    fn selection_uses_apply_order_not_version_order() {
        let derived = MigrationId::derive("ir_step", b"seed");
        std::thread::sleep(std::time::Duration::from_millis(2));
        let file = MigrationId::generate();

        // The premise this test exists to defend.
        assert!(
            derived.as_str() > file.as_str(),
            "a derived id sorts above a generated one: {} vs {}",
            derived.as_str(),
            file.as_str()
        );

        // But the DERIVED one was applied FIRST.
        let set = vec![mig(derived.clone()), mig(file.clone())];
        let applied = vec![
            AppliedRecord {
                version: derived.as_str().to_string(),
                checksum: set[0].checksum.as_str().to_string(),
                event_seq: 1,
            },
            AppliedRecord {
                version: file.as_str().to_string(),
                checksum: set[1].checksum.as_str().to_string(),
                event_seq: 2,
            },
        ];

        let plan = plan_rollback(
            &RollbackRequest {
                target: RollbackTarget::Steps(1),
                options: RollbackOptions::default(),
            },
            &set,
            &applied,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect("plan");
        assert_eq!(
            plan.steps.len(),
            1,
            "Steps(1) unwinds exactly one migration"
        );
        assert_eq!(
            plan.steps[0].version.as_str(),
            file.as_str(),
            "Steps(1) must take the LAST APPLIED migration, not the highest version"
        );
    }

    /// Rollback must not launder the drift gate.
    ///
    /// Rolling a version back returns it to pending, where the next apply runs its
    /// `up`. If the supplied migration no longer matches what the journal recorded,
    /// rolling back would run an edited `down` and then re-apply an edited `up` with
    /// no drift abort, because drift only compares versions whose latest event is
    /// `applied`.
    #[test]
    fn a_migration_edited_since_it_applied_cannot_be_rolled_back() {
        let v = MigrationId::generate();
        let set = vec![mig(v.clone())];
        let applied = vec![AppliedRecord {
            version: v.as_str().to_string(),
            checksum: "a-different-checksum".to_string(),
            event_seq: 1,
        }];

        let err = plan_rollback(
            &RollbackRequest {
                target: RollbackTarget::All,
                options: RollbackOptions::default(),
            },
            &set,
            &applied,
            Approval::Approved,
            &PermissiveGuard,
        )
        .expect_err("the supplied migration is not the one that was applied");
        match err {
            RollbackError::ChecksumDrift {
                version, recorded, ..
            } => {
                assert_eq!(version, v.as_str());
                assert_eq!(recorded, "a-different-checksum");
            }
            other => panic!("expected ChecksumDrift, got {other:?}"),
        }
    }
}
