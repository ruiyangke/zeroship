//! The **migration-submission adapter** — the single safe ingress for a
//! client-authored migration script.
//!
//! A client (the builder / control plane / CLI) hands in a [`Submission`]: raw
//! `up` SQL, an optional `down`, and a little metadata. [`submit_migration`] runs
//! it through the FULL security stack — **ingest → guard → lint → live-seeded
//! dry-run → gate → apply** — and never lets any one of those steps be skipped or
//! its verdict forged. The submitter cannot reach [`crate::apply::executor::apply`] (nor
//! the engine gate) except through this funnel.
//!
//! # The whole point: the submitter cannot lie about danger
//!
//! [`Submission`] has **no** `destructive` / `requires_approval` field by
//! construction. A `DROP`/`TRUNCATE`/`DROP COLUMN`/lossy-type-change is judged
//! **server-side** by the [`SqlGuard`](crate::guard::SqlGuard) and folded into the
//! migration's [`MigrationFlags`] via [`flags_for`](crate::guard::flags_for). So a
//! client can never mark a destructive migration "non-destructive" to auto-apply
//! it — the gate decides on the SERVER-DERIVED flags, never on anything the
//! submitter supplied.
//!
//! # Flow ([`submit_migration`])
//!
//! 1. **Ingest → [`Migration`]**: mint a fresh [`MigrationId`]; build `up` / `down`
//!    / `depends_on` / `owner_app`; **derive flags via the guard** —
//!    `SqlGuard::check(up)` → [`flags_for`] gives `destructive` / `transactional` /
//!    `requires_approval` (NOT from the submission); layer the submission's
//!    `repeatable` / `timeout_ms` on top; compute the [`Checksum`] over the whole
//!    apply-relevant unit.
//! 2. **Guard**: re-check `up` (and `down` if present); a denial ⇒
//!    [`SubmissionOutcome::Denied`] — NOTHING runs, prod untouched.
//! 3. **Lint**: collect [`analyze`](crate::analyze::analyze) advisories (carried in
//!    the outcome, never gating).
//! 4. **Dry-run on a live-seeded shadow** ([`dry_run_incremental`]): a failure to
//!    apply on the shadow ⇒ [`SubmissionOutcome::DryRunFailed`] — the REAL DB is
//!    untouched.
//! 5. **Gate**: if the SERVER-DERIVED `flags.destructive` / `flags.requires_approval`
//!    AND `approval != Approved` ⇒ [`SubmissionOutcome::ApprovalRequired`] (with the
//!    advisories + `dry_run_ok: true` for the operator to review). A destructive
//!    submission is NEVER auto-applied.
//! 6. **Apply**: otherwise [`MigrationEngine::apply`] the single-migration set under
//!    the migrator role + journal ⇒ [`SubmissionOutcome::Applied`].
//!
//! # Idempotency / dedup key — the **checksum**
//!
//! A fresh [`MigrationId`] is minted on every call, so version cannot be the dedup
//! key. We dedup on the migration's [`Checksum`], which folds the WHOLE
//! apply-relevant unit (`up`, `down`, `flags`, `owner_app`, `depends_on`,
//! `supersedes`, `preconditions`) and is computed identically every call for the
//! same script — it deliberately EXCLUDES `version` and `name`. Before applying,
//! [`submit_migration`] reads the journal's net-applied checksums
//! ([`journal::applied`](crate::apply::journal::applied)); if this submission's checksum
//! is already net-applied it returns [`SubmissionOutcome::NoOp`] WITHOUT a second
//! journal apply. (Name is intentionally not part of the key: a re-submit that
//! only renames the human label is the same applied unit.)
//!
//! ## The dedup key is NET-applied — resubmit-after-rollback RE-APPLIES (MED-3)
//!
//! The dedup is over the journal's **net-applied** checksums — a checksum whose
//! LATEST journal event is `completed` (see
//! [`journal::applied`](crate::apply::journal::applied)). A migration that was applied and
//! then ROLLED BACK is no longer net-applied, so resubmitting the IDENTICAL script
//! after a rollback **RE-APPLIES** it (it is NOT a [`SubmissionOutcome::NoOp`]).
//! This is intended: the rolled-back unit is gone from the live schema and the
//! operator is explicitly asking for it back. The dedup answers "is this exact
//! apply-relevant unit CURRENTLY live?", not "was it EVER applied?".
//!
//! ## Checksum-collision NoOp masking (MED-4)
//!
//! `name` is excluded from the checksum, so two genuinely DIFFERENT submissions
//! that fold to the same apply-relevant unit (identical
//! `up`/`down`/`flags`/`owner_app`/`depends_on`/`supersedes`/`preconditions`,
//! differing only in `name`) COLLIDE: the second correctly
//! [`SubmissionOutcome::NoOp`]s (the live unit IS identical), but that NoOp may be
//! MASKING a different operator intent. The adapter emits a `tracing::debug` line
//! on exactly that case — a NoOp whose journaled migration name ≠ the submission's
//! name — so the masking is observable in the logs. It does NOT change the
//! (correct) NoOp behavior; identity-folding two same-bodied units is the right
//! call, the log just surfaces the rare accidental collision.
//!
//! ## Concurrency: dedup→apply is one locked critical section (HIGH-1)
//!
//! The dedup read and the apply MUST be serialized as ONE critical section under
//! the project advisory lock. The dedup key is the checksum, but the executor's
//! pending computation dedups by VERSION — and a fresh [`MigrationId`] is minted
//! every call. So if the dedup read ran unlocked, two concurrent identical
//! submissions would mint different versions, both pass the unlocked dedup, and
//! both apply (a double-apply of a non-idempotent `up`). [`submit_migration`]
//! therefore acquires the project advisory lock (the H10 `acquire_project_lock_outer`
//! mechanism) BEFORE the dedup read and holds it across the apply — driving the
//! inner [`MigrationEngine::apply_with_lock`] with [`LockMode::AlreadyHeld`] so the
//! executor does not re-acquire it — and releases it on every exit path. The
//! second submission blocks until the first has applied + journaled, then its
//! dedup read sees the net-applied checksum and returns [`SubmissionOutcome::NoOp`].
//!
//! # Ownership: `owner_app` is UNTRUSTED (HIGH-2 / LOW-3)
//!
//! `submission.owner_app` is folded into the checksum but is NOT an authorization
//! anchor. Ownership is enforced against a caller-supplied `ownership` map (bare
//! table name → owning app), the same shape and semantics the declarative differ's
//! `live_ownership` uses. After the guard and before the dry-run/apply, the adapter
//! mirrors `declarative::enforce_ownership`: an `ALTER`/`DROP`/`RENAME` or DML
//! against a relation the map attributes to a DIFFERENT app — or, for a `DROP`, one
//! whose owner the map cannot confirm — is REFUSED
//! ([`SubmissionOutcome::OwnershipDenied`], carrying the reused
//! [`DeclarativeError::NotTableOwner`] / [`DeclarativeError::DropOfUnownedTable`]);
//! a `CREATE TABLE` establishes ownership for `owner_app`. `owner_app` is COMPARED
//! against the trusted map, never trusted as the principal, and the `applied_by`
//! actor is recorded independently of it (LOW-3).

