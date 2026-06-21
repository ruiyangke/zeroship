//! The versioned executor — the apply flow (design §2.3 / §2.4).
//!
//! The heart of the engine. Given a connection, an [`ExecutorConfig`], and the
//! project's full migration set, [`apply`]:
//!
//! 1. acquires the project advisory lock `pg_advisory_lock(hashtext(project_id))`
//!    (serialize all migration activity; released at end);
//! 2. bootstraps the journal (idempotent);
//! 3. computes `pending = set − applied`, in `UUIDv7` version order;
//! 4. re-verifies the checksums of already-applied migrations — a mismatch is a
//!    hard abort (drift / tamper, design §1.5 / §3.6);
//! 5. **first pass (static, all-up-front):** runs the **[`SqlGuard`]** over the
//!    `up` SQL of EVERY pending migration, and validates that every
//!    non-transactional `up` is **idempotent** (each non-txn statement uses the
//!    `IF NOT EXISTS` form). A denial / non-idempotent op aborts the whole apply
//!    before ANY migration executes — a denied batch applies *nothing* (no
//!    earlier migration half-commits);
//! 6. **second pass (execute):** for each pending migration, applies either:
//!    - **transactionally** (default): `BEGIN; SET LOCAL …; <up>; INSERT
//!      journal; COMMIT` — DDL + journal atomic, so a crash leaves
//!      applied+recorded *or* neither. The `SET LOCAL` timeouts/`search_path` are
//!      transaction-scoped, so they never leak onto the session;
//!    - **non-transactionally** (opt-in, e.g. `CREATE INDEX CONCURRENTLY IF NOT
//!      EXISTS`): two-phase `started` marker → run `<up>` → `completed` row +
//!      clear marker. A lone `started` marker on a re-run triggers the recovery
//!      path, which (because the `up` is required to be idempotent) simply drops
//!      any INVALID-index residue and **re-runs `<up>`** — safe whether the
//!      prior attempt failed mid-build OR succeeded then crashed before
//!      recording `completed`.
//! 7. restores the session GUCs it touched and releases the lock.
//!
//! Runs out-of-band at deploy, async on compio — ZERO tokio.

use std::collections::HashMap;
use std::time::Instant;

use compio_postgres::Client;
use pg_query::protobuf::node::Node as NodeEnum;

use crate::approval::Approval;
use crate::backend::{JournalAtomicity, MigrationBackend, PostgresBackend, SessionSnapshot};
use crate::db::ExecutorConfig;
use crate::guard::{GuardError, SqlGuard};
use crate::journal::{self, AppliedEntry, JournalError, Phase};
use crate::migration::{Migration, MigrationId};

/// The Postgres leaf operations the [`PostgresBackend`](crate::backend::PostgresBackend)
/// drives the [`MigrationBackend`](crate::backend::MigrationBackend) trait
/// through. These are the EXACT pre-seam executor functions — the dialect-coupled
/// session/lock/txn/up/down/parse/precondition leaves — re-exported under one
/// path so the backend impl can name them without exposing them crate-wide as
/// loose free functions. They stay defined inline in this module (so their rich
/// doc-comments and the surrounding apply/rollback context are unchanged); this
/// `pub(crate) mod pg` is a thin façade, not a relocation.
pub(crate) mod pg {
    pub(crate) use super::{
        acquire_project_lock, apply_non_transactional, apply_transactional,
        configure_session_non_txn, release_project_lock, restore_session,
        rollback_one_transactional, snapshot_session, validate_non_txn_idempotent,
    };
}

/// Whether an apply sub-batch must acquire/release the project advisory lock
/// itself, or whether an OUTER caller already holds it for the whole operation
/// (H10).
///
/// A standalone [`apply`] (engine `apply` / `apply_verified` / the versioned
/// path) uses [`LockMode::Acquire`]: it takes the project advisory lock at the
/// start and releases it on every exit path, serializing the whole apply against
/// concurrent deploys for the same project.
///
/// A **declarative** deploy is several sub-batches — the plain set plus one
/// expand per rename — that must be serialized **as a whole** (design §2.3:
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
/// every sub-batch regardless of lock mode — those are session-leak guards (H2 /
/// L1), independent of who owns the lock.
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

/// Error from [`apply`].
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A **dialect-neutral** backend error — used by non-Postgres
    /// [`MigrationBackend`](crate::backend::MigrationBackend) impls (e.g.
    /// `SqliteBackend`) that cannot produce a `compio_postgres::Error`. The
    /// payload is the backend's own error rendered to a string, so the generic
    /// executor body and callers can surface it without the trait leaking a
    /// PG-specific error type. The Postgres impl never constructs this arm (it
    /// keeps using [`ApplyError::Db`]), so its behavior is byte-identical.
    #[error("backend error: {0}")]
    Backend(String),
    /// A migration requested the **non-transactional** path (`transaction:false`)
    /// on a dialect that has no non-txn DDL to recover (SQLite, design §2.3/L3).
    /// Rejected at the dialect boundary, before any apply. Postgres never returns
    /// this — its non-txn path is real.
    #[error("migration {version} is transaction:false but the {dialect} backend has no non-transactional DDL path (design §2.3/L3)")]
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
    /// The two-phase non-txn path's crash-recovery re-runs `<up>` verbatim
    /// (C1/C2), so a non-idempotent op would wedge the migration permanently on
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
    /// An already-applied migration's recorded checksum no longer matches the
    /// migration in the set — drift / tamper. Hard abort (design §3.6).
    #[error("checksum drift on {version}: journal has {recorded}, set has {expected}")]
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
    #[error("migration {version} depends on unknown migration {missing} (not in the set or journal)")]
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
    /// A `phase: Contract` online migration (Plan 8 v1.2) was about to be applied
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
    /// be recorded WITHOUT running its `up` via [`crate::squash`]; apply refuses
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
    /// set (all applied via [`crate::squash`]); a partial set is an inconsistent
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
    /// A pending migration carried a precondition (v3 Plan D) with
    /// [`OnUnmet::Halt`](crate::precondition::OnUnmet::Halt) that was UNMET (it
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
    /// repeatable cannot be a squash (v3 Plan E re-critic #4a). A repeatable has a
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
    /// same supplied set (v3 Plan E re-critic #3d). Repeatables run AFTER all
    /// versioned migrations, so a once-only migration can never have a repeatable
    /// dependency satisfied in order. This is a DEDICATED error (not the misleading
    /// [`MissingDependency`] the partition would otherwise raise). Refused in the
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
    /// A `repeatable=true` migration declared a `down` (v3 Plan E re-critic #4c).
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
        /// The DB error from the failed statement.
        #[source]
        source: compio_postgres::Error,
    },
}

/// A stable i64 advisory-lock key from the project id, mirroring the
/// `hashtext(project_id)` design intent: we run `pg_advisory_lock(hashtext($1))`
/// server-side so the key is computed by Postgres exactly as the design states
/// (`hashtext` is the canonical PG text hash; deterministic per cluster).
///
/// Holding it for the whole apply serializes concurrent deploys for the same
/// project (design §2.3 step 1); a second apply waits, then sees the first's
/// committed journal and no-ops.
///
/// M2 (known limitation): `hashtext` yields a 32-bit hash, so two *unrelated*
/// project ids can collide onto the same advisory-lock key. The consequence is
/// liveness-only — two unrelated projects would serialize against each other
/// (one waits for the other's apply) — never a correctness/cross-tenant defect,
/// since each apply still operates strictly within its own meta + project
/// schema. Acceptable for v1. Revisit at scale with a 64-bit key
/// (`pg_advisory_lock(int4, int4)` from a SHA-256 prefix, or two keys).
pub(crate) async fn acquire_project_lock(conn: &Client, project_id: &str) -> Result<(), ApplyError> {
    conn.execute(
        "SELECT pg_advisory_lock(hashtext($1)::bigint)",
        &[&project_id],
    )
    .await?;
    Ok(())
}

pub(crate) async fn release_project_lock(conn: &Client, project_id: &str) -> Result<(), ApplyError> {
    conn.execute(
        "SELECT pg_advisory_unlock(hashtext($1)::bigint)",
        &[&project_id],
    )
    .await?;
    Ok(())
}

/// Read the session GUCs we are about to override, so they can be restored when
/// `apply` finishes. Uses `current_setting(name)` (text form, exactly what `SET`
/// round-trips).
pub(crate) async fn snapshot_session(conn: &Client) -> Result<SessionSnapshot, ApplyError> {
    let row = conn
        .query_one(
            "SELECT current_setting('statement_timeout') AS st, \
                    current_setting('lock_timeout')      AS lt, \
                    current_setting('search_path')       AS sp",
            &[],
        )
        .await?;
    Ok(SessionSnapshot {
        statement_timeout: row.get("st"),
        lock_timeout: row.get("lt"),
        search_path: row.get("sp"),
    })
}

/// Restore the GUCs captured by [`snapshot_session`]. Uses `set_config(name,
/// value, false)` so the *value* is a bound literal, not interpolated SQL
/// (the snapshot strings are server-provided, but we keep the parameterized
/// path regardless).
pub(crate) async fn restore_session(conn: &Client, snap: &SessionSnapshot) -> Result<(), ApplyError> {
    // RESET ROLE first: belt-and-suspenders behind `apply`'s unconditional
    // `RESET ROLE` (L1). The non-txn path's `SET ROLE` mutates the session, so
    // drop back to the admin role before anything else, ensuring the executor's
    // least-privilege confinement never leaks onto the pooled/long-lived
    // connection after `apply` returns (H2). Harmless no-op when no SET ROLE ran
    // (txn-only applies use SET LOCAL ROLE, auto-reverted at COMMIT).
    conn.batch_execute("RESET ROLE").await?;
    conn.execute(
        "SELECT set_config('statement_timeout', $1, false), \
                set_config('lock_timeout', $2, false), \
                set_config('search_path', $3, false)",
        &[
            &snap.statement_timeout,
            &snap.lock_timeout,
            &snap.search_path,
        ],
    )
    .await?;
    Ok(())
}

/// The effective `statement_timeout` for a migration: its per-migration
/// override ([`crate::migration::MigrationFlags::timeout_ms`], H3) if set, else
/// the executor-wide default.
fn effective_timeout_ms(cfg: &ExecutorConfig, m: &Migration) -> u64 {
    m.flags.timeout_ms.unwrap_or_else(|| cfg.statement_timeout_ms())
}

/// `SET LOCAL …` clauses (transaction-scoped) for the **txn path** — they
/// vanish at COMMIT/ROLLBACK, so nothing leaks onto the session (H2). Pins the
/// project `search_path` (project schema **only** — the meta schema is
/// deliberately OFF the migration-time path so an unqualified name in the `up`
/// can never resolve to the journal, C1 defense-in-depth) and the mandatory
/// timeouts (§1.5), with the per-migration timeout override applied (H3).
///
/// This intentionally does **not** switch role: the role scoping is done
/// explicitly in [`apply_transactional`] so that `SET LOCAL ROLE migrator`
/// brackets ONLY the `<up>` and is `RESET` (back to admin) before the journal
/// INSERT — the migrator can no longer write the journal (its grant is revoked;
/// C1 fix), so the journal write must run as the admin, atomically in the SAME
/// transaction as the `up`.
fn set_local_session_sql(cfg: &ExecutorConfig, m: &Migration) -> String {
    format!(
        "SET LOCAL search_path TO {}; \
         SET LOCAL statement_timeout = {}; \
         SET LOCAL lock_timeout = {};",
        cfg.search_path_clause(),
        effective_timeout_ms(cfg, m),
        cfg.lock_timeout_ms(),
    )
}

/// `SET LOCAL ROLE "<migrator>"` for the txn path, or empty when no migrator role
/// is configured (tests / single-tenant dev). Brackets ONLY the `<up>`; the
/// caller `RESET ROLE`s before the journal write (C1).
fn set_local_role_sql(cfg: &ExecutorConfig) -> Option<String> {
    cfg.pg.migrator_role
        .as_ref()
        .map(|role| format!("SET LOCAL ROLE \"{}\"", role.replace('"', "\"\"")))
}

/// Session-level `SET …` for the **non-txn path** (no transaction to scope to).
/// These DO mutate the session, but [`apply`] restores the original GUCs on exit
/// via [`restore_session`] so they never leak (H2). Per-migration timeout
/// override applied (H3).
///
/// Runs as the **admin** role (no `SET ROLE` here): the non-txn journal I/O
/// (`record_started` / `record_completed` / `clear_inflight`) runs as admin, and
/// only the `<up>` is bracketed by an explicit `SET ROLE migrator` / `RESET ROLE`
/// in [`apply_non_transactional`] (C1 fix). `search_path` is the project schema
/// **only** — the meta schema is off the migration-time path so an unqualified
/// name in the `up` can never resolve to the journal.
pub(crate) async fn configure_session_non_txn(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    let stmt = format!(
        "SET search_path TO {}; SET statement_timeout = {}; SET lock_timeout = {};",
        cfg.search_path_clause(),
        effective_timeout_ms(cfg, m),
        cfg.lock_timeout_ms(),
    );
    conn.batch_execute(&stmt).await?;
    Ok(())
}

