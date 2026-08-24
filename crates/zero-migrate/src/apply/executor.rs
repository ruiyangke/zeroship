//! The versioned executor — the apply flow.
//!
//! The heart of the engine. Given a backend, an [`ExecutorConfig`], and the
//! project's full migration set, the apply shell
//! ([`MigrationEngine::apply`](crate::engine::MigrationEngine::apply), which
//! reaches `apply_with_lock_backend` below):
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

use crate::approval::Approval;
// The orchestration below is driver-neutral AND vendor-neutral: it names the
// dialect seam and nothing behind it. A backend arrives as a `&B` parameter,
// constructed by whoever knows which vendor this deploy targets — never here.
use crate::apply::backend::MigrationBackend;
use crate::apply::journal::{AppliedEntry, Phase};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Migration, MigrationId};
use crate::render::plan::AppliedPlan;
use crate::render::step::PlanStep;

// ── The apply/rollback VOCABULARY moved down to the backend contract, whose
// `MigrationBackend` signatures name every one of these. The ORCHESTRATION — the
// two-pass apply body, the topological ordering, the rollback selection — stays
// here. Re-exported so every `crate::apply::executor::…` path (and the flattened
// root re-exports in `lib.rs`) resolves unchanged.
pub use zero_migrate_backend::executor::{
    ApplyError, ApplyOutcome, BackendError, LockMode, PreconditionVerdict, RollbackError,
    RollbackOptions, RollbackOutcome, RollbackRequest, RollbackTarget,
};
// `authorize_existence_guard_schema` followed the vocabulary down. It is the gate
// each backend's own session path runs before reading a schema a guard NAMED, so it
// has to be reachable from a vendor crate; it reads the `ExecutorConfig`'s composed
// policy and nothing of the orchestration's.
pub use zero_migrate_backend::executor::authorize_existence_guard_schema;

// `unmet_halt_error` travelled with them and for the same reason: it formats a
// `Precondition` and a blocker list into an `ApplyError`, touches no database, and the
// PostgreSQL precondition evaluator is one of its two callers.
pub(crate) use zero_migrate_backend::executor::unmet_halt_error;

/// Apply the project's pending migrations through a backend the CALLER built.
/// Idempotent: a re-run with no new migrations is a no-op.
///
/// Takes the project lock for the batch ([`LockMode::Acquire`]) under the blanket
/// [`ApprovalScope::All`](crate::approval::ApprovalScope::All); a caller that owns
/// the lock or carries a per-version scope drives `apply_with_lock_backend`
/// through the engine instead.
///
/// `applied_by` is the actor recorded in the journal (`app/actor/AI`).
///
/// `approval` is the caller's approval decision. This is the executor's OWN
/// defense-in-depth approval gate: if any pending migration is
/// flagged [`destructive`](crate::model::migration::MigrationFlags::destructive) and
/// `approval != Approval::Approved`, the apply is refused with
/// [`ApplyError::ApprovalRequired`] before any migration executes — independent
/// of (and additional to) the engine's gate, so a caller driving this directly
/// cannot bypass approval. A non-destructive batch runs with [`Approval::None`].
///
/// # It takes a backend, not a session, and that is the point
///
/// This used to take `&D: SqlSession` and build a `PostgresBackend` from it, which
/// made a neutral orchestration entry silently PostgreSQL-only — the dialect was
/// decided here, by this file, for every caller. It now takes whatever backend the
/// caller resolved. The body is otherwise byte-identical: same shell, same scope,
/// same lock mode.
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
pub async fn apply<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
    approval: Approval,
    applied_by: &str,
) -> Result<ApplyOutcome, ApplyError> {
    apply_with_lock_backend(
        backend,
        cfg,
        migrations,
        approval,
        &crate::approval::ApprovalScope::All,
        applied_by,
        LockMode::Acquire,
    )
    .await
}