use std::collections::HashMap;

use compio_postgres::Client;

use crate::approval::Approval;
use crate::apply::backend::PostgresBackend;
use crate::analysis::classify::{relations_touched, OwnershipNeed};
use crate::conn::ExecutorConfig;
use crate::render::declarative::DeclarativeError;
use crate::engine::{EngineError, MigrationEngine};
use crate::apply::executor::{self, LockMode};
use crate::guard::{flags_for, GuardConfig, GuardError, SqlGuard};
use crate::apply::journal::{self, JournalError};
use crate::model::migration::{Checksum, ChecksumInput, Migration, MigrationFlags, MigrationId};
use crate::apply::backend::{DryRunError, ShadowConfig};
use crate::apply::backend::postgres::shadow::dry_run_incremental;

/// A client-submitted migration script (raw SQL + light metadata).
///
/// **Deliberately carries NO `destructive` / `requires_approval` field.** Whether a
/// migration is destructive (and therefore gated) is a SERVER judgement derived by
/// the guard, never something the submitter declares — so a `DROP`/`TRUNCATE`
/// cannot be smuggled in as "safe". See the module docs.
#[derive(Debug, Clone)]
pub struct Submission {
    /// Human-readable migration name (a label only; no apply effect, NOT part of
    /// the dedup checksum). Because it is excluded from the checksum, two
    /// submissions with identical bodies but different names COLLIDE — the second
    /// NoOps; the adapter logs a `debug` line on a name mismatch so an accidental
    /// collision is observable (MED-4, see the module docs).
    pub name: String,
    /// The forward SQL to apply.
    pub up: String,
    /// The reverse SQL, or `None` for an explicitly irreversible migration.
    ///
    /// The `down` is guard-checked here (a malicious/broken reverse cannot slip
    /// in) but is NOT folded into the apply-time approval gate even when it is
    /// destructive (LOW-1): `down` runs only on a SEPARATELY-gated rollback
    /// (`executor::rollback` / the engine rollback path), which makes its own
    /// approval decision at rollback time. Gating the forward apply on the
    /// destructiveness of a reverse that may never run would be the wrong axis.
    pub down: Option<String>,
    /// Cross-slice ordering dependencies (already-known migration versions).
    pub depends_on: Vec<MigrationId>,
    /// Marks a replace-style repeatable migration (an authoring facet; runs after
    /// versioned migrations and re-applies on checksum change). Not derivable from
    /// SQL, so it IS taken from the submission.
    pub repeatable: bool,
    /// Optional per-migration `statement_timeout` (ms), for a long backfill / a big
    /// concurrent index. Not derivable from SQL, so it IS taken from the submission.
    pub timeout_ms: Option<u64>,
    /// The declaring app (`app_…`) — per-table ownership, folded into the checksum.
    pub owner_app: String,
}