/// Validate that a non-transactional migration's `up` is **idempotent** (C1/C2).
///
/// The two-phase non-txn recovery path re-runs `<up>` verbatim after a crash, so
/// every statement that cannot run in a transaction must tolerate already having
/// run. We enforce this statically, before any execution:
///
/// - `CREATE INDEX CONCURRENTLY …` MUST be `CREATE INDEX CONCURRENTLY IF NOT
///   EXISTS …`.
/// - `ALTER TYPE … ADD VALUE …` MUST be `… ADD VALUE IF NOT EXISTS …`.
///
/// (Other non-txn ops the classifier recognizes — `DROP INDEX CONCURRENTLY`,
/// `VACUUM` — are themselves naturally re-runnable.)
///
/// Bare DML — `INSERT` / `UPDATE` / `DELETE` / `MERGE` / `TRUNCATE` — is
/// **forbidden** on the non-txn path. The guard admits DML (it is safe data
/// access), and `transaction:false` will happily route a pure-DML `up` onto the
/// two-phase path; but recovery re-runs the `up` VERBATIM, and a bare
/// `INSERT`/`UPDATE`/`DELETE`/`MERGE` is NOT re-runnable — a success-then-crash
/// (the op committed, the `completed` row did not) double-applies it on recovery.
/// `TRUNCATE` is technically re-runnable, but it cannot run on the non-txn path
/// at all (recovery would have to re-run it AFTER the real op, wiping a table the
/// migration itself may have repopulated), so it is forbidden alongside the rest.
/// DML belongs in a transactional migration (the default), where a crash rolls
/// the whole `up` back atomically and re-apply is clean. There is no idempotent
/// form we can mechanically assert for arbitrary DML, so it is rejected
/// outright; an author who genuinely needs a non-txn data step must wrap it in an
/// idempotent guard (e.g. `INSERT … ON CONFLICT DO NOTHING` driven from a DDL op)
/// rather than a bare statement.
///
/// A violation is rejected with [`ApplyError::NonIdempotentNonTxn`].
pub(crate) fn validate_non_txn_idempotent(m: &Migration) -> Result<(), ApplyError> {
    let parsed = pg_query::parse(&m.up).map_err(|e| ApplyError::NonIdempotentNonTxn {
        version: m.version.as_str().to_string(),
        reason: format!("could not parse `up` SQL: {e}"),
    })?;
    for raw_stmt in &parsed.protobuf.stmts {
        let Some(node) = raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) else {
            continue;
        };
        match node {
            NodeEnum::IndexStmt(idx) if idx.concurrent && !idx.if_not_exists => {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`CREATE INDEX CONCURRENTLY {}` lacks `IF NOT EXISTS`",
                        if idx.idxname.is_empty() { "<unnamed>" } else { &idx.idxname }
                    ),
                });
            }
            NodeEnum::AlterEnumStmt(e)
                if !e.new_val.is_empty() && !e.skip_if_new_val_exists =>
            {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`ALTER TYPE … ADD VALUE '{}'` lacks `IF NOT EXISTS`",
                        e.new_val
                    ),
                });
            }
            // Bare DML re-applies on success-then-crash recovery (the up is
            // re-run verbatim). Forbid it on the non-txn path — DML belongs in a
            // transactional migration where a crash rolls it back atomically.
            NodeEnum::InsertStmt(_)
            | NodeEnum::UpdateStmt(_)
            | NodeEnum::DeleteStmt(_)
            | NodeEnum::MergeStmt(_)
            | NodeEnum::TruncateStmt(_) => {
                return Err(ApplyError::NonIdempotentNonTxn {
                    version: m.version.as_str().to_string(),
                    reason: format!(
                        "`{}` is data-manipulation (DML), which crash-recovery would re-run \
                         verbatim and double-apply. Run DML in a transactional migration \
                         (do not set `transaction:false`).",
                        dml_keyword(node)
                    ),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

/// The SQL keyword for a DML node, for the `NonIdempotentNonTxn` reason message.
const fn dml_keyword(node: &NodeEnum) -> &'static str {
    match node {
        NodeEnum::InsertStmt(_) => "INSERT",
        NodeEnum::UpdateStmt(_) => "UPDATE",
        NodeEnum::DeleteStmt(_) => "DELETE",
        NodeEnum::MergeStmt(_) => "MERGE",
        NodeEnum::TruncateStmt(_) => "TRUNCATE",
        _ => "DML",
    }
}

/// Detect whether a rollback `down` contains a statement that cannot run inside
/// a transaction block (#5), reusing apply's classifier
/// ([`crate::classify::classify`], whose [`StatementClass.non_transactional`] is
/// the canonical "needs the two-phase non-txn path" flag — `CREATE`/`DROP INDEX
/// CONCURRENTLY`, `ALTER TYPE … ADD VALUE`, `VACUUM`). Returns `Some(reason)`
/// describing the first offending statement, or `None` if every statement is
/// transaction-safe.
///
/// The rollback executor only has a transactional down path, so a `Some(_)`
/// result means the `down` is unrunnable here and must be refused up-front with
/// [`RollbackError::NonTransactionalDown`] (vs. dying late with PG `25001`).
///
/// A parse failure yields `None` (not our concern here): the guard runs the same
/// parser on the `down` immediately before this and already rejects unparseable
/// SQL, so a `down` that reaches this point parses.
///
/// [`StatementClass.non_transactional`]: crate::classify::StatementClass::non_transactional
fn non_transactional_down_reason(down: &str) -> Option<String> {
    let classes = crate::classify::classify(down).ok()?;
    classes.iter().find(|c| c.non_transactional).map(|c| {
        let raw = c.raw.trim();
        let snippet = raw.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
        format!("`{snippet}` cannot run inside a transaction block")
    })
}

/// Apply the project's pending migrations (design §2.3). Idempotent: a re-run
/// with no new migrations is a no-op.
///
/// `applied_by` is the actor recorded in the journal (`app/actor/AI`).
///
/// `approval` is the caller's approval decision. This is the executor's OWN
/// defense-in-depth approval gate (design §1.6): if any pending migration is
/// flagged [`destructive`](crate::migration::MigrationFlags::destructive) and
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
pub async fn apply(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    // Standalone callers own the lock: acquire it here, release it on exit.
    apply_with_lock(conn, cfg, migrations, approval, applied_by, LockMode::Acquire).await
}

/// [`apply`] with an explicit [`LockMode`] (H10).
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
pub async fn apply_with_lock(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
    lock_mode: LockMode,
) -> Result<ApplyOutcome, ApplyError> {
    // Drive the whole apply through the dialect seam. P1: Postgres is the only
    // backend; constructing it here keeps `apply`/`apply_with_lock`'s public
    // `&Client` signature intact while the apply body below is generic over
    // `MigrationBackend` (no `compio_postgres::Client` reaches `apply_locked`). The
    // defense-in-depth approval gate now lives in `apply_with_lock_backend` so it
    // runs IDENTICALLY for both the PG entry here and the generic engine path (P6a) —
    // a single source of the executor-layer gate.
    let backend = PostgresBackend::new(conn);
    apply_with_lock_backend(&backend, cfg, migrations, approval, applied_by, lock_mode).await
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
    applied_by: &str,
    lock_mode: LockMode,
) -> Result<ApplyOutcome, ApplyError> {
    // Defense-in-depth approval gate (design §1.6) — refuse a destructive batch
    // without explicit approval BEFORE doing anything (not even the lock). The
    // engine has its own gate; this is the independent executor-layer check so a
    // direct caller cannot bypass it. It is dialect-agnostic (reads only
    // `flags.destructive`), so it sits in the generic core — running identically for
    // PG and the engine path (P6a moved it here from `apply_with_lock`).
    if approval != Approval::Approved
        && migrations.iter().any(|m| m.flags.destructive)
    {
        return Err(ApplyError::ApprovalRequired);
    }
    // H10: acquire the project advisory lock only when WE own it. Under
    // `AlreadyHeld` the outer `apply_declarative` already holds it for the whole
    // declarative deploy — re-acquiring here (and releasing below) would FREE the
    // lock between sub-batches (PG advisory locks are session-re-entrant, so a
    // nested unlock would pop one level), letting a concurrent deploy interleave.
    if lock_mode == LockMode::Acquire {
        backend.acquire_project_lock(&cfg.project_id).await?;
    }
    // Capture the session GUCs we will override so we can restore them on exit
    // — the executor's search_path / statement_timeout / lock_timeout must NOT
    // leak onto the (pooled / long-lived) connection after apply (H2). This runs
    // regardless of lock mode: every sub-batch is responsible for its own session
    // hygiene even when the lock is owned outside it.
    let snapshot = backend.snapshot_session().await;
    let result = apply_locked(backend, cfg, migrations, applied_by).await;
    // L1: RESET ROLE UNCONDITIONALLY — regardless of whether `snapshot_session`
    // succeeded. The non-txn path's `SET ROLE` mutates the session; if the
    // snapshot had failed we would otherwise skip `restore_session` entirely and
    // leak the migrator role onto the pooled/long-lived connection. So drop the
    // role back to admin on EVERY exit path first, then restore the GUCs if we
    // have a snapshot. (Harmless no-op when no `SET ROLE` ran.)
    backend.reset_role_best_effort().await;
    // Restore the original session settings (best-effort; logged on failure)
    // before releasing the lock. The txn path uses SET LOCAL so only the non-txn
    // path actually mutates the session, but restoring unconditionally is cheap
    // and keeps the guarantee total.
    if let Ok(snap) = &snapshot {
        if let Err(e) = backend.restore_session(snap).await {
            tracing::warn!(error = %e, "zeroship-migrate: failed to restore session GUCs after apply");
        }
    }
    // Release the lock only when WE acquired it. Under `AlreadyHeld` the outer
    // `apply_declarative` releases it once, after every sub-batch. Always release
    // on the `Acquire` path, even on error. Surface the original error first.
    if lock_mode == LockMode::AlreadyHeld {
        return result;
    }
    let unlock = backend.release_project_lock(&cfg.project_id).await;
    // Surface the apply error first if there was one; otherwise surface any
    // unlock failure. (The lock auto-releases on session end regardless.)
    match result {
        Ok(o) => unlock.map(|()| o),
        Err(e) => Err(e),
    }
}

/// Acquire the project advisory lock for an outer multi-sub-batch operation (H10).
///
/// Used by [`apply_declarative`](crate::engine::MigrationEngine::apply_declarative)
/// so it can hold the lock across the plain set + every rename's expand, passing
/// [`LockMode::AlreadyHeld`] into each sub-batch. The key is `hashtext(project_id)`
/// — exactly the key the per-batch [`apply`]/[`rollback`] paths use — so the outer
/// hold serializes against ANY concurrent deploy/rollback for the same project.
///
/// # Errors
/// [`ApplyError::Db`] if the `SELECT pg_advisory_lock(...)` fails.
pub async fn acquire_project_lock_outer(
    conn: &Client,
    project_id: &str,
) -> Result<(), ApplyError> {
    acquire_project_lock(conn, project_id).await
}

/// Release the project advisory lock taken by [`acquire_project_lock_outer`] (H10).
///
/// The companion to it; the outer caller MUST call this exactly once on every exit
/// path (success/error) so the lock is held exactly once and released exactly once
/// for the whole declarative deploy.
///
/// # Errors
/// [`ApplyError::Db`] if the `SELECT pg_advisory_unlock(...)` fails.
pub async fn release_project_lock_outer(
    conn: &Client,
    project_id: &str,
) -> Result<(), ApplyError> {
    release_project_lock(conn, project_id).await
}

/// Pre-flight over the FULL supplied set (v3 Plan E re-critic): reject malformed
/// repeatable/versioned combinations BEFORE the partition or any apply, so a
/// dropped or misrouted facet can never silently apply. Fail-closed per the
/// no-back-compat stance — these shapes are author errors, not legacy inputs.
///
/// Three rejections, each before any execution (nothing applied):
///
/// - **#4a** — a `repeatable=true` migration with a non-empty `supersedes`: a
///   repeatable cannot be a squash ([`ApplyError::RepeatableCannotSquash`]). Without
///   this, the partition routes it into the repeatable phase and its `supersedes` is
///   silently dropped (never gated).
/// - **#4c** — a `repeatable=true` migration with `down.is_some()`: a repeatable is
///   replace-style with no true reverse ([`ApplyError::RepeatableHasDown`]).
/// - **#3d** — a VERSIONED (once-only) migration whose `depends_on` names a
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
    // #3d once-only-depends-on-repeatable check.
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
            // #4a — a repeatable cannot be a squash.
            if !m.supersedes.is_empty() {
                return Err(ApplyError::RepeatableCannotSquash {
                    version: m.version.as_str().to_string(),
                });
            }
            // #4c — a repeatable must not declare a down.
            if m.down.is_some() {
                return Err(ApplyError::RepeatableHasDown {
                    version: m.version.as_str().to_string(),
                });
            }
        } else {
            // #3d — a once-only migration may not depend on a repeatable in the set.
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

    // v3 Plan E (re-critic) — PRE-FLIGHT over the FULL supplied set, before the
    // partition or any apply, rejecting malformed repeatable/versioned combinations
    // fail-closed (a dropped/misrouted facet must never silently apply). Refusing
    // here, before the partition, means the rejected shapes never reach the
    // versioned pipeline or the repeatable phase.
    check_repeatable_wellformed(migrations)?;

    // v3 Plan E — partition the supplied set into VERSIONED (run-once) and
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

    // Drift / tamper check (design §2.3 step 3): every migration in the set that
    // the journal records as net-applied must still match its recorded checksum.
    // This is the SHARED comparison — `crate::drift::check_checksum_drift` builds
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
            crate::drift::DriftError::Db(db) => ApplyError::Db(db),
            crate::drift::DriftError::Journal(j) => ApplyError::Journal(j),
            crate::drift::DriftError::Backend(b) => ApplyError::Backend(b),
        })?;
    if let Some(d) = drift_report.checksum_drift.into_iter().next() {
        return Err(ApplyError::ChecksumDrift {
            version: d.version,
            recorded: d.recorded,
            expected: d.expected,
        });
    }

    // M1: a version recorded net-applied in the journal but ABSENT from the
    // supplied set (an orphan) is surfaced, not silently ignored — it usually
    // means the bundle is missing a migration the database already has (a
    // downgrade / a dropped slice). We log a warning; correctness is unaffected
    // (we still only apply what's pending), but the operator should know.
    for orphan in &drift_report.orphan_journal {
        tracing::warn!(
            version = %orphan.version,
            project = %cfg.project_id,
            "zeroship-migrate: journal records a completed migration absent from the supplied set"
        );
    }

    // EXPAND/CONTRACT GATE (Plan 8 v1.2) — refuse a pending contract whose expand
    // is not net-applied in the journal. Run BEFORE `order_pending` so it beats
    // the generic MissingDependency with a precise error.
    check_expand_contract_gate(migrations, &completed)?;

    // Supersession (Plan 9 B): a version superseded by a net-applied squash (read
    // from the journal) OR by an in-set squash that will run this batch is SATISFIED
    // — it must not (re-)run. `compute_superseded` unions both sources; the squash
    // `S` itself is never in this set (it runs / is already applied). Computed
    // BEFORE the all-or-none gate so the gate can classify each squash's superseded
    // set against the SAME satisfied set the pending computation uses.
    let journal_superseded = backend.superseded_versions(cfg).await?;
    let superseded_owned = compute_superseded(migrations, &journal_superseded);

    // SQUASH ALL-OR-NONE GATE (Plan 9 B) — a pending squash may run its `up` only
    // when NONE of its superseded versions are SATISFIED (fresh DB). All-satisfied
    // => use squash() (record without running); partial => inconsistent. A version
    // is satisfied when it is directly net-applied (`completed`) OR covered by a
    // net-applied squash (`journal_superseded`) — the #1 fix: a chained/overlapping
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

    // Multi-engine P0 (design 2026-06-21 §2.2 L3) — run the **per-engine** line-1
    // guard through the [`MigrationGuard`] seam, NOT an `if dialect == Sqlite`
    // branch. The guard is selected for `cfg`'s dialect (which equals
    // `backend.dialect()`) via [`guard_for`], so it carries the apply's project +
    // trust profile from `cfg.guard_config()` (the trust profile lives on
    // `ExecutorConfig`, not the backend, hence `guard_for(cfg.guard_config())`
    // rather than `backend.guard()` here — same seam, config-correct):
    //   - Postgres → `PgGuard` (libpg_query deny-list) — byte-identical to the
    //     pre-seam `SqlGuard::new(cfg.guard_config())`;
    //   - SQLite → `SqliteDescriptorGuard` — the trusted descriptor-diff path
    //     (`check` returns the empty clean outcome: `libpg_query` cannot vet SQLite,
    //     the line-1 vet is the descriptor emitter at the author boundary and the
    //     line-2 defense is the backend authorizer applied per statement at apply).
    // The non-txn idempotency check still runs through the trait (`validate_non_txn`),
    // which for SQLite rejects `transaction:false` at the dialect boundary.
    let guard = crate::guard::guard_for(&cfg.guard_config());

    // FIRST PASS — static validation over EVERY pending migration BEFORE any
    // execution (H1). The guard runs per-migration inside the apply loop in the
    // original design, which means an earlier migration could commit before a
    // later one is denied (a half-applied batch). Hoisting the static checks
    // (guard deny-list + non-txn idempotency) up front makes a denial apply
    // NOTHING. (A migration failing at EXECUTION still legitimately leaves the
    // earlier ones applied — standard migration semantics; only the STATIC
    // checks are all-or-nothing.)
    for m in &pending {
        let version = m.version.as_str();
        // GUARD GATE — line-1 per engine: PG denies RCE / priv-esc / cross-tenant /
        // file / network; SQLite trusts the descriptor-diff DDL (vetted by the
        // descriptor emitter + the backend authorizer).
        guard.check(&m.up).map_err(|source| ApplyError::Guard {
            version: version.to_string(),
            source,
        })?;
        // C1/C2 — a migration taking the two-phase path must be idempotent
        // (re-runnable by crash recovery). Reject the non-idempotent form with a
        // clear error. Behind the seam: PG parses with `pg_query`; SQLite rejects
        // `transaction:false` at the dialect boundary (design §2.3/L3). The gate
        // mirrors the apply-path decision ([`uses_two_phase_path`]) so a
        // non-transactional (`InflightMarker`) backend validates EVERY migration's
        // idempotency, while PG/SQLite (`Transactional`) keep the byte-identical
        // `transaction:false`-only check.
        if uses_two_phase_path(backend, m) {
            backend.validate_non_txn(m)?;
        }
    }

    let mut outcome = ApplyOutcome {
        applied: Vec::new(),
        // Skipped = already-completed versions PLUS versions skipped because a
        // squash supersedes them (they will never run — `S` covers their effect).
        skipped: completed
            .keys()
            .map(|v| (*v).to_string())
            .chain(superseded_owned.iter().cloned())
            .collect(),
        recovered: Vec::new(),
    };

    // SECOND PASS — execute (precondition gate + apply). All static checks have
    // already passed.
    execute_pending(backend, cfg, &pending, &started, applied_by, &mut outcome).await?;

    // v3 Plan E — REPEATABLE PHASE. Runs AFTER every versioned pending migration
    // has applied (the versioned schema the repeatables' views/functions reference
    // is now present). Each repeatable re-applies iff its checksum differs from the
    // latest journaled `completed` checksum for its identity (or it was never
    // applied); an unchanged checksum is skipped.
    apply_repeatables(backend, cfg, &repeatables, applied_by, &mut outcome).await?;

    Ok(outcome)
}

