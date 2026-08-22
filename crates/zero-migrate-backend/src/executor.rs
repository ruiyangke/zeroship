//! The apply/rollback vocabulary every backend's signatures name.
//!
//! This module holds CONCEPTS, not an executor. The generic apply and rollback
//! orchestration stays in the engine (`zero_migrate::apply::executor`); what lives
//! here is the shared shape those orchestrators and every `MigrationBackend` impl
//! agree on: how a batch says who owns the project lock ([`LockMode`]), what an
//! apply reports ([`ApplyOutcome`]) or refuses ([`ApplyError`]), how a driver
//! error crosses the seam without either side naming the other's concrete driver
//! ([`BackendError`]), what a precondition sweep decided
//! ([`PreconditionVerdict`]), and the rollback request/outcome/refusal set
//! ([`RollbackTarget`], [`RollbackRequest`], [`RollbackOptions`],
//! [`RollbackOutcome`], [`RollbackError`]).
//!
//! It emits no SQL and names no vendor. It sits with the traits because the
//! vendors cannot move out of the engine while the types in their own signatures
//! still live above them.

use std::error::Error;
use std::fmt;

use zero_migrate_ir::migration::MigrationId;

use crate::guard::GuardError;
use crate::journal::JournalError;

/// Whether an apply sub-batch must acquire/release the project advisory lock
/// itself, or whether an OUTER caller already holds it for the whole operation.
///
/// A standalone `apply` (engine `apply` / `apply_verified` / the versioned
/// path) uses [`LockMode::Acquire`]: it takes the project advisory lock at the
/// start and releases it on every exit path, serializing the whole apply against
/// concurrent deploys for the same project.
///
/// A **declarative** deploy is several sub-batches — the plain set plus one
/// expand per rename — that must be serialized **as a whole** (to
/// "serialize all migration activity"). The outer
/// `MigrationEngine::apply_declarative`
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

/// What `apply` did.
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

/// Error from `apply`.
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
    /// but the caller passed [`Approval::None`](crate::approval::Approval::None). This is the executor's OWN
    /// defense-in-depth approval gate — independent of (and additional to) the
    /// engine's gate (`MigrationEngine::apply`), so a caller that
    /// drives `apply` directly, bypassing the engine, still cannot run a
    /// destructive batch without explicit approval. Nothing was applied.
    #[error("apply contains a destructive migration but Approval::Approved was not given")]
    ApprovalRequired,
    /// **Per-version approval scoping (anti-bypass).** The batch is approved
    /// ([`crate::approval::Approval::Approved`]) but it carries a DESTRUCTIVE migration whose
    /// version-id is NOT in the operator's reviewed
    /// [`ApprovalScope::Versions`](crate::approval::ApprovalScope::Versions) set. The
    /// executor's OWN defense-in-depth scope gate (mirrors
    /// [`ApprovalRequired`](Self::ApprovalRequired)): a direct caller cannot run a
    /// co-bundled destructive op the operator never individually reviewed, even with
    /// blanket [`Approved`](crate::approval::Approval::Approved). Nothing was applied.
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
    /// be recorded WITHOUT running its `up` via `ops::squash`; apply refuses
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
    /// set (all applied via `ops::squash`); a partial set is an inconsistent
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
    /// [`OnUnmet::Halt`](zero_migrate_ir::precondition::OnUnmet::Halt) that was UNMET (it
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
    /// rather than interpolate it. Maps [`crate::dml::IdentQuoteError`]; the
    /// meta-schema journal-write seams route the same byte-logic through
    /// [`JournalError`] (which also carries this `From`).
    #[error("apply: {0}")]
    IdentQuote(#[from] crate::dml::IdentQuoteError),
    /// A migration's effective `statement_timeout` / `lock_timeout` budget
    /// resolved to `0`, which the database reads as "no limit", the opposite of
    /// the mandatory finite budget the executor advertises. Refused before the
    /// migration's SQL reaches the server; nothing ran. See
    /// [`crate::timeout`].
    #[error(transparent)]
    IndefiniteTimeout(#[from] crate::timeout::IndefiniteTimeoutError),
}
impl From<crate::driver::DbError> for ApplyError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}
/// The verdict of evaluating a migration's preconditions.
///
/// `pub` because it is the return type of
/// `MigrationBackend::evaluate_preconditions`
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
/// How far a rollback should unwind the applied migrations.
///
/// Every variant here is resolved in **apply order** — the journal's `event_seq`
/// — and never by how version strings sort. The distinction is not cosmetic:
/// [`AppliedEntry::event_seq`](crate::journal::AppliedEntry::event_seq)
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
    /// rather than interpolate it. Maps [`crate::dml::IdentQuoteError`].
    #[error("rollback: {0}")]
    IdentQuote(#[from] crate::dml::IdentQuoteError),
    /// A migration's effective `statement_timeout` / `lock_timeout` budget
    /// resolved to `0` while rendering the `down` session. Same rule as
    /// [`ApplyError::IndefiniteTimeout`]: a `down` is author SQL that takes locks
    /// too, so it never runs on an unbounded budget. Nothing was rolled back.
    #[error(transparent)]
    IndefiniteTimeout(#[from] crate::timeout::IndefiniteTimeoutError),
    /// A dialect-level backend error whose message is already the intended
    /// operator-facing text. See [`ApplyError::Backend`].
    #[error("backend error: {0}")]
    Backend(String),
    /// Reserved for a future rollback orchestrator that would reject
    /// [`Approval::None`](crate::approval::Approval::None) and require [`Approval::Approved`](crate::approval::Approval::Approved) before any `down`.
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
    /// classifier apply uses, `classify()`) and refuse the WHOLE rollback
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

impl From<crate::driver::DbError> for RollbackError {
    fn from(error: crate::driver::DbError) -> Self {
        Self::Db(error.into())
    }
}