/// The verdict of [`submit_migration`].
#[derive(Debug, Clone)]
pub enum SubmissionOutcome {
    /// The migration passed every stage and was applied + journaled.
    Applied {
        /// The minted version of the applied migration.
        version: MigrationId,
        /// Operational advisories surfaced by the linter (lock-heavy ops,
        /// destructive shapes, missing FK indexes, …). Advisory-only.
        advisories: Vec<crate::analysis::analyze::Advisory>,
    },
    /// The migration is destructive (SERVER-DERIVED) and `approval != Approved`, OR
    /// the author/guard flagged it `requires_approval`. NOTHING was applied — the
    /// operator reviews the advisories + the passing dry-run and re-submits with
    /// [`Approval::Approved`].
    ApprovalRequired {
        /// The minted version (stable for this submission instance only — a
        /// re-submit mints a new one; dedup is by checksum).
        version: MigrationId,
        /// The linter advisories for the operator's review.
        advisories: Vec<crate::analysis::analyze::Advisory>,
        /// Always `true` here: the gate is only reached AFTER the dry-run passed.
        dry_run_ok: bool,
    },
    /// The guard hard-denied the `up` (or `down`) SQL — RCE / privilege-escalation
    /// / cross-tenant / file / network. NOTHING ran; the real DB is untouched.
    Denied {
        /// The stable deny-list rule id, or a descriptor for a cross-schema /
        /// parse denial.
        rule: String,
        /// The offending statement text.
        statement: String,
    },
    /// The migration applied cleanly through the guard but FAILED to apply on the
    /// live-seeded shadow (bad SQL, a missing relation, a constraint violation).
    /// The REAL DB + journal are untouched.
    DryRunFailed {
        /// The shadow apply error (the offending migration + reason).
        error: String,
    },
    /// This exact migration (same checksum) is already **net-applied** in the
    /// journal — an idempotent re-submit. No second journal apply happened.
    ///
    /// The key is NET-applied (latest event `completed`), not ever-applied: an
    /// identical resubmit AFTER a rollback re-applies (it is NOT a NoOp) — see the
    /// module docs (MED-3). A NoOp can also fire on a checksum COLLISION (two
    /// different submissions that fold to the same apply-relevant unit, differing
    /// only in `name`); the adapter logs a `debug` line when the journaled name
    /// differs, surfacing the potential masking (MED-4).
    NoOp {
        /// The minted version (informational; the already-applied row keeps its
        /// own original version).
        version: MigrationId,
    },
    /// The `up` tried to `ALTER`/`DROP`/`RENAME` or write (`INSERT`/`UPDATE`/
    /// `DELETE`/`TRUNCATE`) a relation the deploying app does NOT own (HIGH-2).
    /// Ownership is enforced against a caller-supplied ownership map — NOT the
    /// submitter-supplied `owner_app`, which is untrusted. NOTHING was applied; the
    /// real DB + journal are untouched. The carried [`DeclarativeError`] is the
    /// REUSED [`DeclarativeError::NotTableOwner`] (a foreign-owned relation) or
    /// [`DeclarativeError::DropOfUnownedTable`] (a `DROP` of a relation whose
    /// ownership the map cannot confirm — fail-closed, mirroring the differ).
    OwnershipDenied {
        /// The reused declarative ownership error (`NotTableOwner` /
        /// `DropOfUnownedTable`).
        error: DeclarativeError,
    },
}