/// Whether a migration must take the **two-phase non-transactional** apply path
/// (`started → completed` inflight marker + crash recovery) rather than the atomic
/// `BEGIN; <up>; INSERT journal; COMMIT` path (multi-engine abstraction C2).
///
/// Two independent reasons route a migration onto the two-phase path; either is
/// sufficient (an OR):
///
/// 1. **Per-migration `transaction:false`** — the migration itself contains a
///    statement that cannot run inside a transaction (Postgres `CREATE INDEX
///    CONCURRENTLY` / `ALTER TYPE … ADD VALUE`). This is the pre-existing,
///    per-migration opt-in, unchanged.
/// 2. **A non-transactional backend** — the backend reports
///    [`JournalAtomicity::InflightMarker`] (a MySQL-class engine whose DDL
///    auto-commits per statement, so NO migration on it can be wrapped in one
///    transaction with its journal row). Such a backend routes ALL its migrations
///    through the two-phase path.
///
/// Postgres and SQLite both report [`JournalAtomicity::Transactional`], so clause
/// (2) NEVER fires for them and this reduces to exactly the pre-seam
/// `!m.flags.transactional` test — their apply behavior is byte-identical. The
/// `InflightMarker` clause is the seam by which a MySQL-class backend plugs in.
fn uses_two_phase_path<B: MigrationBackend>(backend: &B, m: &Migration) -> bool {
    !m.flags.transactional || backend.journal_atomicity() == JournalAtomicity::InflightMarker
}

/// The execute pass (design §2.3 step 6): for each pending migration, evaluate
/// its preconditions (v3 Plan D) read-only under the advisory lock, then apply
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
    // Versions SKIPPED this run because an `OnUnmet::Skip` precondition was unmet
    // (v3 Plan D). A skipped migration is NOT applied and NOT journaled — it stays
    // pending for the next deploy. Its dependents must also not run this batch: a
    // dependent's depended-on object does not exist (the dep did not run), so we
    // transitively skip any pending migration whose `depends_on` includes a
    // skipped version. `pending` is in topological order, so a dependent is always
    // visited after the dep it would skip on.
    let mut skipped_this_run: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for &m in pending {
        let version = m.version.as_str();
        let had_inflight = started.contains_key(version);

        // v3 Plan D: a dependent of a Skip'd (still-pending) migration cannot run
        // this batch — the object its `up` needs was never created. Transitively
        // skip it (and record it so ITS dependents skip too).
        let dep_skipped = m
            .depends_on
            .iter()
            .any(|d| skipped_this_run.contains(d.as_str()));
        // Evaluate preconditions (read-only) BEFORE the `up`, under the advisory
        // lock so the checked state is stable. Skipped-by-dependency short-circuits
        // evaluation (we already know this won't run).
        let skip = dep_skipped
            || matches!(
                backend.evaluate_preconditions(cfg, m).await?,
                PreconditionVerdict::Skip
            );
        if skip {
            skipped_this_run.insert(version);
            outcome.skipped.push(version.to_string());
            continue;
        }

        // Plan 9 B (fresh-DB squash): a squash whose `up` RUNS this batch records its
        // supersession edges so future pending computations know `S` satisfies
        // `[v1..vN]`. The all-or-none gate already proved NONE of the superseded
        // versions were satisfied, so this is the fresh path. #2 fix: the edges are
        // written in the SAME transaction that journals `S`'s `completed` row (not a
        // separate post-commit statement) — a crash between would otherwise leave `S`
        // net-applied with edges missing, re-entering `v1..vN` into pending and
        // re-running them on top of `S`'s schema (double-apply).
        let sups: Vec<&str> = m.supersedes.iter().map(MigrationId::as_str).collect();
        // Versioned once-only path: `'squash'` for a fresh-path squash (non-empty
        // supersedes), else the ordinary `'apply'`. Never `'repeatable'` here — a
        // repeatable never reaches the versioned pipeline (it is partitioned out).
        let kind = if sups.is_empty() { "apply" } else { "squash" };

        if uses_two_phase_path(backend, m) {
            // Two-phase `started → completed` path. Reached when the migration is
            // `transaction:false`, OR the backend is non-transactional
            // (`JournalAtomicity::InflightMarker`, MySQL-class) so no migration can
            // be wrapped in one transaction with its journal row. PG/SQLite report
            // `Transactional`, so for them this fires only on `transaction:false` —
            // byte-identical to the pre-seam path.
            //
            // Mandatory timeouts + pinned search_path on the session (the non-txn
            // path has no transaction to SET LOCAL within). Restored on exit by
            // `apply` so nothing leaks (H2). Per-migration timeout applied (H3).
            backend.configure_session_non_txn(cfg, m).await?;
            let recovered = backend
                .apply_up_non_transactional(cfg, m, applied_by, had_inflight, &sups)
                .await?;
            if recovered {
                outcome.recovered.push(version.to_string());
            }
        } else {
            // Atomic path: the `<up>` and its journal row commit together in one
            // `BEGIN … COMMIT`. The default for a transactional migration on a
            // transactional backend (PG/SQLite).
            backend
                .apply_up_transactional(cfg, m, applied_by, &sups, kind)
                .await?;
        }
        outcome.applied.push(version.to_string());
    }

    Ok(())
}

/// The REPEATABLE PHASE (v3 Plan E — Flyway `R__` / Liquibase `runOnChange`).
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
///    preconditions (v3 Plan D) read-only under the lock, then run `up` under the
///    least-privilege migrator role inside a transaction and append a NEW
///    `completed` event carrying the new checksum (via [`apply_transactional`]).
///
/// A repeatable is ALWAYS transactional (replace-style `CREATE OR REPLACE …`,
/// `down: None`), so it never takes the non-txn two-phase path. Its `supersedes`
/// is always empty, so the `completed` event is stamped the ordinary `kind='apply'`.
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

    // Order repeatables among themselves by `depends_on` topo (version-tiebroken),
    // honoring deps on versioned migrations as pre-satisfied (they already ran).
    let ordered = order_repeatables(repeatables)?;

    let guard = SqlGuard::new(cfg.guard_config());

    // FIRST PASS — guard EVERY repeatable's `up` before any execution, mirroring
    // the versioned all-up-front static gate: a denial applies NOTHING.
    for m in &ordered {
        guard.check(&m.up).map_err(|source| ApplyError::Guard {
            version: m.version.as_str().to_string(),
            source,
        })?;
    }

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

        // Preconditions (v3 Plan D): a repeatable may gate its re-apply too. An
        // unmet Skip leaves it unchanged this deploy (re-evaluated next time); an
        // unmet/inevaluable Halt fails closed.
        if matches!(
            backend.evaluate_preconditions(cfg, m).await?,
            PreconditionVerdict::Skip
        ) {
            outcome.skipped.push(version.to_string());
            continue;
        }

        // Replace-style: always transactional, never superseding. `apply_transactional`
        // runs `up` under the migrator role and appends a fresh `completed` event with
        // the NEW checksum — exactly the re-apply record the next deploy compares against.
        // Stamped `kind='repeatable'` (v3 Plan E re-critic): the journaled kind is the
        // tamper anchor, so the drift exemption can distinguish a genuine repeatable
        // re-run from a flipped once-only, and `latest_completed_checksums` reads only
        // `kind='repeatable'` rows for the re-run oracle.
        backend
            .apply_up_transactional(cfg, m, applied_by, &[], "repeatable")
            .await?;
        outcome.applied.push(version.to_string());
    }

    Ok(())
}