/// The lock + session-hygiene shell around [`apply_locked`], generic over the
/// dialect seam. Every caller — the engine's declarative path, the engine's flat
/// [`apply`](crate::engine::MigrationEngine::apply) path, and each backend's own
/// tests — CONSTRUCTS or FORWARDS its backend and calls this.
///
/// It takes `&B` and never builds one. That is the whole reason this file names no
/// vendor: choosing between PostgreSQL, MySQL and SQLite is knowledge about which
/// vendors exist, and it lives with whoever knows the deploy's target — the host —
/// not in the orchestration. Two `pub` entries used to sit here doing exactly that
/// (`apply_with_lock` built a `PostgresBackend`, `apply_with_lock_mysql` built a
/// `MysqlBackend`); both were dead, and deleting them is what made core neutral
/// here. Do not reintroduce a vendor-constructing entry point in this module.
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
    if let Some(marker) = backend
        .unresolved_rollback_markers(cfg)
        .await?
        .into_iter()
        .find(|marker| supplied_versions.contains(marker.version.as_str()))
    {
        return Err(ApplyError::UnresolvedRollbackMarker {
            version: marker.version,
            clear_instruction: marker.clear_instruction,
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
    // This is the SHARED comparison — `crate::apply::drift::compare_applied_to_set` builds
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
    // trust profile from `cfg.guard_config_for(..)` (the trust profile lives on
    // `ExecutorConfig`, not the backend):
    //   - Postgres → `PgGuard` (libpg_query deny-list) — byte-identical to the
    //     pre-seam `SqlGuard::new(cfg.guard_config_for(..))`;
    //   - SQLite → `SqliteGuard` (from `zero-migrate-sqlite`) — the trusted descriptor-diff path
    //     (`check` returns the empty clean outcome: `libpg_query` cannot vet SQLite,
    //     the first-line vet is the descriptor emitter at the author boundary and the
    //     second-line defense is the backend authorizer applied per statement at apply).
    // The non-txn idempotency check still runs through the trait (`validate_non_txn`),
    // which for SQLite rejects `transaction:false` at the dialect boundary.
    let guard = crate::render::backends::guard_for(&cfg.guard_config_for(&backend.dialect()));

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
    guard_repeatable_batch(cfg, &backend.dialect(), &ordered)?;

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
    dialect: &zero_migrate_ir::dialect::DialectId,
    migrations: &[&Migration],
) -> Result<(), ApplyError> {
    let guard = crate::render::backends::guard_for(&cfg.guard_config_for(dialect));
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

/// The per-migration precondition verdict loop now lives in
/// `zero_migrate_postgres::backend::precondition::evaluate_all` — the **Postgres** leaf reached only via
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

// The migration-graph vocabulary — `order_pending`, the shared version-tiebroken
// topological core beneath it, `canonical_set_order` and `compute_superseded` — moved
// down to the backend contract. Every line of it reads `depends_on`/`supersedes` off a
// `Migration` and a journal `AppliedEntry`; none of it names a dialect, a vendor or a
// statement. It had to travel because a vendor journal reader answers the same
// "what is pending?" question apply answers and must reuse the ONE implementation:
// `zero-migrate-postgres`'s `status_sql` says so in its own comment, "so the two views
// never diverge". Re-exported so every `crate::apply::executor::…` path resolves
// unchanged.
pub(crate) use zero_migrate_backend::executor::{
    canonical_set_order, compute_superseded, order_pending, topo_order_version_tiebroken,
};
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

        guard_repeatable_batch(&cfg, &crate::test_fixtures::MYSQL, &[&repeatable])
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
/// The counterpart to the crate-private `apply_with_lock_backend` shell, and the
/// driver [`plan_rollback`] was written for.
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
// `zero_migrate_backend::guard`.
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

        // The gate tests drive `apply`/`rollback`, which never lower a raw island and
        // never derive flags from SQL text — those run in `render::lower` and
        // `plan::author`. Panicking rather than answering keeps this double from
        // silently standing in for a posture it was never written to express.
        fn check_raw_island_sql(&self, _sql: &str) -> Result<(), crate::guard::GuardError> {
            unreachable!("gate tests never lower a raw island")
        }
        fn check_raw_island_body(
            &self,
            _body: &str,
            _raw: &str,
        ) -> Result<(), crate::guard::GuardError> {
            unreachable!("gate tests never lower a raw function body")
        }
        fn raw_island_escapes_rls_net_state(&self, _sql: &str) -> bool {
            unreachable!("gate tests never run the require_rls net-state walk")
        }
        fn refuses_destructive_ops_itself(&self) -> bool {
            unreachable!("gate tests never run the destructive-posture walk either")
        }
        fn flags_for_sql(
            &self,
            _up: &str,
        ) -> Result<crate::model::migration::MigrationFlags, crate::guard::GuardError> {
            unreachable!("gate tests supply flags directly; they never derive them from SQL")
        }
    }

    /// A guard that refuses everything, to prove the `down` is guarded at all.
    struct DenyingGuard;
    impl crate::guard::MigrationGuard for DenyingGuard {
        fn check(&self, _up: &str) -> Result<crate::guard::GuardOutcome, crate::guard::GuardError> {
            Err(crate::guard::GuardError::RawSqlRejected {
                dialect: crate::test_fixtures::MYSQL,
            })
        }

        // The gate tests drive `apply`/`rollback`, which never lower a raw island and
        // never derive flags from SQL text — those run in `render::lower` and
        // `plan::author`. Panicking rather than answering keeps this double from
        // silently standing in for a posture it was never written to express.
        fn check_raw_island_sql(&self, _sql: &str) -> Result<(), crate::guard::GuardError> {
            unreachable!("gate tests never lower a raw island")
        }
        fn check_raw_island_body(
            &self,
            _body: &str,
            _raw: &str,
        ) -> Result<(), crate::guard::GuardError> {
            unreachable!("gate tests never lower a raw function body")
        }
        fn raw_island_escapes_rls_net_state(&self, _sql: &str) -> bool {
            unreachable!("gate tests never run the require_rls net-state walk")
        }
        fn refuses_destructive_ops_itself(&self) -> bool {
            unreachable!("gate tests never run the destructive-posture walk either")
        }
        fn flags_for_sql(
            &self,
            _up: &str,
        ) -> Result<crate::model::migration::MigrationFlags, crate::guard::GuardError> {
            unreachable!("gate tests supply flags directly; they never derive them from SQL")
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

        // The gate tests drive `apply`/`rollback`, which never lower a raw island and
        // never derive flags from SQL text — those run in `render::lower` and
        // `plan::author`. Panicking rather than answering keeps this double from
        // silently standing in for a posture it was never written to express.
        fn check_raw_island_sql(&self, _sql: &str) -> Result<(), crate::guard::GuardError> {
            unreachable!("gate tests never lower a raw island")
        }
        fn check_raw_island_body(
            &self,
            _body: &str,
            _raw: &str,
        ) -> Result<(), crate::guard::GuardError> {
            unreachable!("gate tests never lower a raw function body")
        }
        fn raw_island_escapes_rls_net_state(&self, _sql: &str) -> bool {
            unreachable!("gate tests never run the require_rls net-state walk")
        }
        fn refuses_destructive_ops_itself(&self) -> bool {
            unreachable!("gate tests never run the destructive-posture walk either")
        }
        fn flags_for_sql(
            &self,
            _up: &str,
        ) -> Result<crate::model::migration::MigrationFlags, crate::guard::GuardError> {
            unreachable!("gate tests supply flags directly; they never derive them from SQL")
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
