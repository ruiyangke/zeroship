//! The optional backend capabilities: what an engine can do BEYOND applying a
//! migration.
//!
//! [`OnlineSchemaChange`] is here in full. Everything its `run_online` names — the
//! [`OnlineIntent`] it is handed, the [`OnlineError`] it refuses with, the
//! `Migration`/`BackfillSpec`/`Approval`/`ExecutorConfig` it works from — now sits
//! at or below this crate, so a vendor can implement it without naming the engine.
//!
//! `ShadowDryRun` could NOT follow, and its blockers are worth naming precisely
//! rather than deferring: `dry_run_declarative` takes a `DeclarativeDeployPlan`
//! (which holds the engine's `MigrationPlan` and a private policy field) and a
//! `DesiredSchema` (which holds the engine's `ResolvedInject`), and its
//! `SeedError` carries the engine's whole `EngineError`. All three are
//! ORCHESTRATION RESULTS — what the engine decided — not vocabulary a backend
//! speaks. Its neutral half is here anyway ([`ShadowConfig`] in,
//! [`DryRunReport`]/[`MigrationResult`] out) because those parts are backend
//! vocabulary and were never the obstacle.
//!
//! The engine re-exports every item here under its historical path.

use crate::advisory::Advisory;
use crate::approval::{Approval, ApprovalScope};
use crate::backfill::BackfillSpec;
use crate::conn::ExecutorConfig;
use crate::drift::StructuralDrift;
use crate::executor::{ApplyError, ApplyOutcome, LockMode};
use zero_migrate_ir::migration::{Migration, MigrationId};

/// A high-level online-migration intent the engine's `ExpandContractAuthor`
/// expands into an ordered, phased [`Migration`] sequence.
///
/// It is the NEUTRAL half of that authoring: `ExpandContractPlan` carries the
/// authored PostgreSQL DDL, and this carries the intent behind it, so the generic
/// declarative apply path can hand a backend what the rename MEANS rather than one
/// vendor's spelling of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnlineIntent {
    /// Rename column `from` → `to` (of type `ty`) on `table`, online, via the
    /// canonical expand-contract dual-write sequence.
    RenameColumn {
        /// The table the column lives on (bare; project-schema-qualified on emit).
        table: String,
        /// The existing column name.
        from: String,
        /// The new column name.
        to: String,
        /// The Postgres type of the column (emitted verbatim for the new column).
        ty: String,
    },
}

/// A failure from `OnlineSchemaChange::run_online`.
#[derive(Debug, thiserror::Error)]
pub enum OnlineError {
    /// The online expand needs explicit [`Approval::Approved`] (its backfill
    /// mutates data). Nothing was applied.
    #[error("online expand requires approval (the backfill mutates data) but none was given")]
    Approval,
    /// **Per-version approval scope (executor-layer defense in depth).** The
    /// expand is approved ([`Approval::Approved`]) but the rename's PLAN-GROUP
    /// version is NOT in the operator's reviewed
    /// [`ApprovalScope::Versions`] set — the
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
    Backfill(#[from] crate::backfill::BackfillError),
}

/// Where + how to provision a throwaway shadow database.
#[derive(Debug, Clone)]
pub struct ShadowConfig {
    /// A DSN for an admin connection whose role has `CREATEDB`.
    pub admin_dsn: String,
    /// The prefix for the throwaway database name.
    pub db_name_prefix: String,
}

/// The per-migration outcome of a dry-run apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationResult {
    /// The migration's version (`mig_...`).
    pub version: String,
    /// Whether this migration's `up` applied cleanly on the shadow.
    pub applied_ok: bool,
    /// The error when `applied_ok == false`.
    pub error: Option<String>,
}

/// The result of a dry-run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DryRunReport {
    /// Overall success.
    pub ok: bool,
    /// Per-migration outcome, in apply order.
    pub per_migration: Vec<MigrationResult>,
    /// Declarative resulting drift, if this was a declarative dry-run.
    pub resulting_drift: Option<StructuralDrift>,
    /// Operational advisories per migration version.
    pub advisories: Vec<(String, Vec<Advisory>)>,
    /// Whether the shadow DB + role teardown fully succeeded.
    pub teardown_ok: bool,
    /// The teardown failure message when `teardown_ok == false`.
    pub teardown_error: Option<String>,
}

/// The online schema-change capability — the dialect-neutral seam the generic
/// declarative apply path uses to drive a zero-downtime online operation.
#[allow(clippy::module_name_repetitions)]
pub trait OnlineSchemaChange {
    /// Drive one online intent's expand sequence.
    #[allow(clippy::too_many_arguments)]
    fn run_online<'a>(
        &'a self,
        intent: &'a OnlineIntent,
        expand: &'a [Migration],
        backfill: &'a BackfillSpec,
        approval: Approval,
        scope: &'a ApprovalScope,
        trigger_version: &'a MigrationId,
        cfg: &'a ExecutorConfig,
        applied_by: &'a str,
        lock_mode: LockMode,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ApplyOutcome, OnlineError>> + 'a>>;
}

/// The full ordered output of the engine's `ExpandContractAuthor::author` — the expand and
/// contract migrations for one online intent, with the `depends_on` chain wired.
///
/// The expand migrations ([`expand`](Self::expand)) and contract migrations
/// ([`contract`](Self::contract)) are exposed separately so a caller (the
/// control plane) can bundle the expand into deploy N and the contract into a
/// later deploy N+1 — the cross-deploy partition the engine gate enforces. The
/// flat [`all`](Self::all) view is the input to `MigrationEngine::plan`.
#[derive(Debug, Clone)]
pub struct ExpandContractPlan {
    /// The stable logical identity of the owning authored plan. IR lowering
    /// stamps this after the ordered plan is assembled; declarative callers that
    /// do not have an outer plan identity leave it `None` and retain the legacy
    /// first-expand-step fallback.
    pub plan_version: Option<MigrationId>,
    /// E1, E2, E3 in order (add column, dual-write trigger, backfill marker).
    pub expand: Vec<Migration>,
    /// C1, C2 in order (drop trigger/function, drop old column).
    pub contract: Vec<Migration>,
    /// The structured backfill spec for E3, to be driven by
    /// [`OnlineSchemaChange::run_online`]
    /// during orchestration.
    pub backfill: BackfillSpec,
    /// The version of the E2 trigger migration — the dependency every contract
    /// step and the gate keys on as "the expand". Carried out so the
    /// orchestrator / gate need not re-derive it.
    pub trigger_version: MigrationId,
    /// The neutral [`OnlineIntent`] this plan was authored from. Carried so the
    /// generic declarative apply path hands the **intent** (not the PG-DDL plan)
    /// to the [`OnlineSchemaChange`] seam — the Postgres impl ignores it and
    /// runs the pre-authored [`expand`](Self::expand) steps verbatim, while a
    /// future engine lowers the intent to its own native online DDL.
    pub intent: OnlineIntent,
}

impl ExpandContractPlan {
    /// All migrations (expand then contract) in apply order — the input to
    /// `MigrationEngine::plan`.
    #[must_use]
    pub fn all(&self) -> Vec<Migration> {
        self.expand.iter().chain(&self.contract).cloned().collect()
    }
}