/// Topologically order the repeatables among THEMSELVES (v3 Plan E), honoring
/// `depends_on` edges between repeatables, version-tiebroken for determinism.
///
/// A repeatable's `depends_on` may name a VERSIONED migration (e.g. a view depends
/// on a table); that dependency is pre-satisfied — the versioned phase already ran
/// — so it imposes no ordering here and is simply NOT treated as an edge among the
/// repeatables (it is also not required to be in the repeatable set). Only an edge
/// to ANOTHER REPEATABLE in this set constrains order. With no inter-repeatable
/// edges this degrades to pure version order.
///
/// # Errors
/// - [`ApplyError::DependencyCycle`] — the inter-repeatable edges form a cycle.
fn order_repeatables<'a>(
    repeatables: &[&'a Migration],
) -> Result<Vec<&'a Migration>, ApplyError> {
    use std::collections::{BTreeMap, BTreeSet, HashMap};

    let rep_versions: BTreeSet<&str> =
        repeatables.iter().map(|m| m.version.as_str()).collect();
    let by_version: HashMap<&str, &Migration> =
        repeatables.iter().map(|m| (m.version.as_str(), *m)).collect();

    // in-degree over the repeatable subgraph; adj[dep] = repeatables after `dep`.
    let mut indeg: BTreeMap<&str, usize> =
        repeatables.iter().map(|m| (m.version.as_str(), 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in repeatables {
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            // Only edges to ANOTHER repeatable in this set constrain order; a dep
            // on a versioned migration is pre-satisfied (it already ran) and a dep
            // outside the set entirely imposes no repeatable ordering.
            if rep_versions.contains(dep_v) {
                adj.entry(dep_v).or_default().push(m.version.as_str());
                *indeg.get_mut(m.version.as_str()).expect("repeatable node") += 1;
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

/// The verdict of evaluating a migration's preconditions (v3 Plan D).
///
/// `pub` because it is the return type of
/// [`MigrationBackend::evaluate_preconditions`](crate::backend::MigrationBackend::evaluate_preconditions)
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
/// [`crate::precondition::evaluate_all`] — the **Postgres** leaf reached only via
/// [`MigrationBackend::evaluate_preconditions`](crate::backend::MigrationBackend::evaluate_preconditions)
/// (multi-engine abstraction C3). The generic apply body calls the backend method
/// (`backend.evaluate_preconditions(cfg, m)`); it holds no `&Client` and runs no
/// `pg_query` / `information_schema` query directly.
///
/// The EXPAND/CONTRACT gate (Plan 8 v1.2). A `phase: Contract` online migration
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
    use crate::migration::OnlinePhase;
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
                expand: "<none declared: a contract must declare an expand dependency>"
                    .to_string(),
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

/// Order the pending migrations honoring `depends_on` (design §4 cross-slice
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
/// `pub(crate)` so the read-only status API ([`crate::status`]) computes its
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
    // `satisfied` (Plan 9 squash) is the set of versions made redundant by a
    // SUPERSESSION — a version `v_i` superseded by a squash `S` that is net-applied
    // OR being applied in this batch. Such a version is treated like a completed
    // one: it is EXCLUDED from pending (its `up` must never run — `S` covers it),
    // and it counts as a pre-met dependency (a later migration `depends_on v_i` is
    // satisfied by `S`). `S` itself is NOT in `satisfied` (it is pending and runs).
    let pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| {
            !completed.contains_key(m.version.as_str())
                && !satisfied.contains(m.version.as_str())
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

/// The SHARED canonical ordering core (M2): a deterministic, **version-tiebroken
/// topological sort** of `nodes` over their `depends_on` edges. Both the apply
/// path ([`order_pending`]) and the integrity manifest ([`canonical_set_order`],
/// folded by [`crate::manifest::compute_manifest`]) order through this one
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

/// The CANONICAL EXECUTED ORDER of a FULL supplied set (M2), used by
/// [`crate::manifest::compute_manifest`] to fold the manifest over the order the
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
///   manifest (also independently caught by C1's checksum fold).
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

/// Compute the set of versions made redundant by a SUPERSESSION (Plan 9 squash),
/// for the supplied set + the journal's net state.
///
/// A version `v_i` is satisfied-by-supersession when a squash `S` (with `v_i ∈
/// S.supersedes`) is either:
/// - **net-applied in the journal** (`journal_superseded`, read via
///   [`crate::journal::superseded_versions`]); or
/// - **present in the supplied set** — whether already net-applied OR pending. A
///   pending `S` will run its `up` THIS batch, so its superseded versions must not
///   also run (`order_pending` excludes them); an already-applied `S` is also
///   covered by `journal_superseded`, so adding the in-set edges is at worst
///   redundant.
///
/// The squash `S` itself is never added to the result (it is not superseded by
/// itself; it runs or is already applied). Used by both [`apply_locked`] and the
/// read-only [`crate::status`] so their "pending" views agree.
pub(crate) fn compute_superseded(
    migrations: &[Migration],
    journal_superseded: &[String],
) -> std::collections::HashSet<String> {
    let mut out: std::collections::HashSet<String> =
        journal_superseded.iter().cloned().collect();
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
/// version (Plan 9 sub-feature B). A version may be collapsed by at most one
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

/// Validate the squash all-or-none rule (Plan 9 sub-feature B) for every PENDING
/// squash in the set, BEFORE any execution.
///
/// A squash `S` (`supersedes = [v1..vN]`) that is about to RUN its `up` (it is in
/// the set and NOT net-applied) requires that NONE of `[v1..vN]` are SATISFIED —
/// the fresh-DB path, where `S.up` builds the schema and the superseded versions
/// are skipped. If ALL of `[v1..vN]` are satisfied, `S.up` would re-create existing
/// objects (double-apply): the correct path is [`crate::squash`] (record the
/// supersession WITHOUT running `up`), so apply refuses with
/// [`ApplyError::SquashAlreadyApplied`]. A PARTIAL set (some but not all satisfied)
/// is an inconsistent state refused with [`ApplyError::SquashPartialOverlap`].
///
/// `satisfied` is the SAME set the pending computation uses: a version is satisfied
/// when it is directly net-applied (`completed`) OR covered by a net-applied squash
/// (`journal::superseded_versions`). This is the #1 fix: a version covered by an
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
///   is fully satisfied (use [`crate::squash`] instead of apply).
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

/// Order the migrations being rolled back in **REVERSE TOPOLOGICAL** order of
/// `depends_on` — the transpose of [`order_pending`].
///
/// Invariant (design §5): a migration's `down` must run **before** the `down` of
/// any migration it `depends_on`. The depended-on object (e.g. a parent table)
/// must survive until every dependent that references it (e.g. a child with an FK)
/// has been torn down. This is exactly the reverse of apply's topo order, so we
/// compute the forward topo order (Kahn, lowest-version-ready-first — identical to
/// `order_pending`, restricted to the rollback set) and reverse it. The result is
/// stable + version-tiebroken: with no `depends_on` edges it degrades to strict
/// **reverse-version** order (preserving the pre-#1 behavior for the version-aligned
/// case).
///
/// The graph is restricted to the set being rolled back: a `depends_on` pointing
/// OUTSIDE the set (a migration that stays applied, or one already rolled back)
/// imposes no ordering on the batch — it is not torn down here.
///
/// A `depends_on` pointing to a version selected for rollback but absent from the
/// supplied set is already rejected by the caller's `MissingFromSet` pre-flight
/// (every selected version is resolved to a `Migration` before this runs), so the
/// graph is always buildable; the only fail-closed case here is a cycle.
///
/// # Errors
/// - [`RollbackError::DependencyCycle`] — the selected migrations' `depends_on`
///   edges form a cycle (impossible if apply enforced acyclicity; defended anyway).
fn order_rollback<'a>(
    selected: &[&'a Migration],
) -> Result<Vec<&'a Migration>, RollbackError> {
    use std::collections::{BTreeMap, HashSet};

    // Versions in the rollback set; edges to deps OUTSIDE the set are ignored
    // (those objects are not being torn down in this batch).
    let in_set: HashSet<&str> = selected.iter().map(|m| m.version.as_str()).collect();
    let by_version: HashMap<&str, &Migration> =
        selected.iter().map(|m| (m.version.as_str(), *m)).collect();

    // Forward topo over the rollback subgraph: edge `dep -> m` for each `dep` in
    // `m.depends_on` that is ALSO in the set. `adj[dep]` = members that must apply
    // AFTER `dep`; `indeg[m]` = # of in-set deps `m` still waits on.
    let mut indeg: BTreeMap<&str, usize> =
        selected.iter().map(|m| (m.version.as_str(), 0usize)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in selected {
        for dep in &m.depends_on {
            let dep_v = dep.as_str();
            if in_set.contains(dep_v) {
                adj.entry(dep_v).or_default().push(m.version.as_str());
                *indeg.get_mut(m.version.as_str()).expect("in-set node") += 1;
            }
        }
    }

    // Kahn with a version-ordered ready set (BTreeSet → lowest version first), so
    // the forward order is deterministic + version-tiebroken, mirroring
    // `order_pending`.
    let mut ready: std::collections::BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, &d)| d == 0)
        .map(|(&v, _)| v)
        .collect();
    let mut topo: Vec<&Migration> = Vec::with_capacity(selected.len());
    while let Some(&v) = ready.iter().next() {
        ready.remove(v);
        topo.push(by_version[v]);
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

    if topo.len() != selected.len() {
        let mut cyclic: Vec<&str> = indeg
            .iter()
            .filter(|(_, &d)| d > 0)
            .map(|(&v, _)| v)
            .collect();
        cyclic.sort_unstable();
        return Err(RollbackError::DependencyCycle(cyclic.join(", ")));
    }

    // Reverse the topo order → reverse-topological: dependents first, depended-on
    // last (each migration's down runs before the downs of its `depends_on`).
    topo.reverse();
    Ok(topo)
}

/// Transactional apply (design §2.3): `BEGIN; <up>; INSERT journal; COMMIT`.
/// DDL + journal are atomic — a failure rolls back leaving no partial DDL and
/// no journal row.
///
/// `kind` is the journaled `kind` to stamp on the `completed` event: `'apply'` for
/// an ordinary once-only migration, `'squash'` for a fresh-path squash (non-empty
/// `supersedes`), or `'repeatable'` for a re-applied repeatable (v3 Plan E). The
/// caller passes it explicitly — the journaled kind is the tamper anchor, so it is
/// never inferred from anything the migration set supplies at apply time. A debug
/// assertion ties `'squash'` ⇔ non-empty `supersedes`.
pub(crate) async fn apply_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    supersedes: &[&str],
    kind: &str,
) -> Result<(), ApplyError> {
    let started = Instant::now();
    // `transaction()` needs `&mut Client`; the apply flow owns the connection,
    // so we take a short-lived mutable borrow via a raw pointer-free path:
    // callers pass `&Client`, so we cannot call `transaction()` directly. We
    // instead drive BEGIN/COMMIT/ROLLBACK explicitly over the shared `&Client`
    // (still one physical session, still atomic) — this avoids requiring
    // `&mut` plumbing through the whole apply loop.
    conn.batch_execute("BEGIN").await?;

    // Pin search_path + the mandatory timeouts (per-migration override applied)
    // with SET LOCAL so they are scoped to THIS transaction and vanish at
    // COMMIT/ROLLBACK — nothing leaks onto the session (H2 / H3). This runs as
    // the admin (always permitted); the role switch is applied separately around
    // the `<up>` only.
    if let Err(e) = conn.batch_execute(&set_local_session_sql(cfg, m)).await {
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(ApplyError::Db(e));
    }

    // C1: drop to the least-privilege migrator role for the duration of the
    // `<up>` ONLY. `SET LOCAL ROLE` is transaction-scoped, so the role switch is
    // confined to this txn; we explicitly `RESET ROLE` (below) before the journal
    // INSERT so the journal write runs as the admin — the migrator's journal
    // grant is revoked (role.rs), so it could not write the journal even if it
    // tried. The up's DDL is thereby confined to line-2 privileges (design §1.3)
    // while the journal stays unforgeable by the migration.
    if let Some(set_role) = set_local_role_sql(cfg) {
        if let Err(e) = conn.batch_execute(&set_role).await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(ApplyError::Db(e));
        }
    }

    // Run the migration's up SQL (as the migrator, if a role is configured).
    if let Err(e) = conn.batch_execute(&m.up).await {
        // Roll back; report the failure. No journal row was written.
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a migration error (M4)");
        }
        return Err(ApplyError::MigrationFailed {
            version: m.version.as_str().to_string(),
            source: e,
        });
    }

    // C1: RESET ROLE back to the admin — still INSIDE the transaction — so the
    // journal INSERT below runs as the admin (the migrator cannot write the
    // journal). `RESET ROLE` mid-transaction is supported and does not end the
    // txn, so atomicity of `<up>` + journal is preserved.
    if cfg.pg.migrator_role.is_some() {
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(ApplyError::Db(e));
        }
    }

    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Journal the completed row in the SAME transaction, as the admin. The `kind`
    // is passed by the caller (the journaled kind is the tamper anchor — never
    // inferred from the supplied set): `'apply'` for an ordinary migration,
    // `'squash'` for a fresh-path squash (non-empty `supersedes`, so its
    // supersession edges are honored by `journal::superseded_versions` — #4 filters
    // on `kind='squash'`), `'repeatable'` for a re-applied repeatable (v3 Plan E).
    debug_assert_eq!(
        kind == "squash",
        !supersedes.is_empty(),
        "kind='squash' iff supersedes is non-empty"
    );
    let meta = format!("\"{}\"", cfg.pg.meta_schema.replace('"', "\"\""));
    if let Err(e) = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations
                     (version, name, checksum, applied_by, exec_ms, phase, outcome, kind)
                 VALUES ($1, $2, $3, $4, $5, 'completed', 'success', $6)"
            ),
            &[
                &m.version.as_str(),
                &m.name,
                &m.checksum.as_str(),
                &applied_by,
                &exec_ms,
                &kind,
            ],
        )
        .await
    {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a journal-insert error (M4)");
        }
        return Err(ApplyError::Journal(JournalError::Db(e)));
    }

    // #2 fix: write the fresh-DB squash supersession edges in the SAME transaction
    // as the `completed` row above (admin). Edges-last-but-same-txn — so `S`'s
    // net-applied state and its full edge set commit atomically. A failure here
    // rolls back the entire apply (no `completed` row, no edges).
    if let Err(e) = insert_supersedes_edges(conn, cfg, m.version.as_str(), supersedes).await {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a supersedes-edge error (M4)");
        }
        return Err(ApplyError::Journal(e));
    }

    conn.batch_execute("COMMIT").await?;
    Ok(())
}

/// Insert the `S → v_i` supersession edges for a squash whose `up` RAN this batch
/// (Plan 9 B fresh path). Each `conn.execute` participates in whatever transaction
/// the caller has open — the txn apply path calls this INSIDE its `BEGIN…COMMIT`
/// so the edges are atomic with `S`'s `completed` row (#2). No-op for a non-squash
/// (`supersedes` empty). Admin write (the migrator has no meta-schema grant).
async fn insert_supersedes_edges(
    conn: &Client,
    cfg: &ExecutorConfig,
    squash_version: &str,
    supersedes: &[&str],
) -> Result<(), JournalError> {
    let meta = format!("\"{}\"", cfg.pg.meta_schema.replace('"', "\"\""));
    for sup in supersedes {
        conn.execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations_supersedes
                     (squash_version, superseded_version)
                 VALUES ($1, $2)"
            ),
            &[&squash_version, sup],
        )
        .await
        .map_err(JournalError::Db)?;
    }
    Ok(())
}