/// A failure of the submission adapter itself (infrastructure, not a verdict).
///
/// A guard denial / dry-run failure / approval-required is NOT an error — those are
/// [`SubmissionOutcome`] variants. These are faults that stop the pipeline before a
/// verdict can be reached.
#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    /// Reading the journal's net-applied checksums (the dedup lookup) failed.
    #[error("submit: journal lookup failed: {0}")]
    Journal(#[from] JournalError),
    /// The live-seeded shadow dry-run harness itself failed (CREATE/DROP DATABASE,
    /// shadow connect, role provisioning, the live snapshot / seed) — distinct from
    /// the migration failing on the shadow (that is [`SubmissionOutcome::DryRunFailed`]).
    #[error("submit: dry-run harness failed: {0}")]
    DryRun(#[from] DryRunError),
    /// The gated apply failed for a reason that is NOT a guard denial / missing
    /// approval (those are caught upstream as outcomes) — an executor / DB failure,
    /// a checksum-drift abort, a missing dependency, etc.
    #[error("submit: apply failed: {0}")]
    Apply(#[from] EngineError),
}

impl MigrationEngine {
    /// Submit a client migration script through the full
    /// guard→lint→dry-run→gate→apply stack — a thin method over
    /// [`submit_migration`] for callers that already hold an engine.
    ///
    /// # Errors
    /// [`SubmitError`] on an adapter-infrastructure fault (journal lookup, dry-run
    /// harness, or a non-gate apply failure). A guard denial / dry-run-failure /
    /// approval-required is a [`SubmissionOutcome`], not an error.
    // Seven irreducible inputs (two conns, project cfg, shadow cfg, the submission,
    // the approval decision, the actor) plus &self — each load-bearing; bundling
    // them would only obscure the call shape. Mirrors `apply_verified`.
    #[allow(clippy::too_many_arguments)]
    pub async fn submit(
        &self,
        conn: &Client,
        admin_conn: &Client,
        cfg: &ExecutorConfig,
        shadow_cfg: &ShadowConfig,
        submission: Submission,
        ownership: &HashMap<String, String>,
        approval: Approval,
        applied_by: &str,
    ) -> Result<SubmissionOutcome, SubmitError> {
        submit_migration(
            conn, admin_conn, cfg, shadow_cfg, submission, ownership, approval, applied_by,
        )
        .await
    }
}

/// Fail-closed ownership enforcement for the submit path (HIGH-2) — the adapter
/// peer of [`declarative::enforce_ownership`](crate::render::declarative).
///
/// For each ownership-relevant relation the `up` touches (extracted from the SAME
/// real-Postgres parse the guard uses, via
/// [`relations_touched`](crate::classify::relations_touched)):
///
/// - [`OwnershipNeed::Establishes`] (`CREATE TABLE`) — always allowed; the
///   deploying app (`owner_app`) becomes the owner. (The map is the journaled
///   ownership ledger; a CREATE of an already-owned name is caught downstream as a
///   dry-run / apply failure, not here.)
/// - [`OwnershipNeed::RequiresOwnership`] (`ALTER`/`RENAME`/DML/`CREATE INDEX`) —
///   allowed ONLY if `ownership[relation] == owner_app`. A DIFFERENT owner ⇒
///   [`DeclarativeError::NotTableOwner`]. An UNKNOWN owner (no map entry) also
///   fails closed as [`DeclarativeError::NotTableOwner`] (the deploying app cannot
///   prove it owns the relation — it is treated as foreign, never auto-claimed by
///   a non-CREATE).
/// - [`OwnershipNeed::RequiresOwnershipForDrop`] (`DROP TABLE`) — a DIFFERENT
///   owner ⇒ [`DeclarativeError::NotTableOwner`]; an UNKNOWN owner ⇒
///   [`DeclarativeError::DropOfUnownedTable`] (mirrors the differ's fail-closed
///   drop check, which refuses a destructive drop it cannot attribute).
///
/// Returns `Some(err)` on the FIRST refusal (nothing else runs), `None` if every
/// touched relation is owned by `owner_app` (or only established).
///
/// `owner_app` is the submitter-supplied declaring app — used here only as the
/// value to COMPARE against the trusted map, never as the authorization principal
/// (a submitter cannot grant itself ownership by claiming `owner_app`).
fn enforce_ownership(
    up: &str,
    owner_app: &str,
    ownership: &HashMap<String, String>,
) -> Option<DeclarativeError> {
    // The guard already validated parseability before this is reached; on the
    // (unreachable) parse failure, fail closed — refuse rather than skip the check.
    let touched = match relations_touched(up) {
        Ok(t) => t,
        Err(_) => {
            return Some(DeclarativeError::DropOfUnownedTable {
                table: "<unparseable>".to_string(),
            })
        }
    };
    for t in touched {
        match t.need {
            OwnershipNeed::Establishes => {}
            OwnershipNeed::RequiresOwnership => match ownership.get(&t.relation) {
                Some(owner) if owner == owner_app => {}
                Some(owner) => {
                    return Some(DeclarativeError::NotTableOwner {
                        table: t.relation,
                        owner: owner.clone(),
                        deploying_app: owner_app.to_string(),
                    });
                }
                None => {
                    // Unknown ownership for a non-CREATE structural/DML touch —
                    // fail closed: the deploying app has not proven it owns this
                    // relation, so treat it as foreign (never auto-claim).
                    return Some(DeclarativeError::NotTableOwner {
                        table: t.relation,
                        owner: "<unknown>".to_string(),
                        deploying_app: owner_app.to_string(),
                    });
                }
            },
            OwnershipNeed::RequiresOwnershipForDrop => match ownership.get(&t.relation) {
                Some(owner) if owner == owner_app => {}
                Some(owner) => {
                    return Some(DeclarativeError::NotTableOwner {
                        table: t.relation,
                        owner: owner.clone(),
                        deploying_app: owner_app.to_string(),
                    });
                }
                None => {
                    return Some(DeclarativeError::DropOfUnownedTable { table: t.relation });
                }
            },
        }
    }
    None
}

/// Render a [`GuardError`] into the `(rule, statement)` pair of
/// [`SubmissionOutcome::Denied`].
fn denial(e: &GuardError) -> (String, String) {
    match e {
        GuardError::Denied { rule, statement } => ((*rule).to_string(), statement.clone()),
        GuardError::CrossSchema { schema, statement } => {
            (format!("cross_schema:{schema}"), statement.clone())
        }
        GuardError::Parse(p) => ("unparseable".to_string(), p.to_string()),
        // PHASE 4 — the raw-SQL submit path is PG-only; a SQLite Confined guard
        // refuses raw SQL outright (descriptor-diff-only). If this is ever reached,
        // surface it as a denial with a stable rule id.
        GuardError::SqliteRawSqlRejected => (
            "sqlite_raw_sql_rejected".to_string(),
            "raw SQL not accepted on the Confined SQLite path".to_string(),
        ),
        GuardError::MysqlRawSqlRejected => (
            "mysql_raw_sql_rejected".to_string(),
            "raw SQL not accepted on the render-only MySQL path".to_string(),
        ),
    }
}

/// Submit a client migration script and safely execute it through the full
/// **ingest → guard → lint → live-seeded dry-run → gate → apply** stack.
///
/// `conn` is the project DB (apply + journal). `admin_conn` is an admin session
/// (its role needs `CREATEDB`) used ONLY for the shadow `CREATE/DROP DATABASE` and
/// the read-only live snapshot. `shadow_cfg` is the throwaway-shadow config. The
/// `submission`'s destructiveness is SERVER-DERIVED — see [`Submission`].
///
/// Returns a [`SubmissionOutcome`] for every NON-infrastructure result (applied /
/// approval-required / denied / dry-run-failed / no-op / ownership-denied); a
/// [`SubmitError`] only on an adapter-infrastructure fault.
///
/// # Ownership (HIGH-2)
///
/// `ownership` maps a BARE relation name → the owning app (`app_…`), exactly the
/// shape the declarative differ's `live_ownership` uses. The adapter enforces it
/// the same way `declarative::enforce_ownership` does: an `ALTER`/`DROP`/`RENAME`
/// or DML against a relation the map attributes to a DIFFERENT app — or, for a
/// `DROP`, one whose owner the map cannot confirm — is REFUSED
/// ([`SubmissionOutcome::OwnershipDenied`]); a `CREATE TABLE` establishes
/// ownership for `submission.owner_app`. The submitter-supplied `owner_app` is
/// **UNTRUSTED**: it is checked against the map, never trusted as the
/// authorization principal.
///
/// # Errors
/// [`SubmitError`] — see its variants. A guard denial, an ownership refusal, a
/// dry-run failure, or a gate (approval) refusal are OUTCOMES, not errors.
#[allow(clippy::too_many_arguments)]
pub async fn submit_migration(
    conn: &Client,
    admin_conn: &Client,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    submission: Submission,
    ownership: &HashMap<String, String>,
    approval: Approval,
    applied_by: &str,
) -> Result<SubmissionOutcome, SubmitError> {
    let guard_cfg = GuardConfig::confined(cfg.project_schema.clone());
    let guard = SqlGuard::new(guard_cfg.clone());

    // ---- 2. GUARD (run FIRST so flag-derivation is over a passing report) ----
    // Check `up`. A denial ⇒ Denied (nothing runs). We need the passing report to
    // derive the server-side flags, so the guard runs before flag derivation.
    let up_report = match guard.check(&submission.up) {
        Ok(r) => r,
        Err(e) => {
            let (rule, statement) = denial(&e);
            return Ok(SubmissionOutcome::Denied { rule, statement });
        }
    };
    // Check `down` too if present — a malicious/broken reverse must not slip in.
    if let Some(down) = submission.down.as_deref() {
        if let Err(e) = guard.check(down) {
            let (rule, statement) = denial(&e);
            return Ok(SubmissionOutcome::Denied { rule, statement });
        }
    }

    // ---- 2b. OWNERSHIP (HIGH-2): fail-closed on a foreign-owned relation ----
    // Mirror `declarative::enforce_ownership`: a non-owner may USE a table's rows
    // but may NOT ALTER/DROP/RENAME its structure or write to it. The submitter's
    // `owner_app` is UNTRUSTED — ownership is decided by the caller-supplied map.
    // Runs AFTER the guard (so the SQL is parseable + confined) and BEFORE the
    // dry-run/apply (nothing runs on a refusal). The check is pure-parse — no DB —
    // so it sits outside the project advisory lock.
    if let Some(error) = enforce_ownership(&submission.up, &submission.owner_app, ownership) {
        return Ok(SubmissionOutcome::OwnershipDenied { error });
    }

    // ---- 1. INGEST → Migration (flags SERVER-DERIVED from the guard report) ----
    let version = MigrationId::generate();
    // Derive destructive / transactional / requires_approval from the guard report
    // (NOT from the submission), then layer the authoring-only facets the SQL
    // cannot express (repeatable, timeout_ms) from the submission.
    let derived = flags_for(&up_report);
    let flags = MigrationFlags {
        repeatable: submission.repeatable,
        timeout_ms: submission.timeout_ms,
        ..derived
    };
    let checksum = Checksum::of(&ChecksumInput {
        up: &submission.up,
        down: submission.down.as_deref(),
        flags: &flags,
        owner_app: &submission.owner_app,
        depends_on: &submission.depends_on,
        supersedes: &[],
        preconditions: &[],
    });
    let migration = Migration {
        version: version.clone(),
        name: submission.name.clone(),
        up: submission.up.clone(),
        down: submission.down.clone(),
        checksum: checksum.clone(),
        flags,
        owner_app: submission.owner_app.clone(),
        depends_on: submission.depends_on.clone(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        existence_guard: None,
    };

    // ---- 3. LINT (advisories carried through; never gate) ----
    let advisories = crate::analysis::analyze::analyze(&submission.up);

    // ---- CRITICAL SECTION: acquire the project advisory lock for dedup→apply ----
    //
    // HIGH-1 — the dedup read and the apply MUST be serialized as one critical
    // section. A fresh `MigrationId` is minted every call, so two concurrent
    // identical submissions would mint different versions; the executor's pending
    // computation dedups by VERSION (not checksum), so both would pass an unlocked
    // checksum dedup and both apply (double-apply of a non-idempotent `up`).
    //
    // Fix: take the project advisory lock HERE — before the dedup read — and hold
    // it across dry-run → gate → apply, driving the inner apply with
    // `LockMode::AlreadyHeld` so `engine.apply_with_lock` does NOT re-acquire /
    // re-release it. The second submission blocks on this lock until the first has
    // applied + journaled, then its dedup read sees the now-net-applied checksum
    // and returns `NoOp`. The lock is released on EVERY exit path below
    // (NoOp / DryRunFailed / ApprovalRequired / Applied / error) — mirroring
    // `engine::apply_declarative`'s release-on-every-path discipline.
    executor::acquire_project_lock_outer(conn, &cfg.project_id)
        .await
        .map_err(EngineError::from)?;

    let outcome = submit_locked(
        conn,
        admin_conn,
        cfg,
        shadow_cfg,
        &guard_cfg,
        &migration,
        &checksum,
        version,
        advisories,
        approval,
        applied_by,
    )
    .await;

    // Release on EVERY path (success/outcome/error). Surface the inner result
    // first; a release failure is only logged (the lock auto-releases on session
    // end regardless). Mirrors `apply_declarative`'s H10 discipline.
    if let Err(e) = executor::release_project_lock_outer(conn, &cfg.project_id).await {
        tracing::warn!(
            error = %e,
            project = %cfg.project_id,
            "zeroship-migrate: failed to release project lock after submit (HIGH-1)"
        );
    }

    outcome
}

/// The body of [`submit_migration`] run while the project advisory lock is held
/// (HIGH-1): dedup read → dry-run → gate → apply. The lock serializes the whole
/// section so two concurrent identical submissions can never both apply. Driving
/// the inner apply with [`LockMode::AlreadyHeld`] keeps the executor from
/// re-acquiring the lock the caller already owns.
#[allow(clippy::too_many_arguments)]
async fn submit_locked(
    conn: &Client,
    admin_conn: &Client,
    cfg: &ExecutorConfig,
    shadow_cfg: &ShadowConfig,
    guard_cfg: &GuardConfig,
    migration: &Migration,
    checksum: &Checksum,
    version: MigrationId,
    advisories: Vec<crate::analysis::analyze::Advisory>,
    approval: Approval,
    applied_by: &str,
) -> Result<SubmissionOutcome, SubmitError> {
    // ---- idempotency: dedup by CHECKSUM against the journal's net-applied set ----
    // A fresh version is minted each call, so the checksum (which folds the whole
    // apply-relevant unit and excludes version/name) is the stable dedup key. If
    // this exact unit is already net-applied, short-circuit to NoOp BEFORE the
    // dry-run/gate/apply — a re-submit is a no-op, not a duplicate apply. The read
    // runs under the held lock, so a concurrent identical submission that already
    // applied is visible here (HIGH-1).
    let applied = journal::applied(conn, cfg).await?;
    let already_applied = applied.iter().any(|e| e.checksum == checksum.as_str());
    if already_applied {
        // MED-4 — surface accidental checksum-collision MASKING: the NoOp is
        // correct (the live apply-relevant unit IS identical), but if the journaled
        // migration's NAME differs from this submission's, the collision may be
        // masking a genuinely different operator intent. Emit a debug line so it is
        // observable; the (correct) NoOp behavior is unchanged.
        log_noop_name_mismatch(conn, cfg, checksum.as_str(), &migration.name).await;
        return Ok(SubmissionOutcome::NoOp { version });
    }

    // ---- 4. DRY-RUN on a live-seeded shadow (faithful for an incremental set) ----
    // Seeds the shadow from the CURRENT LIVE structure then applies THIS migration
    // on the shadow. The real DB is never touched. A migration FAILING here is a
    // DryRunFailed outcome (not a SubmitError); only a harness fault is an error.
    let report = dry_run_incremental(
        admin_conn,
        std::slice::from_ref(migration),
        cfg,
        shadow_cfg,
        applied_by,
    )
    .await?;
    // LOW-2 — the shadow's teardown signal (the H2 immediate-sweep hint) is a
    // SEPARATE operational signal from `report.ok` (a migration's correctness is
    // independent of teardown hygiene). Previously discarded; now surfaced. A failed
    // teardown means a shadow DB and/or a cluster-global role may have leaked.
    //
    // We surface it as a `warn` rather than reaping here directly: an immediate
    // `sweep_leaked_shadows(prefix, ZERO)` would match `<prefix>%` and could DROP a
    // CONCURRENT sibling submission's still-in-use shadow (in prod the prefix is
    // shared across a project's submissions). The age-gated reap is the control
    // plane's job (the periodic sweep, or an operator-driven one with a safe
    // `older_than`); the adapter's responsibility is to make the leak observable.
    // This does not change the submission verdict (read from `report.ok` below).
    if !report.teardown_ok {
        tracing::warn!(
            project = %cfg.project_id,
            shadow_prefix = %shadow_cfg.db_name_prefix,
            error = report.teardown_error.as_deref().unwrap_or("unknown"),
            "zeroship-migrate: shadow DB/role teardown failed during submit — a shadow may \
             have leaked; the periodic sweep_leaked_shadows will reap it (LOW-2/H2)"
        );
    }
    if !report.ok {
        // Surface the offending migration's error from the per-migration report.
        let error = report
            .per_migration
            .iter()
            .find(|r| !r.applied_ok)
            .and_then(|r| r.error.clone())
            .unwrap_or_else(|| "dry-run failed on the shadow".to_string());
        return Ok(SubmissionOutcome::DryRunFailed { error });
    }

    // ---- 5. GATE on the SERVER-DERIVED flags (never auto-apply destructive) ----
    if (migration.flags.destructive || migration.flags.requires_approval)
        && approval != Approval::Approved
    {
        return Ok(SubmissionOutcome::ApprovalRequired {
            version,
            advisories,
            dry_run_ok: true,
        });
    }

    // ---- 6. APPLY the single-migration set through the gated engine path ----
    // The engine re-plans (guard) + gates + delegates to executor::apply, which
    // independently re-runs the guard and runs the DDL under the least-privilege
    // migrator role + journal (defense in depth). We forward the approval decision.
    // `LockMode::AlreadyHeld`: we already hold the project advisory lock (HIGH-1),
    // so the executor must NOT re-acquire/re-release it.
    let engine = MigrationEngine::new();
    let plan = engine.plan(std::slice::from_ref(migration), guard_cfg);
    engine
        .apply_with_lock(
            &plan,
            approval,
            &PostgresBackend::new(conn),
            cfg,
            applied_by,
            LockMode::AlreadyHeld,
        )
        .await?;

    Ok(SubmissionOutcome::Applied {
        version,
        advisories,
    })
}

/// MED-4 helper — log a `debug` line when a `NoOp`-firing checksum's journaled
/// migration NAME differs from the resubmitted one, surfacing accidental
/// checksum-collision masking (two genuinely different submissions that fold to the
/// same apply-relevant unit). Best-effort: a lookup failure is swallowed (the NoOp
/// itself is correct regardless).
async fn log_noop_name_mismatch(
    conn: &Client,
    cfg: &ExecutorConfig,
    checksum: &str,
    submitted_name: &str,
) {
    let meta = &cfg.pg.meta_schema;
    // The latest completed journal row for this checksum carries the name the unit
    // was first applied under. `meta_schema` is a server-controlled identifier (not
    // submitter input); the checksum is bound as a parameter.
    let sql = format!(
        "SELECT name FROM \"{meta}\".schema_migrations \
         WHERE checksum = $1 AND phase = 'completed' \
         ORDER BY event_seq DESC LIMIT 1"
    );
    if let Ok(rows) = conn.query(&sql, &[&checksum]).await {
        if let Some(row) = rows.first() {
            let journaled_name: String = row.get("name");
            if journaled_name != submitted_name {
                tracing::debug!(
                    project = %cfg.project_id,
                    checksum,
                    journaled_name = %journaled_name,
                    submitted_name = %submitted_name,
                    "zeroship-migrate: submit NoOp fired on a checksum whose journaled \
                     migration name differs from the submission — possible checksum-collision \
                     masking of a different intent (MED-4)"
                );
            }
        }
    }
}

// ===========================================================================
// In-crate live-PG regression: the submit-path two-guard coupling (LOW-1).
//
// These MUST live in-crate (not in `tests/submit_pg.rs`): the only way to mint a
// Platform `ExecutorConfig` is `ExecutorConfig::platform(&cap, …)` with a
// `OperatorCapability` token, and BOTH that ctor and `OperatorCapability::for_test`
// are `pub(crate)` — unreachable from an integration test (a separate crate). So
// the hostile-caller scenario (a buggy/hostile caller hands `submit_migration` a
// PLATFORM `ExecutorConfig`) can only be constructed from inside the crate.
//
// What this pins: `submit_migration` forces a CONFINED planner guard
// (`GuardConfig::confined(cfg.project_schema)` at the top of the fn), independent
// of `cfg.trust`. The apply path (`engine.apply` → `executor::apply`) re-derives
// its own guard from `ExecutorConfig::guard_config()` (i.e. from `cfg.trust`). The
// submission path's safety against a privileged op therefore rests on TWO
// independent stages: (a) the external boundary (control/builder cannot build a
// Platform `ExecutorConfig` — pinned by the T8 trybuild + T11 unit), AND (b) this
// forced-Confined planner gate denying privileged SQL BEFORE the apply ever runs.
// If a hostile/buggy caller smuggles a Platform `ExecutorConfig` in, stage (b)
// must STILL deny a privileged `up` — proving the two stages can't silently drift
// to "both Platform" and that `submit`'s forced `confined()` is load-bearing.
// ===========================================================================
#[cfg(test)]
// These are `#[compio::test]` futures driven on compio's single-threaded
// io_uring runtime (the whole stack is `!Send` by design — zero tokio), so
// `future_not_send` does not apply to a test harness that never crosses threads.
#[allow(clippy::future_not_send)]
mod low1_two_guard_coupling_pg {
    use std::collections::HashMap;
    use std::time::Duration;

    use compio_postgres::Client;

    use crate::approval::Approval;
    use crate::conn::ExecutorConfig;
    use crate::model::capability::OperatorCapability;
    use crate::apply::journal::ensure_journal;
    use crate::apply::role::{deprovision_migrator, migrator_role_name, provision_migrator};
    use crate::apply::backend::ShadowConfig;
    use crate::ops::submit::{submit_migration, Submission, SubmissionOutcome};

    const DEFAULT_DSN: &str =
        "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

    fn dsn() -> String {
        std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
    }

    async fn pg() -> Client {
        crate::conn::connect(&dsn()).await.expect("connect :5440")
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

    /// Mint a **Platform** `ExecutorConfig` over a unique schema/meta — the exact
    /// shape a hostile/buggy caller would have to forge to weaken the submit path.
    /// Uses the `#[cfg(test)]` `for_test` token seam; unreachable from an
    /// integration test.
    fn platform_cfg(tok: &str) -> ExecutorConfig {
        let cap = OperatorCapability::for_test();
        let schema = format!("proj_{tok}");
        let mut c = ExecutorConfig::platform(
            &cap,
            format!("prj_{tok}"),
            schema.clone(),
            // A platform-style allowlist; `public` is on it so an unqualified
            // privileged op would resolve if the guard let it through.
            vec![schema, "public".to_string()],
            Vec::new(),
        );
        c.pg.meta_schema = format!("meta_{tok}");
        c.pg.statement_timeout = Duration::from_secs(30);
        c.pg.lock_timeout = Duration::from_secs(10);
        let role = migrator_role_name(&c.project_id).unwrap();
        c.with_migrator_role(role)
    }

    fn shadow_cfg(tok: &str) -> ShadowConfig {
        let prefix: String = format!("zsmig_low1_{}_", tok.replace('_', ""))
            .chars()
            .take(40)
            .collect();
        ShadowConfig {
            admin_dsn: dsn(),
            db_name_prefix: prefix,
        }
    }

    async fn setup(conn: &Client, cfg: &ExecutorConfig) {
        conn.batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
        ensure_journal(conn, cfg).await.expect("ensure journal");
        provision_migrator(conn, cfg)
            .await
            .expect("provision migrator role");
    }

    async fn teardown(conn: &Client, cfg: &ExecutorConfig) {
        let _ = deprovision_migrator(conn, cfg).await;
        let _ = conn
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
                cfg.project_schema, cfg.pg.meta_schema
            ))
            .await;
    }

    async fn role_exists(conn: &Client, role: &str) -> bool {
        let rows = conn
            .query("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&role])
            .await
            .expect("query role");
        !rows.is_empty()
    }

    fn no_owners() -> HashMap<String, String> {
        HashMap::new()
    }

    fn submission(name: &str, up: &str) -> Submission {
        Submission {
            name: name.to_string(),
            up: up.to_string(),
            down: None,
            depends_on: Vec::new(),
            repeatable: false,
            timeout_ms: None,
            owner_app: "app_test".to_string(),
        }
    }

    /// LOW-1 — a privileged `CREATE ROLE evil` submitted with a **Platform**
    /// `ExecutorConfig` (the hostile/buggy caller scenario) is STILL `Denied` by
    /// `submit_migration`'s forced-Confined planner gate — NOT applied. Proves the
    /// two guard stages cannot drift to both-Platform: `submit` hard-wires
    /// `GuardConfig::confined(project_schema)` regardless of `cfg.trust`, so even a
    /// Platform `ExecutorConfig` (whose own `guard_config()` would ADMIT role
    /// management) cannot relax the submission gate. `Approval::Approved` is passed
    /// so the gate can never be the thing stopping it — only the guard can.
    #[compio::test]
    async fn platform_executor_config_does_not_relax_submit_create_role_still_denied() {
        let conn = pg().await;
        let admin = pg().await;
        let tok = token();
        let cfg = platform_cfg(&tok);
        let scfg = shadow_cfg(&tok);
        setup(&conn, &cfg).await;

        // Sanity: the config IS Platform — its own executor-path guard would ADMIT
        // role management. If this were Confined the test would be vacuous.
        assert_eq!(
            cfg.guard_config().trust(),
            crate::model::policy::TrustProfile::Platform,
            "the config under test must be Platform for the coupling proof to be meaningful"
        );

        let evil_role = format!("evil_{tok}");
        let outcome = submit_migration(
            &conn,
            &admin,
            &cfg,
            &scfg,
            submission("escalate", &format!("CREATE ROLE \"{evil_role}\" SUPERUSER")),
            &no_owners(),
            Approval::Approved,
            "app_test",
        )
        .await
        .expect("submit (no infra fault)");

        match &outcome {
            SubmissionOutcome::Denied { .. } => {}
            other => panic!(
                "a privileged CREATE ROLE must be Denied by submit's forced-Confined planner \
                 guard even with a Platform ExecutorConfig — got {other:?}. If this APPLIED, the \
                 submit-path two-guard coupling is BROKEN (submit's confined() is not load-bearing)."
            ),
        }
        // And it really did nothing: no role was created on the real cluster.
        assert!(
            !role_exists(&conn, &evil_role).await,
            "the denied CREATE ROLE must NOT have created a role on the real cluster"
        );

        teardown(&conn, &cfg).await;
    }

    /// LOW-1 (cross-schema arm) — the same coupling for a cross-schema write. A
    /// Platform config's `guard_config()` permits the whole `[project, public]`
    /// allowlist, so a `CREATE TABLE control.x` to a NON-allowlisted platform
    /// schema would be admitted under Platform — but `submit`'s forced-Confined
    /// gate (single-schema scope = the project schema only) still `Denied`s it.
    #[compio::test]
    async fn platform_executor_config_does_not_relax_submit_cross_schema_still_denied() {
        let conn = pg().await;
        let admin = pg().await;
        let tok = token();
        let cfg = platform_cfg(&tok);
        let scfg = shadow_cfg(&tok);
        setup(&conn, &cfg).await;

        let outcome = submit_migration(
            &conn,
            &admin,
            &cfg,
            &scfg,
            submission(
                "xschema",
                "CREATE TABLE control.stolen (id bigint PRIMARY KEY)",
            ),
            &no_owners(),
            Approval::Approved,
            "app_test",
        )
        .await
        .expect("submit (no infra fault)");

        match &outcome {
            SubmissionOutcome::Denied { .. } => {}
            other => panic!(
                "a cross-schema CREATE TABLE control.x must be Denied by submit's forced-Confined \
                 single-schema gate even with a Platform ExecutorConfig — got {other:?}"
            ),
        }

        teardown(&conn, &cfg).await;
    }

    // =======================================================================
    // Track A — the Trusted profile cannot weaken `submit_migration` either.
    //
    // Same coupling proof as the Platform arms above, but with a TRUSTED
    // `ExecutorConfig` (whose own `guard_config()` skips the deny-list ENTIRELY).
    // `submit_migration` STILL forces `GuardConfig::confined(project_schema)` for
    // the planner gate, so a privileged `up` is `Denied` regardless of `cfg.trust`.
    // If submit's forced-Confined gate ever drifted to honour `cfg.trust`, this
    // would APPLY the role and go RED. (The Trusted ctor is `pub(crate)` + the
    // token is `pub(crate)`, so this hostile config is only constructible
    // in-crate — an external crate can never even name it; pinned by T8.)
    // =======================================================================

    /// Mint a **Trusted** `ExecutorConfig` over a unique schema/meta — the hostile
    /// shape a buggy/hostile caller would forge to try to weaken the submit path.
    fn trusted_cfg(tok: &str) -> ExecutorConfig {
        let cap = OperatorCapability::for_test();
        let schema = format!("proj_{tok}");
        let mut c =
            ExecutorConfig::trusted(&cap, format!("prj_{tok}"), schema.clone());
        c.pg.meta_schema = format!("meta_{tok}");
        c.pg.statement_timeout = Duration::from_secs(30);
        c.pg.lock_timeout = Duration::from_secs(10);
        c
    }

    /// `submit_cannot_reach_trusted` — a privileged `CREATE ROLE evil` submitted
    /// with a **Trusted** `ExecutorConfig` is STILL `Denied` by
    /// `submit_migration`'s forced-Confined planner gate — NOT applied. Proves the
    /// submission ingress can never be flipped Trusted: `submit` hard-wires
    /// `GuardConfig::confined(project_schema)` independent of `cfg.trust`, so even a
    /// Trusted config (whose own `guard_config()` would skip the deny-list) cannot
    /// relax the submission gate. `Approval::Approved` is passed so only the guard
    /// can be the thing stopping it.
    #[compio::test]
    async fn submit_cannot_reach_trusted() {
        let conn = pg().await;
        let admin = pg().await;
        let tok = token();
        let cfg = trusted_cfg(&tok);
        let scfg = shadow_cfg(&tok);

        // Project schema + journal only (NO migrator role: Trusted runs as the
        // connecting role, so `provision_migrator` is not part of this path).
        conn.batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .expect("create project schema");
        ensure_journal(&conn, &cfg).await.expect("ensure journal");

        // Sanity: the config IS Trusted — its own executor-path guard would SKIP
        // the deny-list. If this were Confined the test would be vacuous.
        assert_eq!(
            cfg.guard_config().trust(),
            crate::model::policy::TrustProfile::Trusted,
            "the config under test must be Trusted for the coupling proof to be meaningful"
        );

        let evil_role = format!("evil_trusted_{tok}");
        let outcome = submit_migration(
            &conn,
            &admin,
            &cfg,
            &scfg,
            submission("escalate", &format!("CREATE ROLE \"{evil_role}\" SUPERUSER")),
            &no_owners(),
            Approval::Approved,
            "app_test",
        )
        .await
        .expect("submit (no infra fault)");

        match &outcome {
            SubmissionOutcome::Denied { .. } => {}
            other => panic!(
                "a privileged CREATE ROLE must be Denied by submit's forced-Confined planner \
                 guard even with a Trusted ExecutorConfig — got {other:?}. If this APPLIED, \
                 submit's forced confined() is NOT load-bearing and the Trusted profile leaked \
                 into the creator submission ingress."
            ),
        }
        assert!(
            !role_exists(&conn, &evil_role).await,
            "the denied CREATE ROLE must NOT have created a role on the real cluster"
        );

        // Teardown (no migrator role to deprovision).
        let _ = conn
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS \"{}\" CASCADE; DROP SCHEMA IF EXISTS \"{}\" CASCADE;",
                cfg.project_schema, cfg.pg.meta_schema
            ))
            .await;
    }
}