/// Non-transactional apply (design §2.3 / §2.4): two-phase with a `started`
/// marker, plus the idempotent recovery path.
///
/// Returns `true` if this was a recovery (a prior `started` marker existed).
pub(crate) async fn apply_non_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
    had_inflight: bool,
    supersedes: &[&str],
) -> Result<bool, ApplyError> {
    let version = m.version.as_str();

    // Journal / inflight I/O runs as the ADMIN (C1): the migrator's grant on the
    // meta schema is revoked, so `record_started` / `recover_non_transactional`
    // (which clears the marker) / `record_completed` must NOT run under
    // `SET ROLE migrator`. Only the `<up>` (and the recovery `DROP INDEX`, which
    // the migrator owns) runs as the migrator.
    if had_inflight {
        // Recovery path: a prior run wrote `started` then crashed before
        // `completed`. Inspect the real state and make the apply idempotent. The
        // INVALID-index DROP inside runs as the migrator (it owns the index); the
        // inflight `clear` runs as admin. See `recover_non_transactional`.
        recover_non_transactional(conn, cfg, m).await?;
    } else {
        journal::record_started(conn, cfg, version, &m.name, m.checksum.as_str(), applied_by)
            .await?;
    }

    let started = Instant::now();
    // C1: bracket the `<up>` with SET ROLE / RESET ROLE so the migration's DDL
    // runs under line-2 confinement, but the journal writes above/below run as
    // admin. `RESET ROLE` runs on ALL exit paths (including the error path) so
    // the role never leaks onto the session even if the `<up>` fails — and
    // `apply`'s `restore_session` is an unconditional backstop (L1).
    if let Some(role) = &cfg.pg.migrator_role {
        conn.batch_execute(&format!("SET ROLE \"{}\"", role.replace('"', "\"\"")))
            .await?;
    }
    let up_result = conn.batch_execute(&m.up).await;
    if cfg.pg.migrator_role.is_some() {
        // RESET ROLE regardless of the up's success, so the journal writes below
        // run as admin and no role leaks onto the session.
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            // If RESET ROLE itself fails, surface it (apply's restore_session is
            // the L1 backstop). Prefer surfacing the up's error if it failed.
            if up_result.is_ok() {
                return Err(ApplyError::Db(e));
            }
        }
    }
    up_result.map_err(|e| ApplyError::MigrationFailed {
        version: version.to_string(),
        source: e,
    })?;
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Phase 2: immutable completed row + clear the marker (as admin). #2 fix: for a
    // fresh-DB squash, the supersession edges must commit ATOMICALLY with the
    // `completed` row, else a crash between leaves `S` net-applied with edges missing
    // → `v1..vN` re-enter pending and re-run on top of `S` (double-apply). The non-txn
    // `<up>` already committed (that is the nature of a non-txn migration), but the
    // JOURNAL writes — the completed row, the inflight clear, and the edges — are
    // bracketed in one transaction so they land together. No surrounding txn for a
    // non-squash: keep the original single-statement finalize.
    if supersedes.is_empty() {
        journal::record_completed(
            conn,
            cfg,
            journal::CompletedRecord {
                version,
                name: &m.name,
                checksum: m.checksum.as_str(),
                applied_by,
                exec_ms,
                kind: "apply",
            },
        )
        .await?;
    } else {
        conn.batch_execute("BEGIN").await?;
        let finalize = async {
            // A fresh-path squash is stamped `kind='squash'` so its edges are honored
            // by `superseded_versions` (#4 filters on `kind='squash'`).
            journal::record_completed(
                conn,
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
            insert_supersedes_edges(conn, cfg, version, supersedes).await
        }
        .await;
        if let Err(e) = finalize {
            if let Err(rb) = conn.batch_execute("ROLLBACK").await {
                tracing::warn!(error = %rb, version = %version, "zeroship-migrate: ROLLBACK failed after a non-txn squash finalize error");
            }
            return Err(ApplyError::Journal(e));
        }
        conn.batch_execute("COMMIT").await?;
    }

    Ok(had_inflight)
}

/// Idempotent recovery for a crashed non-transactional migration (design §2.4,
/// C1/C2).
///
/// A `started` marker with no `completed` row means the prior attempt may have
/// (a) failed mid-DDL, or (b) **succeeded then crashed** before recording
/// `completed`. The old recovery reasoned per-op-type and blindly re-ran a
/// possibly-non-idempotent `<up>`, which permanently wedged case (b)
/// (`CREATE INDEX CONCURRENTLY` → `already exists`; `ALTER TYPE … ADD VALUE` →
/// `label already exists`).
///
/// The robust model: non-txn `up`s are **required to be idempotent**
/// (enforced up-front by [`validate_non_txn_idempotent`] — every non-txn
/// statement uses `IF NOT EXISTS`), so recovery does not need per-op reasoning.
/// It performs ONE cleanup that `IF NOT EXISTS` cannot itself do — dropping the
/// `INVALID` index residue of an interrupted CONCURRENTLY build (an INVALID
/// index satisfies `IF NOT EXISTS`, so it would otherwise never be rebuilt) —
/// then clears the marker. The caller then **re-runs the idempotent `<up>`**,
/// which is safe in both case (a) and case (b).
///
/// Runs as the **admin** (C1): it is called BEFORE the `<up>`'s `SET ROLE` and
/// clears the inflight marker (`clear_inflight`), which is meta-schema I/O the
/// migrator has no grant for. The admin owns the meta schema and is privileged
/// over the project schema, so the project-schema `DROP INDEX` succeeds as admin
/// without needing the migrator role.
async fn recover_non_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
) -> Result<(), ApplyError> {
    // Drop the INVALID residue of an interrupted CONCURRENTLY build — but ONLY
    // the index(es) this migration's `up` names (v1.x scope fix). An INVALID
    // index satisfies `IF NOT EXISTS`, so the caller's re-run of `<up>` would
    // never rebuild it; we must drop it first. We parse the `CREATE INDEX … name`
    // out of the `up` and drop *that* name (if it is currently invalid), rather
    // than every invalid index in the schema.
    //
    // Why scoped: an OUT-OF-BAND invalid index (a manual CONCURRENTLY build the
    // operator is running elsewhere in the project schema) must NOT be collateral
    // damage of recovering an unrelated migration. The per-project advisory lock
    // serializes the engine's own applies, but it does not stop a human's manual
    // session, so scoping is the correct fix.
    for idx in index_names_in_up(&m.up) {
        // Only drop it if it is currently INVALID — a valid index named here means
        // the prior attempt actually succeeded (case (b): completed then crashed
        // before journaling), and the re-run of the idempotent `up`'s
        // `IF NOT EXISTS` will correctly no-op over it. Dropping a valid index
        // would needlessly rebuild it.
        let is_invalid: bool = conn
            .query_one(
                "SELECT EXISTS (
                     SELECT 1
                       FROM pg_index x
                       JOIN pg_class c ON c.oid = x.indexrelid
                       JOIN pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = $1 AND c.relname = $2 AND x.indisvalid = false
                 ) AS invalid",
                &[&cfg.project_schema, &idx],
            )
            .await?
            .get("invalid");
        if is_invalid {
            let stmt = format!(
                "DROP INDEX IF EXISTS \"{}\".\"{}\"",
                cfg.project_schema.replace('"', "\"\""),
                idx.replace('"', "\"\""),
            );
            conn.batch_execute(&stmt).await?;
        }
    }

    // Clear the stale marker; the caller re-runs `<up>` and re-records.
    journal::clear_inflight(conn, cfg, m.version.as_str()).await?;
    Ok(())
}

/// Parse the index name(s) created by `CREATE INDEX … name … ON …` statements in
/// a migration's `up`, via the real Postgres parser (so syntax we cannot parse
/// simply yields no names — recovery then drops nothing, which is the safe
/// default). Unnamed `CREATE INDEX` (no explicit name) is skipped: Postgres
/// derives the name, and recovery cannot target a name it does not know — the
/// non-txn idempotency rule already forbids unnamed `CONCURRENTLY` indirectly
/// (the author always emits a name; raw SQL must too to be re-runnable).
fn index_names_in_up(up: &str) -> Vec<String> {
    let Ok(parsed) = pg_query::parse(up) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for raw_stmt in &parsed.protobuf.stmts {
        if let Some(NodeEnum::IndexStmt(idx)) =
            raw_stmt.stmt.as_ref().and_then(|s| s.node.as_ref())
        {
            if !idx.idxname.is_empty() {
                names.push(idx.idxname.clone());
            }
        }
    }
    names
}

// ===========================================================================
// Rollback (Plan 5) — apply `down` SQL in reverse to a target.
// ===========================================================================

/// One resolved step of a rollback plan (internal): either run a migration's
/// `down`, or force-skip an irreversible (`down: None`) migration.
enum RollbackStep<'a> {
    /// Run this migration's `down` (it is `Some`) and journal a `rolled_back`.
    Down(&'a Migration),
    /// Skip this irreversible migration (force path); it stays applied.
    SkipIrreversible(&'a Migration),
}

/// How far [`rollback`] should unwind the applied migrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RollbackTarget {
    /// Roll back every net-applied migration whose version is **strictly after**
    /// this one — i.e. unwind *down to* (and keeping) this version. The target
    /// itself is NOT rolled back.
    ToVersion(MigrationId),
    /// Roll back the `n` most-recently-applied migrations (the `n` highest
    /// net-applied versions). `Steps(0)` is a no-op; `Steps(k)` with `k` ≥ the
    /// applied count behaves like [`RollbackTarget::All`].
    Steps(usize),
    /// Roll back **all** net-applied migrations.
    All,
}

/// A complete rollback request.
///
/// How far to unwind ([`RollbackTarget`]) plus the irreversible-handling
/// [`RollbackOptions`]. Bundled so [`rollback`] and
/// [`MigrationEngine::rollback`](crate::engine::MigrationEngine::rollback) carry
/// one parameter rather than two.
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

/// Options controlling [`rollback`] over irreversible (`down: None`) migrations.
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
    /// acknowledgement (design §5).
    pub backup_acknowledged: bool,
}

/// What [`rollback`] did.
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
    /// was given (design §5). Empty unless forcing.
    pub skipped_irreversible: Vec<String>,
}

impl RollbackOutcome {
    /// True if nothing was rolled back (and nothing force-skipped).
    #[must_use]
    pub const fn is_noop(&self) -> bool {
        self.rolled_back.is_empty() && self.skipped_irreversible.is_empty()
    }
}

/// Error from [`rollback`].
#[derive(Debug, thiserror::Error)]
pub enum RollbackError {
    /// A database error outside a guarded/journaled step.
    #[error("db error: {0}")]
    Db(#[from] compio_postgres::Error),
    /// A journal operation failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// A **dialect-neutral** backend error (non-Postgres
    /// [`MigrationBackend`](crate::backend::MigrationBackend) impls). See
    /// [`ApplyError::Backend`]. The Postgres impl never constructs this arm.
    #[error("backend error: {0}")]
    Backend(String),
    /// Rollback was requested with [`Approval::None`]. A `down` is inherently
    /// destructive (it tears structure down), so rollback ALWAYS requires
    /// [`Approval::Approved`]. This is the executor's OWN defense-in-depth gate,
    /// independent of (and additional to) the engine's gate
    /// ([`crate::engine::MigrationEngine::rollback`]), so a caller driving
    /// [`rollback`] directly cannot bypass approval. Nothing was rolled back.
    #[error("rollback requires Approval::Approved (every down is destructive) but it was not given")]
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
    /// classifier apply uses, [`crate::classify`]) and refuse the WHOLE rollback
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
    /// The SQL guard denied a `down`'s SQL — the down is SQL too and goes through
    /// the SAME defenses as an up (design §5). The whole rollback aborts before
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
    #[error("checksum drift on {version}: journal has {recorded}, set has {expected}")]
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
        /// The DB error from the failed `down`.
        #[source]
        source: compio_postgres::Error,
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
    /// **SQLite, P5 additive-only.** The migration's `down` requires the 12-step
    /// table REBUILD to reverse (a column TYPE-change reversal, a constraint
    /// add/drop, or any `ALTER` SQLite cannot perform natively). P5 implements only
    /// the ADDITIVE reversals SQLite ≥ 3.35 supports natively — `DROP TABLE` /
    /// `DROP COLUMN` / `DROP INDEX` / `RENAME`. A rebuild-needing `down` is REFUSED
    /// here (not half-rebuilt): the rebuild path is P3b. Nothing was rolled back.
    #[error(
        "migration {version} has a SQLite `down` requiring the 12-step table rebuild ({reason}); \
         the rebuild path is P3b (not yet built). P5 reverses only the additive operations SQLite \
         supports natively (DROP TABLE/COLUMN/INDEX, RENAME). Author a compensating migration, or \
         wait for the P3b rebuild phase."
    )]
    SqliteRebuildRequired {
        /// The migration whose `down` needs a table rebuild.
        version: String,
        /// What specifically requires the rebuild.
        reason: String,
    },
}

/// Roll back applied migrations to a [`RollbackTarget`] (design §5).
///
/// Applies the `down` SQL of net-applied-and-not-rolled-back migrations **after**
/// the target, in **reverse topological order of `depends_on`** (the transpose of
/// apply: a migration's `down` runs before the downs of everything it `depends_on`,
/// so a depended-on object survives until every dependent is torn down). This
/// degrades to reverse-version order when there are no `depends_on` edges. Each
/// `down` runs under the project advisory lock.
///
/// **The `down` is privileged SQL too — it gets the SAME defenses as an `up`:**
/// every selected `down` is run through the [`SqlGuard`] up-front (a denial aborts
/// the whole rollback before any down runs), and each `down` executes under the
/// least-privilege `migrator` role (`SET LOCAL ROLE` for the txn path), exactly
/// like the up path. A malicious/buggy `down` (cross-schema, RCE) is just as
/// dangerous as an up.
///
/// **Append-only journaling:** each `down` + its `rolled_back` journal append run
/// in ONE transaction (txn path: `BEGIN; SET LOCAL ROLE migrator; <down>; RESET
/// ROLE; INSERT rolled_back as admin; COMMIT`). The completed row is never
/// deleted; a rolled-back version becomes pending again (re-appliable).
///
/// **Irreversible (`down: None`):** by default rollback **refuses** to cross such
/// a migration ([`RollbackError::Irreversible`]) and directs the operator to
/// roll-forward. With [`RollbackOptions::force`] + [`RollbackOptions::backup_acknowledged`]
/// it proceeds, **skipping** the irreversible step (recorded in
/// [`RollbackOutcome::skipped_irreversible`]; the down cannot be journaled because
/// it never ran, so no `rolled_back` event is written for it — it stays applied).
///
/// `approval` is the caller's approval decision. A `down` is inherently
/// destructive, so rollback ALWAYS requires [`Approval::Approved`]: with
/// [`Approval::None`] this returns [`RollbackError::ApprovalRequired`] before any
/// `down` executes. This is the executor's OWN defense-in-depth gate (design
/// §1.6), independent of (and additional to) the engine's gate, so a caller
/// driving [`rollback`] directly cannot bypass approval.
///
/// **Full-history contract:** `migrations` must carry the FULL historical `down`
/// set, not just the tail. Every net-applied version (per the journal) must be
/// present in `migrations` so its `down` SQL is available — the journal records
/// no SQL, so the bundle is the only source of `down`. A net-applied version
/// absent from the set is refused with [`RollbackError::MissingFromSet`] (nothing
/// is rolled back; see `rollback_of_applied_version_absent_from_set_errors` in
/// `tests/rollback_pg.rs`). The corollary: a migration's `down` must remain
/// available FOREVER — a rolled-back-then-re-shipped migration's `down` cannot be
/// dropped, or rolling it back later becomes permanently impossible. The
/// engine / control-plane must always supply the complete applied history.
///
/// # Errors
/// See [`RollbackError`]. All pre-flight checks (approval, unknown target,
/// missing-from-set, guard denial, irreversible-without-force, checksum drift)
/// abort before any `down` executes.
pub async fn rollback(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    request: RollbackRequest,
    approval: Approval,
    applied_by: &str,
) -> Result<RollbackOutcome, RollbackError> {
    // Defense-in-depth approval gate (design §1.6) — every `down` is destructive,
    // so rollback ALWAYS requires explicit approval. Refuse BEFORE doing anything
    // (not even the lock). Independent of the engine's own gate.
    if approval != Approval::Approved {
        return Err(RollbackError::ApprovalRequired);
    }
    // Drive rollback through the dialect seam (P1: Postgres only). Keeps the
    // public `&Client` signature; the body below is generic over
    // [`MigrationBackend`].
    let backend = PostgresBackend::new(conn);
    rollback_with_backend(&backend, cfg, migrations, request, applied_by).await
}

/// The lock + session-hygiene shell around [`rollback_locked`], generic over the
/// dialect seam. Byte-identical to the pre-seam PG rollback flow, now routed
/// through [`MigrationBackend`].
async fn rollback_with_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    request: RollbackRequest,
    applied_by: &str,
) -> Result<RollbackOutcome, RollbackError> {
    // Acquire the project advisory lock — serializes against concurrent
    // apply/rollback for the same project, exactly like `apply`. The seam maps a
    // lock failure to `ApplyError::Db`; preserve rollback's `RollbackError::Db`
    // surface by converting.
    backend
        .acquire_project_lock(&cfg.project_id)
        .await
        .map_err(rollback_err_from_apply)?;
    let snapshot = backend.snapshot_session().await.ok();
    let result = rollback_locked(backend, cfg, migrations, request, applied_by).await;
    // Mirror apply's exit discipline: unconditional RESET ROLE, then restore GUCs,
    // then release the lock — so the migrator role / executor GUCs never leak.
    backend.reset_role_best_effort().await;
    if let Some(snap) = &snapshot {
        if let Err(e) = backend.restore_session(snap).await {
            tracing::warn!(error = %e, "zeroship-migrate: failed to restore session GUCs after rollback");
        }
    }
    let unlock = backend
        .release_project_lock(&cfg.project_id)
        .await
        .map_err(rollback_err_from_apply);
    match result {
        Ok(o) => unlock.map(|()| o),
        Err(e) => Err(e),
    }
}

/// Map a seam [`ApplyError`] from the lock primitives back to [`RollbackError`].
/// The lock methods are shared across apply + rollback and return `ApplyError`;
/// the pre-seam rollback raised `RollbackError::Db` on a lock failure, so we
/// re-thread the inner `compio_postgres::Error` to preserve that exact surface.
fn rollback_err_from_apply(e: ApplyError) -> RollbackError {
    match e {
        ApplyError::Db(db) => RollbackError::Db(db),
        ApplyError::Journal(j) => RollbackError::Journal(j),
        // The project-lock seam primitives (`acquire`/`release_project_lock`) only
        // ever fail with `ApplyError::Db` (a `SELECT pg_advisory_(un)lock` driver
        // error). Any other variant from those calls is structurally impossible;
        // surface it loudly rather than silently mis-mapping it.
        other => unreachable!(
            "project-lock primitives only return ApplyError::Db, got: {other:?}"
        ),
    }
}

/// Resolve which net-applied migrations to roll back and in what order — the pure
/// (non-async) core of [`rollback_locked`]. Selects per [`RollbackTarget`], resolves
/// each version to its `Migration` (checking `MissingFromSet` + checksum drift),
/// reverse-topologically orders the downs by `depends_on` ([`order_rollback`], #1),
/// then runs the all-up-front pre-flight (irreversible-without-force, guard,
/// non-transactional-down) and returns the executable [`RollbackStep`] plan in
/// execution order.
///
/// **Full-history contract (`MissingFromSet`):** `migrations` MUST carry the FULL
/// historical `down` set — every version the journal records as net-applied must
/// be resolvable here to its `Migration`, or rollback of that version is impossible
/// ([`RollbackError::MissingFromSet`], asserted by
/// `rollback_of_applied_version_absent_from_set_errors` in `tests/rollback_pg.rs`).
/// A migration's `down` must therefore remain available FOREVER — even after that
/// migration was rolled back and re-shipped: the caller (engine / control-plane)
/// must always supply the complete applied history, never just the tail. We
/// resolve against the supplied set (not the journal, which records no SQL) because
/// the journal is deliberately SQL-free; the bundle is the source of truth for
/// `down` SQL.
///
/// # Errors
/// [`RollbackError::UnknownTarget`], [`RollbackError::MissingFromSet`],
/// [`RollbackError::ChecksumDrift`], [`RollbackError::DependencyCycle`],
/// [`RollbackError::Irreversible`], or [`RollbackError::Guard`] — all before any
/// `down` runs.
fn build_rollback_plan<'a>(
    net_applied: &[&'a AppliedEntry],
    migrations: &'a [Migration],
    target: &RollbackTarget,
    opts: RollbackOptions,
    cfg: &ExecutorConfig,
) -> Result<Vec<RollbackStep<'a>>, RollbackError> {
    // Index the supplied set by version (source of the `down` SQL + checksum).
    let by_version: HashMap<&str, &Migration> =
        migrations.iter().map(|m| (m.version.as_str(), m)).collect();

    // Which net-applied versions to roll back (ascending order for now).
    let selected: Vec<&AppliedEntry> = match target {
        RollbackTarget::All => net_applied.to_vec(),
        RollbackTarget::Steps(n) => {
            let n = (*n).min(net_applied.len());
            net_applied[net_applied.len() - n..].to_vec()
        }
        RollbackTarget::ToVersion(v) => {
            let tv = v.as_str();
            if !net_applied.iter().any(|e| e.version == tv) {
                return Err(RollbackError::UnknownTarget {
                    version: tv.to_string(),
                });
            }
            net_applied
                .iter()
                .copied()
                .filter(|e| e.version.as_str() > tv)
                .collect()
        }
    };

    // Resolve each selected version to its `Migration`. MissingFromSet + checksum
    // drift are version-order-stable, so check them here BEFORE ordering — the
    // reverse-topo sort needs the `Migration`s in hand.
    let mut selected_migs: Vec<&Migration> = Vec::with_capacity(selected.len());
    for entry in &selected {
        let v = entry.version.as_str();
        let Some(m) = by_version.get(v).copied() else {
            return Err(RollbackError::MissingFromSet {
                version: v.to_string(),
            });
        };
        if entry.checksum != m.checksum.as_str() {
            return Err(RollbackError::ChecksumDrift {
                version: v.to_string(),
                recorded: entry.checksum.clone(),
                expected: m.checksum.as_str().to_string(),
            });
        }
        selected_migs.push(m);
    }

    // Order the downs in REVERSE TOPOLOGICAL order of `depends_on` (#1): a
    // migration's `down` runs BEFORE the downs of everything it depends_on, so a
    // depended-on object (parent table) survives until every dependent (child FK)
    // is gone. Degrades to reverse-version order with no edges (regression-guarded).
    let ordered = order_rollback(&selected_migs)?;

    // ---- PRE-FLIGHT (all-up-front, before ANY down runs) -------------------
    // 1. Irreversible (`down: None`): refuse unless force + backup_acknowledged.
    // 2. The `down` SQL passes the guard (down is SQL too — same defenses).
    // (MissingFromSet + checksum already verified above.)
    let force_ok = opts.force && opts.backup_acknowledged;
    let guard = SqlGuard::new(cfg.guard_config());
    let mut plan: Vec<RollbackStep> = Vec::with_capacity(ordered.len());
    for m in ordered {
        let v = m.version.as_str();
        match &m.down {
            None => {
                if !force_ok {
                    return Err(RollbackError::Irreversible {
                        version: v.to_string(),
                        name: m.name.clone(),
                    });
                }
                plan.push(RollbackStep::SkipIrreversible(m));
            }
            Some(down) => {
                // The down is privileged SQL — run it through the guard up-front.
                guard.check(down).map_err(|source| RollbackError::Guard {
                    version: v.to_string(),
                    source,
                })?;
                // #5 — the rollback executor has ONLY a transactional down path
                // (each down runs inside BEGIN…COMMIT under SET LOCAL ROLE
                // migrator), so a `down` containing a statement that cannot run in
                // a transaction would die LATE with PG 25001. Detect it up-front
                // with the SAME classifier apply uses (`crate::classify`, whose
                // `StatementClass.non_transactional` flags CONCURRENTLY / ALTER
                // TYPE ADD VALUE / VACUUM) and refuse the whole rollback before any
                // down runs. The safe path is roll-forward.
                if let Some(reason) = non_transactional_down_reason(down) {
                    return Err(RollbackError::NonTransactionalDown {
                        version: v.to_string(),
                        reason,
                    });
                }
                plan.push(RollbackStep::Down(m));
            }
        }
    }

    // Dangling-dependency pre-flight: refuse if any KEPT migration depends_on
    // something being rolled back (would leave it referencing a torn-down object).
    // Covers BOTH keep mechanisms — force-skip (#2) and version-threshold (HIGH).
    check_kept_dependencies(&plan, net_applied, &selected, &by_version)?;

    Ok(plan)
}

/// Dangling-dependency pre-flight for rollback: a KEPT (still-applied) migration
/// must not `depends_on` (transitively) any migration being rolled back, or its
/// FK/reference would dangle the moment the dependency's `down` runs. There are
/// TWO ways a net-applied migration is kept:
///
/// 1. **force-skip** — an irreversible (`down: None`) migration kept via
///    `force`+`backup_acknowledged` (a [`RollbackStep::SkipIrreversible`] in
///    `plan`). Conflict → [`RollbackError::ForceSkipDependencyConflict`].
/// 2. **version-threshold** — a net-applied migration BELOW a
///    [`RollbackTarget::ToVersion`]/[`RollbackTarget::Steps`] cut (in `net_applied`
///    but not `selected`). Selection is purely version-based; `depends_on` only
///    REORDERS the selected set, so a below-threshold dependent is silently kept.
///    Conflict → [`RollbackError::KeptDependsOnRolledBack`].
///
/// Both reuse the same cycle-safe transitive BFS ([`first_dependency_in_set`]).
/// Refused before any `down` runs; the safe path is roll-forward (or roll back far
/// enough to include the kept migration). No-op when nothing is being rolled back.
///
/// # Errors
/// [`RollbackError::ForceSkipDependencyConflict`] or
/// [`RollbackError::KeptDependsOnRolledBack`].
fn check_kept_dependencies(
    plan: &[RollbackStep<'_>],
    net_applied: &[&AppliedEntry],
    selected: &[&AppliedEntry],
    by_version: &HashMap<&str, &Migration>,
) -> Result<(), RollbackError> {
    let rolling: std::collections::HashSet<&str> = plan
        .iter()
        .filter_map(|s| match s {
            RollbackStep::Down(m) => Some(m.version.as_str()),
            RollbackStep::SkipIrreversible(_) => None,
        })
        .collect();
    if rolling.is_empty() {
        return Ok(());
    }

    // #2 — force-skip keep-path: a force-skipped irreversible migration depends on
    // something being torn down beneath it.
    for step in plan {
        if let RollbackStep::SkipIrreversible(kept) = step {
            if let Some(dep) = first_dependency_in_set(kept, &rolling, by_version) {
                return Err(RollbackError::ForceSkipDependencyConflict {
                    kept: kept.version.as_str().to_string(),
                    dependency: dep,
                });
            }
        }
    }

    // HIGH — version-threshold keep-path: a net-applied-but-NOT-selected migration
    // (kept below the cut) depends on something being rolled back. (#2 above only
    // covers the force-skip keep-path; this covers the version-threshold one.)
    let selected_versions: std::collections::HashSet<&str> =
        selected.iter().map(|e| e.version.as_str()).collect();
    for entry in net_applied {
        let kept_v = entry.version.as_str();
        if selected_versions.contains(kept_v) {
            continue; // being rolled back, not kept
        }
        // Resolve the kept migration in the supplied set to read its `depends_on`.
        // If absent we cannot inspect its edges — that is the `MissingFromSet`
        // full-history-contract concern, orthogonal to this guard, and only the
        // SELECTED set is required to be resolvable.
        let Some(kept) = by_version.get(kept_v).copied() else {
            continue;
        };
        if let Some(dep) = first_dependency_in_set(kept, &rolling, by_version) {
            return Err(RollbackError::KeptDependsOnRolledBack {
                kept: kept_v.to_string(),
                dependency: dep,
            });
        }
    }

    Ok(())
}

/// Walk `m`'s `depends_on` transitively (over `by_version`) and return the first
/// dependency version found in `target`. Used by the #2 force-skip guard to detect
/// whether a kept (irreversible) migration depends on anything being torn down.
/// Deterministic (sorted-version frontier) and cycle-safe via a visited set.
fn first_dependency_in_set(
    m: &Migration,
    target: &std::collections::HashSet<&str>,
    by_version: &HashMap<&str, &Migration>,
) -> Option<String> {
    use std::collections::{BTreeSet, HashSet};
    let mut visited: HashSet<&str> = HashSet::new();
    let mut frontier: BTreeSet<&str> = m.depends_on.iter().map(MigrationId::as_str).collect();
    while let Some(&dep_v) = frontier.iter().next() {
        frontier.remove(dep_v);
        if !visited.insert(dep_v) {
            continue;
        }
        if target.contains(dep_v) {
            return Some(dep_v.to_string());
        }
        // Recurse into the dependency's own deps (if it is in the supplied set).
        if let Some(dep_m) = by_version.get(dep_v) {
            for d in &dep_m.depends_on {
                let dv = d.as_str();
                if !visited.contains(dv) {
                    frontier.insert(dv);
                }
            }
        }
    }
    None
}

/// The rollback body, run while holding the project advisory lock. Generic over
/// the dialect seam: selection + reverse-topo ordering + pre-flight are pure; the
/// journal reads and each confined `down` go through `backend`.
async fn rollback_locked<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    request: RollbackRequest,
    applied_by: &str,
) -> Result<RollbackOutcome, RollbackError> {
    let RollbackRequest { target, options: opts } = request;
    backend.ensure_journal(cfg).await?;

    // Net-applied versions (latest event is `completed`), with their recorded
    // checksums, in ascending version order.
    let journal_rows = backend.applied(cfg).await?;
    let mut net_applied: Vec<&AppliedEntry> = journal_rows
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .collect();
    net_applied.sort_by(|a, b| a.version.cmp(&b.version));

    // Resolve selection + reverse-topo ordering + all-up-front pre-flight into the
    // executable plan (pure / non-async — see `build_rollback_plan`).
    let plan = build_rollback_plan(&net_applied, migrations, &target, opts, cfg)?;

    // ---- EXECUTE -----------------------------------------------------------
    let mut outcome = RollbackOutcome {
        rolled_back: Vec::new(),
        skipped_irreversible: Vec::new(),
    };
    for step in plan {
        match step {
            RollbackStep::SkipIrreversible(m) => {
                tracing::warn!(
                    version = %m.version.as_str(),
                    project = %cfg.project_id,
                    "zeroship-migrate: force-skipping irreversible migration during rollback (down: None)"
                );
                outcome
                    .skipped_irreversible
                    .push(m.version.as_str().to_string());
            }
            RollbackStep::Down(m) => {
                backend.rollback_one_transactional(cfg, m, applied_by).await?;
                outcome.rolled_back.push(m.version.as_str().to_string());
            }
        }
    }
    Ok(outcome)
}

/// Roll back ONE migration transactionally: `BEGIN; SET LOCAL …; SET LOCAL ROLE
/// migrator; <down>; RESET ROLE; INSERT rolled_back (as admin); COMMIT`.
///
/// Atomic: the `down` + its `rolled_back` journal append commit together, so a
/// crash leaves either both (rolled back + recorded) or neither. The `down` runs
/// under the migrator role; the journal append runs as admin (the migrator has no
/// meta grant — C1), exactly mirroring [`apply_transactional`].
pub(crate) async fn rollback_one_transactional(
    conn: &Client,
    cfg: &ExecutorConfig,
    m: &Migration,
    applied_by: &str,
) -> Result<(), RollbackError> {
    let down = m
        .down
        .as_deref()
        .expect("rollback_one_transactional is only called for RollbackStep::Down (down is Some)");
    let started = Instant::now();
    conn.batch_execute("BEGIN").await?;

    // Pin search_path + mandatory timeouts (SET LOCAL — vanish at COMMIT/ROLLBACK).
    if let Err(e) = conn.batch_execute(&set_local_session_sql(cfg, m)).await {
        let _ = conn.batch_execute("ROLLBACK").await;
        return Err(RollbackError::Db(e));
    }
    // Drop to the migrator role for the `<down>` ONLY (line-2 confinement). RESET
    // ROLE before the journal append so the admin writes the rolled_back event.
    if let Some(set_role) = set_local_role_sql(cfg) {
        if let Err(e) = conn.batch_execute(&set_role).await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(RollbackError::Db(e));
        }
    }
    // Run the `<down>` as the migrator.
    if let Err(e) = conn.batch_execute(down).await {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a down error");
        }
        return Err(RollbackError::DownFailed {
            version: m.version.as_str().to_string(),
            source: e,
        });
    }
    // RESET ROLE back to admin — still inside the txn — so the journal append runs
    // as the admin (the migrator cannot write the journal).
    if cfg.pg.migrator_role.is_some() {
        if let Err(e) = conn.batch_execute("RESET ROLE").await {
            let _ = conn.batch_execute("ROLLBACK").await;
            return Err(RollbackError::Db(e));
        }
    }
    let exec_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);

    // Append the immutable `rolled_back` event in the SAME transaction, as admin.
    let meta = format!("\"{}\"", cfg.pg.meta_schema.replace('"', "\"\""));
    if let Err(e) = conn
        .execute(
            &format!(
                "INSERT INTO {meta}.schema_migrations_rolled_back
                     (version, name, checksum, rolled_back_by, exec_ms)
                 VALUES ($1, $2, $3, $4, $5)"
            ),
            &[
                &m.version.as_str(),
                &m.name,
                &m.checksum.as_str(),
                &applied_by,
                &exec_ms,
            ],
        )
        .await
    {
        if let Err(rb) = conn.batch_execute("ROLLBACK").await {
            tracing::warn!(error = %rb, version = %m.version.as_str(), "zeroship-migrate: ROLLBACK failed after a rolled_back-insert error");
        }
        return Err(RollbackError::Journal(JournalError::Db(e)));
    }

    conn.batch_execute("COMMIT").await?;
    Ok(())
}

#[cfg(test)]
mod pg_confinement_shape_tests {
    //! Pins the M2 confinement refactor: the **PG** apply leaf still emits its
    //! `SET LOCAL search_path` / `SET LOCAL ROLE` / `SET LOCAL statement_timeout`
    //! + `lock_timeout` bracket from the [`PgConfinement`](crate::db::PgConfinement)
    //! block (now grouped under `cfg.pg`, not flat on the neutral config), and a
    //! default (SQLite-shaped construction reuses this same `new`) carries the
    //! INERT PG confinement — never PG role/cross-schema confinement of its own.
    use super::*;
    use crate::migration::{Checksum, MigrationFlags, MigrationId};

    fn trivial_migration() -> Migration {
        let flags = MigrationFlags::default();
        let version = MigrationId::generate();
        let checksum = Checksum::of(&crate::migration::ChecksumInput {
            up: "CREATE TABLE t (id int)",
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up: "CREATE TABLE t (id int)".into(),
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        }
    }

    /// PG confinement bracket is emitted from the grouped `cfg.pg` block,
    /// byte-identically to the pre-M2 flat shape: search_path = project schema
    /// (+ extension schema), and the mandatory timeouts in ms.
    #[test]
    fn pg_confinement_bracket_is_emitted_from_the_pg_block() {
        let cfg = ExecutorConfig::new("prj_x", "proj_x").with_migrator_role("migrator_proj_x");
        let m = trivial_migration();

        let session = set_local_session_sql(&cfg, &m);
        // search_path is the project schema, then the `public` extension schema
        // (the `new()` default) — pinned for confined resolution.
        assert_eq!(
            session,
            "SET LOCAL search_path TO \"proj_x\", \"public\"; \
             SET LOCAL statement_timeout = 60000; \
             SET LOCAL lock_timeout = 30000;",
            "PG SET LOCAL session bracket must come from cfg.pg byte-identically"
        );

        // The migrator role bracket comes from cfg.pg.migrator_role.
        let role = set_local_role_sql(&cfg).expect("migrator role set");
        assert_eq!(role, "SET LOCAL ROLE \"migrator_proj_x\"");
    }

    /// A default-constructed config (the SHAPE the SQLite engine builds via
    /// `ExecutorConfig::new(app_id, app_id)`) carries NO migrator role — its
    /// PG confinement is inert; SQLite confines via its runtime authorizer
    /// mode-flip, never these PG params (M2).
    #[test]
    fn sqlite_shaped_config_carries_no_pg_role_confinement() {
        // Exactly what crates/plugin-db sqlite_engine.rs constructs.
        let cfg = ExecutorConfig::new("app_test", "app_test");
        assert!(
            cfg.pg.migrator_role.is_none(),
            "a SQLite-shaped config must carry no PG migrator role (SET ROLE) — \
             it confines via the runtime authorizer mode-flip, not the PG bracket"
        );
        // And the PG-only role bracket is therefore absent.
        assert!(
            set_local_role_sql(&cfg).is_none(),
            "no SET LOCAL ROLE is emitted when the PG confinement carries no role"
        );
        // The neutral identity fields ARE populated (engine-agnostic).
        assert_eq!(cfg.project_id, "app_test");
        assert_eq!(cfg.project_schema, "app_test");
    }
}

#[cfg(test)]
mod non_txn_idempotency_tests {
    use super::*;
    use crate::migration::{Checksum, MigrationFlags, MigrationId};

    /// Build a non-transactional migration whose `up` is `sql`.
    fn nontxn(sql: &str) -> Migration {
        let flags = MigrationFlags {
            transactional: false,
            ..MigrationFlags::default()
        };
        let version = MigrationId::generate();
        let checksum = Checksum::of(&crate::migration::ChecksumInput {
            up: sql,
            down: None,
            flags: &flags,
            owner_app: "app_test",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version,
            name: "n".into(),
            up: sql.into(),
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        }
    }

    // REGRESSION (nontxn-dml-recovery-double-apply): a bare INSERT/UPDATE/
    // DELETE/MERGE/TRUNCATE on the non-txn two-phase path would be re-run
    // VERBATIM by crash recovery and double-apply on a success-then-crash.
    // Validation must reject every bare-DML non-txn `up`. Pre-fix these passed
    // (only CONCURRENTLY / ALTER TYPE ADD VALUE were fenced).
    #[test]
    fn bare_dml_on_non_txn_path_is_rejected() {
        for sql in [
            "INSERT INTO t (id) VALUES (1)",
            "UPDATE t SET x = 1 WHERE id = 1",
            "DELETE FROM t WHERE id = 1",
            "TRUNCATE t",
            "MERGE INTO t USING s ON t.id = s.id WHEN MATCHED THEN DO NOTHING",
        ] {
            let m = nontxn(sql);
            let err = validate_non_txn_idempotent(&m)
                .expect_err(&format!("bare DML must be rejected on non-txn path: {sql}"));
            assert!(
                matches!(err, ApplyError::NonIdempotentNonTxn { .. }),
                "expected NonIdempotentNonTxn for `{sql}`, got {err:?}"
            );
        }
    }

    // A DML statement mixed in among DDL on the non-txn path is still caught.
    #[test]
    fn dml_mixed_with_ddl_on_non_txn_path_is_rejected() {
        let m = nontxn(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (x);\n\
             INSERT INTO t (id) VALUES (1);",
        );
        let err = validate_non_txn_idempotent(&m).expect_err("DML among DDL must be rejected");
        assert!(matches!(err, ApplyError::NonIdempotentNonTxn { .. }), "got {err:?}");
    }

    // The legitimate non-txn ops (idempotent CONCURRENTLY / ADD VALUE IF NOT
    // EXISTS, naturally-rerunnable DROP INDEX CONCURRENTLY / VACUUM) still pass —
    // the DML fence must not over-reject inherently-non-txn DDL.
    #[test]
    fn idempotent_non_txn_ddl_still_passes() {
        for sql in [
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS i ON t (x)",
            "DROP INDEX CONCURRENTLY IF EXISTS i",
            "ALTER TYPE mood ADD VALUE IF NOT EXISTS 'excited'",
            "VACUUM ANALYZE t",
        ] {
            let m = nontxn(sql);
            assert!(
                validate_non_txn_idempotent(&m).is_ok(),
                "idempotent non-txn op must pass: {sql}"
            );
        }
    }
}

#[cfg(test)]
mod journal_atomicity_seam_tests {
    //! The `JournalAtomicity` seam (multi-engine abstraction C2): prove the
    //! atomic-vs-two-phase apply decision ([`uses_two_phase_path`]) actually
    //! consults [`MigrationBackend::journal_atomicity`], and that PG/SQLite's
    //! `Transactional` reduces it to the byte-identical `!transactional` test.
    //!
    //! Uses a minimal test backend that overrides ONLY `journal_atomicity`; every
    //! other trait method is `unimplemented!()` because the decision function never
    //! touches them — that is the whole point (the capability is consulted in
    //! isolation, no DB needed).
    use super::*;
    use crate::backend::{JournalAtomicity, SessionSnapshot};
    use crate::baseline::{BaselineError, BaselineOutcome};
    use crate::drift::{DriftError, SchemaSnapshot};
    use crate::journal::JournalError;
    use crate::migration::{Checksum, MigrationFlags, MigrationId};
    use zeroship_schema::query::SqlDialect;

    /// A backend that reports a configurable [`JournalAtomicity`] and panics on any
    /// other method. The decision function only calls `journal_atomicity()`, so the
    /// `unimplemented!()` stubs are never reached.
    struct AtomicityProbe {
        atomicity: JournalAtomicity,
    }

    impl MigrationBackend for AtomicityProbe {
        fn dialect(&self) -> SqlDialect {
            SqlDialect::Postgres
        }
        fn guard(&self) -> &dyn crate::guard::MigrationGuard {
            unimplemented!("seam probe: guard not consulted by uses_two_phase_path")
        }
        fn journal_atomicity(&self) -> JournalAtomicity {
            self.atomicity
        }
        async fn acquire_project_lock(&self, _: &str) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn release_project_lock(&self, _: &str) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn snapshot_session(&self) -> Result<SessionSnapshot, ApplyError> {
            unimplemented!()
        }
        async fn restore_session(&self, _: &SessionSnapshot) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn reset_role_best_effort(&self) {
            unimplemented!()
        }
        async fn apply_up_transactional(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
            _: &str,
            _: &[&str],
            _: &str,
        ) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn configure_session_non_txn(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
        ) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn apply_up_non_transactional(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
            _: &str,
            _: bool,
            _: &[&str],
        ) -> Result<bool, ApplyError> {
            unimplemented!()
        }
        async fn rollback_one_transactional(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
            _: &str,
        ) -> Result<(), RollbackError> {
            unimplemented!()
        }
        fn validate_non_txn(&self, _: &Migration) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn ensure_journal(&self, _: &ExecutorConfig) -> Result<(), JournalError> {
            unimplemented!()
        }
        async fn applied(&self, _: &ExecutorConfig) -> Result<Vec<AppliedEntry>, JournalError> {
            unimplemented!()
        }
        async fn superseded_versions(
            &self,
            _: &ExecutorConfig,
        ) -> Result<Vec<String>, JournalError> {
            unimplemented!()
        }
        async fn latest_completed_checksums(
            &self,
            _: &ExecutorConfig,
        ) -> Result<HashMap<String, String>, JournalError> {
            unimplemented!()
        }
        async fn check_checksum_drift(
            &self,
            _: &ExecutorConfig,
            _: &[Migration],
        ) -> Result<crate::drift::ChecksumDriftReport, DriftError> {
            unimplemented!()
        }
        async fn snapshot_schema(&self, _: &ExecutorConfig) -> Result<SchemaSnapshot, DriftError> {
            unimplemented!()
        }
        async fn evaluate_preconditions(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
        ) -> Result<PreconditionVerdict, ApplyError> {
            unimplemented!()
        }
        async fn record_squash(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
            _: &str,
            _: &[&str],
        ) -> Result<(), ApplyError> {
            unimplemented!()
        }
        async fn rebuild_one(
            &self,
            _: &crate::backend_sqlite::SqliteRebuildSpec,
            _: &Migration,
            _: &str,
        ) -> Result<(), ApplyError> {
            unimplemented!()
        }
        fn online(&self) -> Option<&dyn crate::expand_contract::OnlineSchemaChange> {
            unimplemented!()
        }
        fn shadow(&self) -> Option<&dyn crate::shadow::ShadowDryRun> {
            unimplemented!()
        }
        async fn baseline_one(
            &self,
            _: &ExecutorConfig,
            _: &Migration,
            _: &str,
        ) -> Result<BaselineOutcome, BaselineError> {
            unimplemented!()
        }
    }

    /// Build a migration with the given `transactional` flag.
    fn mig(transactional: bool) -> Migration {
        let flags = MigrationFlags {
            transactional,
            ..MigrationFlags::default()
        };
        let up = "CREATE TABLE t ()";
        let checksum = Checksum::of(&crate::migration::ChecksumInput {
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
            name: "n".into(),
            up: up.into(),
            down: None,
            checksum,
            flags,
            owner_app: "app_test".into(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        }
    }

    // A `Transactional` backend (PG/SQLite) takes the ATOMIC path for an ordinary
    // `transaction:true` migration — the OR-clause never fires. Byte-identical to
    // the pre-seam `!m.flags.transactional` test.
    #[test]
    fn transactional_backend_txn_migration_uses_atomic_path() {
        let b = AtomicityProbe {
            atomicity: JournalAtomicity::Transactional,
        };
        assert!(
            !uses_two_phase_path(&b, &mig(true)),
            "Transactional + transaction:true must use the atomic path"
        );
    }

    // A `Transactional` backend still routes a per-migration `transaction:false`
    // through the two-phase path (the pre-existing PG CONCURRENTLY behavior).
    #[test]
    fn transactional_backend_non_txn_migration_uses_two_phase() {
        let b = AtomicityProbe {
            atomicity: JournalAtomicity::Transactional,
        };
        assert!(
            uses_two_phase_path(&b, &mig(false)),
            "Transactional + transaction:false must still use the two-phase path"
        );
    }

    // The seam: an `InflightMarker` backend (MySQL-class) routes EVERY migration —
    // even a `transaction:true` one — through the two-phase path. This proves the
    // capability is consulted: only `journal_atomicity()` differs between this and
    // the atomic-path case above.
    #[test]
    fn inflight_marker_backend_routes_txn_migration_through_two_phase() {
        let b = AtomicityProbe {
            atomicity: JournalAtomicity::InflightMarker,
        };
        assert!(
            uses_two_phase_path(&b, &mig(true)),
            "InflightMarker backend must route even a transaction:true migration through two-phase"
        );
        assert!(
            uses_two_phase_path(&b, &mig(false)),
            "InflightMarker backend routes transaction:false through two-phase as well"
        );
    }
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use crate::journal::Phase;
    use crate::migration::{Checksum, MigrationFlags, MigrationId};
    use std::collections::HashMap;

    fn m(version: MigrationId, depends_on: Vec<MigrationId>) -> Migration {
        let up = format!("CREATE TABLE t_{}()", version.as_str());
        let flags = MigrationFlags::default();
        let checksum = Checksum::of(&crate::migration::ChecksumInput {
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
        }
    }

    fn pos(ordered: &[&Migration], v: &str) -> usize {
        ordered.iter().position(|x| x.version.as_str() == v).expect("present")
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
        let ordered = order_pending(&set, &completed, &std::collections::HashSet::new()).expect("order");
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
        assert!(later.as_str() > earlier.as_str(), "later must sort after earlier");
        // earlier depends_on later; later depends_on nothing.
        let set = vec![
            m(earlier.clone(), vec![later.clone()]),
            m(later.clone(), vec![]),
        ];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let ordered = order_pending(&set, &completed, &std::collections::HashSet::new()).expect("order");
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
        let set = vec![
            m(a.clone(), vec![b.clone()]),
            m(b.clone(), vec![a.clone()]),
        ];
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
        let set = vec![m(a.clone(), vec![ghost.clone()])];
        let completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        let err = order_pending(&set, &completed, &std::collections::HashSet::new()).unwrap_err();
        assert!(matches!(err, ApplyError::MissingDependency { .. }), "got {err:?}");
    }

    #[test]
    fn dependency_already_in_journal_is_pre_satisfied() {
        // A dep that is already completed (in the journal, not in the pending set)
        // resolves fine and imposes no batch ordering.
        let done = MigrationId::generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let pend = MigrationId::generate();
        let entry = AppliedEntry {
            version: done.as_str().to_string(),
            checksum: String::new(),
            phase: Phase::Completed,
            kind: Some(journal::JournaledKind::Apply),
        };
        let mut completed: HashMap<&str, &AppliedEntry> = HashMap::new();
        completed.insert(done.as_str(), &entry);
        // Only `pend` is in the supplied set (depends on the completed `done`).
        let set = vec![m(pend.clone(), vec![done.clone()])];
        let ordered = order_pending(&set, &completed, &std::collections::HashSet::new()).expect("order");
        let vs: Vec<&str> = ordered.iter().map(|x| x.version.as_str()).collect();
        assert_eq!(vs, vec![pend.as_str()], "only the pending one is ordered");
    }
}

// ===========================================================================
// Track A — the Trusted profile applies arbitrary SQL on a REAL Postgres.
//
// These MUST be in-crate: `ExecutorConfig::trusted` + `OperatorCapability::for_test`
// are `pub(crate)` (the external boundary is pinned by `tests/trybuild_*`, T8), so
// an integration test (a separate crate) could not even construct a Trusted config.
//
// They run the FULL `executor::apply` path under a Trusted `ExecutorConfig` and
// prove (a) SQL the Confined guard hard-denies APPLIES, and (b) a destructive op
// still carries the approval flag (so the CLI `--yes` gate holds).
// ===========================================================================
#[cfg(test)]
#[allow(clippy::future_not_send)] // compio single-thread runtime; the stack is !Send by design.
mod trusted_apply_pg {
    use super::*;
    use crate::guard::OperatorCapability;
    use crate::journal;
    use crate::loader::migration_id_for_version;
    use crate::migration::{Checksum, ChecksumInput, MigrationFlags};

    const DEFAULT_DSN: &str =
        "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

    fn dsn() -> String {
        std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
    }

    async fn pg() -> Client {
        crate::db::connect(&dsn()).await.expect("connect :5440")
    }

    fn token() -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{pid}_{nanos}_{n}")
    }

    /// A Trusted `ExecutorConfig` over a unique project/meta schema. Minted via
    /// the `for_test` operator-token seam (the only in-crate path; un-nameable
    /// externally). Trusted runs as the connecting (admin) role: NO migrator role.
    fn trusted_cfg(tok: &str) -> ExecutorConfig {
        let cap = OperatorCapability::for_test();
        let mut c = ExecutorConfig::trusted(&cap, format!("prj_{tok}"), format!("proj_{tok}"));
        c.pg.meta_schema = format!("meta_{tok}");
        c.pg.statement_timeout = std::time::Duration::from_secs(30);
        c.pg.lock_timeout = std::time::Duration::from_secs(10);
        c
    }

    fn mig(version: u64, name: &str, up: &str, destructive: bool) -> Migration {
        let flags = MigrationFlags {
            destructive,
            requires_approval: destructive,
            ..MigrationFlags::default()
        };
        let mut m = Migration {
            version: migration_id_for_version(version),
            name: name.to_string(),
            up: up.to_string(),
            down: None,
            checksum: Checksum::of(&ChecksumInput {
                up: "",
                down: None,
                flags: &MigrationFlags::default(),
                owner_app: "",
                depends_on: &[],
                supersedes: &[],
                preconditions: &[],
            }),
            flags,
            owner_app: "operator".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        };
        m.recompute_checksum();
        m
    }

    async fn role_exists(conn: &Client, role: &str) -> bool {
        !conn
            .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
            .await
            .expect("query role")
            .is_empty()
    }

    async fn table_exists(conn: &Client, schema: &str, table: &str) -> bool {
        !conn
            .query(
                "SELECT 1 FROM information_schema.tables WHERE table_schema=$1 AND table_name=$2",
                &[&schema, &table],
            )
            .await
            .expect("query table")
            .is_empty()
    }

    /// `trusted_applies_sql_the_confined_guard_denies` — under a Trusted
    /// `ExecutorConfig`, statements the Confined guard HARD-DENIES apply cleanly on
    /// a real cluster: a `CREATE ROLE` (privilege management) AND a cross-schema
    /// `CREATE TABLE <other_schema>.t` (a schema OUTSIDE the project schema). Both
    /// applying proves the deny-list AND the cross-schema confinement are OFF under
    /// Trusted. (We deliberately do NOT use `COPY … PROGRAM` — CREATE ROLE +
    /// cross-schema DDL prove guard-off without running a shell.)
    #[compio::test]
    async fn trusted_applies_sql_the_confined_guard_denies() {
        let conn = pg().await;
        let tok = token();
        let cfg = trusted_cfg(&tok);
        let role = format!("zsmig_trusted_{tok}");
        let other_schema = format!("other_{tok}");

        // Project schema + journal. NO migrator role (Trusted = connecting role).
        conn.batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
        journal::ensure_journal(&conn, &cfg).await.expect("journal");

        // Sanity: a Confined guard hard-denies BOTH of these (so the apply proof
        // is meaningful), but the Trusted guard the executor uses does not.
        let confined = SqlGuard::new(crate::guard::GuardConfig::confined(&cfg.project_schema));
        assert!(
            confined.check(&format!("CREATE ROLE \"{role}\" NOLOGIN")).is_err(),
            "precondition: Confined denies CREATE ROLE"
        );
        assert!(
            confined
                .check(&format!("CREATE TABLE \"{other_schema}\".t (id int)"))
                .is_err(),
            "precondition: Confined denies cross-schema CREATE TABLE"
        );

        // The Trusted migration: create the cross-schema home, a privileged ROLE,
        // and a table in the OTHER schema — all guard-denied under Confined.
        let up = format!(
            "CREATE SCHEMA \"{other_schema}\"; \
             CREATE ROLE \"{role}\" NOLOGIN; \
             CREATE TABLE \"{other_schema}\".t (id int primary key);"
        );
        let migs = vec![mig(1, "trusted_arbitrary", &up, false)];

        let outcome = apply(&conn, &cfg, &migs, Approval::Approved, "trusted-test").await;
        assert!(
            outcome.is_ok(),
            "Trusted apply of guard-denied SQL must succeed; got {outcome:?}"
        );

        // It really applied: the role exists, and the cross-schema table exists.
        assert!(role_exists(&conn, &role).await, "the CREATE ROLE applied");
        assert!(
            table_exists(&conn, &other_schema, "t").await,
            "the cross-schema CREATE TABLE applied"
        );

        // ---- teardown: drop the cluster-global role + both schemas ----
        let _ = conn
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
                 DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
                 DROP SCHEMA IF EXISTS \"{}\" CASCADE; \
                 DROP ROLE IF EXISTS \"{}\";",
                other_schema, cfg.project_schema, cfg.pg.meta_schema, role
            ))
            .await;
        assert!(
            !role_exists(&conn, &role).await,
            "teardown removed the test role (no leak)"
        );
    }

    /// `trusted_still_derives_destructive_flag` — a `DROP TABLE` under a Trusted
    /// apply still has its destructive/requires_approval flags set (classify is
    /// trust-independent). Here we assert the guard report the executor derives is
    /// destructive, AND that the apply is REFUSED without approval (the engine's
    /// `Approval` re-check is the in-executor mirror of the CLI `--yes` gate).
    #[compio::test]
    async fn trusted_still_derives_destructive_flag() {
        let conn = pg().await;
        let tok = token();
        let cfg = trusted_cfg(&tok);

        conn.batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
        journal::ensure_journal(&conn, &cfg).await.expect("journal");

        // The Trusted guard report for a DROP TABLE is destructive even though the
        // deny-list is skipped (classify runs regardless of trust).
        let guard = SqlGuard::new(cfg.guard_config());
        let report = guard
            .check(&format!("DROP TABLE \"{}\".gone", cfg.project_schema))
            .expect("Trusted does not deny a DROP TABLE");
        assert!(report.destructive, "DROP TABLE is destructive under Trusted");
        assert!(
            crate::guard::flags_for(&report).requires_approval,
            "destructive ⇒ requires_approval (CLI --yes) under Trusted"
        );

        // And the in-executor Approval mirror refuses a destructive apply WITHOUT
        // approval: the table is created first (non-destructive), then a DROP is
        // refused under `Approval::None`.
        let create = vec![mig(
            1,
            "create",
            &format!("CREATE TABLE \"{}\".gone (id int)", cfg.project_schema),
            false,
        )];
        apply(&conn, &cfg, &create, Approval::Approved, "trusted-test")
            .await
            .expect("create applies");

        let drop = vec![mig(
            2,
            "drop",
            &format!("DROP TABLE \"{}\".gone", cfg.project_schema),
            true,
        )];
        let refused = apply(&conn, &cfg, &drop, Approval::None, "trusted-test").await;
        assert!(
            matches!(refused, Err(ApplyError::ApprovalRequired)),
            "a destructive Trusted apply must be REFUSED without approval; got {refused:?}"
        );
        // The table is still there — the destructive op did not run.
        assert!(
            table_exists(&conn, &cfg.project_schema, "gone").await,
            "the refused DROP did not run"
        );

        let _ = conn
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
                cfg.project_schema, cfg.pg.meta_schema
            ))
            .await;
    }
}
