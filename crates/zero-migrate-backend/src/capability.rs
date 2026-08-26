//! The optional backend capabilities: what an engine can do BEYOND applying a
//! migration.
//!
//! [`OnlineSchemaChange`] is here in full. Everything its `run_online_backfill`
//! names - the [`OnlineIntent`] it is handed, the [`OnlineError`] it refuses with,
//! the `Migration`/`BackfillSpec`/`Approval`/`ExecutorConfig` it works from - now
//! sits at or below this crate, so a vendor can implement it without naming the
//! engine. That last clause used to be false in the one way that mattered: the
//! method took the whole expand sequence and called the ENGINE'S orchestrator to
//! apply it. The phases are inverted now (see [`OnlineSchemaChange`]), so the
//! vendor answers one phase and drives nothing.
//!
//! `ShadowDryRun` could NOT follow, and its blockers are worth naming precisely
//! rather than deferring: `dry_run_declarative` takes a `DeclarativeDeployPlan`
//! (which holds the engine's `MigrationPlan` and a private policy field) and a
//! `DesiredSchema` (which holds the engine's `ResolvedInject`), and refuses with
//! `DryRunError`, which is the engine's own. Each of those is an
//! ORCHESTRATION RESULT - what the engine decided - not vocabulary a backend
//! speaks. Its neutral half is here anyway ([`ShadowConfig`] in,
//! [`DryRunReport`]/[`MigrationResult`] out) because those parts are backend
//! vocabulary and were never the obstacle.
//!
//! The engine re-exports every item here under its historical path.

use crate::advisory::Advisory;
use crate::approval::{Approval, ApprovalScope};
use crate::backfill::{BackfillOutcome, BackfillSpec};
use crate::conn::ExecutorConfig;
use crate::drift::StructuralDrift;
use crate::executor::ApplyError;
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
    /// Rename column `from` -> `to` (of type `ty`) on `table`, online, via the
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

/// A failure from an online expand - raised by the engine while it drives the
/// expand's neutral phases, and by `OnlineSchemaChange::run_online_backfill` for
/// the vendor phase.
#[derive(Debug, thiserror::Error)]
pub enum OnlineError {
    /// The online expand needs explicit [`Approval::Approved`] (its backfill
    /// mutates data). Nothing was applied.
    #[error("online expand requires approval (the backfill mutates data) but none was given")]
    Approval,
    /// **Per-version approval scope (executor-layer defense in depth).** The
    /// expand is approved ([`Approval::Approved`]) but the rename's PLAN-GROUP
    /// version is NOT in the operator's reviewed
    /// [`ApprovalScope::Versions`] set - the
    /// executor-layer mirror of the engine's EXPAND scope gate, so a direct
    /// `run_online_backfill` caller cannot mirror data for a rename the operator
    /// never individually reviewed. Nothing was applied.
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
    /// The backfill step failed - E3 is NOT journaled, so the gate keeps the
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

/// The online schema-change capability - the dialect-neutral seam the generic
/// declarative apply path uses to drive a zero-downtime online operation.
///
/// # The engine drives the phases; this answers one of them
///
/// This seam used to expose a single method that took the WHOLE authored
/// expand sequence and drove it: it applied E1/E2 by calling the engine's
/// orchestrator (`apply_with_lock_backend`) back across the layer boundary,
/// tripped the engine's fault point, read the journal, and then ran the one thing
/// only a vendor can run - the paged data mirror. That was mutual recursion across
/// the seam the crate split exists to create: a vendor crate cannot depend on the
/// engine, so a vendor that calls the orchestrator can never leave it. Widening
/// the contract could not fix it, because the callee WAS the orchestrator.
///
/// The phases are inverted instead. The engine now owns every neutral phase -
/// the approval and scope gates, splitting the marker off the expand chain,
/// applying E1/E2 through its own apply path, the fault point, and the journal
/// read that decides resume-versus-skip - and calls DOWN here exactly once, for
/// the vendor-only phase. Nothing on this trait names the orchestrator.
#[allow(clippy::module_name_repetitions)]
pub trait OnlineSchemaChange {
    /// Mirror the pre-existing rows for one online intent - the single phase of
    /// an online expand that only the vendor can perform.
    ///
    /// The engine calls this only after it has applied the intent's structural
    /// expand steps (on the canonical rename: E1 `ADD COLUMN` and E2 the
    /// dual-write trigger) and proved `marker` is not already journaled complete.
    /// The trigger keeps NEW rows in sync from E2 onward; this mirrors the cohort
    /// that predates it, in committed pages, resumably.
    ///
    /// `marker` is the durable backfill step (E3) whose `version`/`checksum` key
    /// the progress row, so an interrupted run resumes from its last committed
    /// cursor rather than restarting. Completion is journaled by the backfill
    /// runner, not by the caller - the returned
    /// [`BackfillOutcome::complete`] tells the engine
    /// whether the cohort finished, and an incomplete run leaves the expand
    /// incomplete (so the contract stays blocked).
    ///
    /// `approval_key` is the rename's PLAN-GROUP version - the id the operator
    /// actually reviewed and the id the engine's own scope gate uses. It is passed
    /// resolved rather than re-derived here so the two gates cannot drift, and so
    /// this executor-layer mirror keys on the same version even when the expand
    /// chain is empty.
    ///
    /// # Errors
    /// [`OnlineError::Approval`] when `approval` is not
    /// [`Approval::Approved`] (the mirror mutates data);
    /// [`OnlineError::ApprovalNotScoped`] when `scope` does not admit
    /// `approval_key` - the executor-layer mirror of the engine's per-version
    /// gate, so a direct seam caller cannot mirror data for a rename the operator
    /// never individually reviewed. Nothing is written on either path.
    /// [`OnlineError::Backfill`] / [`OnlineError::Apply`] when the mirror itself
    /// fails; the run is resumable from its last committed cursor.
    #[allow(clippy::too_many_arguments)]
    fn run_online_backfill<'a>(
        &'a self,
        intent: &'a OnlineIntent,
        marker: &'a Migration,
        backfill: &'a BackfillSpec,
        approval: Approval,
        scope: &'a ApprovalScope,
        approval_key: &'a MigrationId,
        cfg: &'a ExecutorConfig,
        applied_by: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<BackfillOutcome, OnlineError>> + 'a>,
    >;
}

/// The full ordered output of the engine's `ExpandContractAuthor::author` - the expand and
/// contract migrations for one online intent, with the `depends_on` chain wired.
///
/// The expand migrations ([`expand`](Self::expand)) and contract migrations
/// ([`contract`](Self::contract)) are exposed separately so a caller (the
/// control plane) can bundle the expand into deploy N and the contract into a
/// later deploy N+1 - the cross-deploy partition the engine gate enforces. The
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
    /// The structured backfill spec for E3, handed to
    /// [`OnlineSchemaChange::run_online_backfill`]
    /// by the engine once it has applied the expand's structural steps.
    pub backfill: BackfillSpec,
    /// The version of the E2 trigger migration - the dependency every contract
    /// step and the gate keys on as "the expand". Carried out so the
    /// orchestrator / gate need not re-derive it.
    pub trigger_version: MigrationId,
    /// The neutral [`OnlineIntent`] this plan was authored from. Carried so the
    /// generic declarative apply path hands the **intent** (not the PG-DDL plan)
    /// to the [`OnlineSchemaChange`] seam - the Postgres impl ignores it and
    /// runs the pre-authored [`expand`](Self::expand) steps verbatim, while a
    /// future engine lowers the intent to its own native online DDL.
    pub intent: OnlineIntent,
}

impl ExpandContractPlan {
    /// All migrations (expand then contract) in apply order - the input to
    /// `MigrationEngine::plan`.
    #[must_use]
    pub fn all(&self) -> Vec<Migration> {
        self.expand.iter().chain(&self.contract).cloned().collect()
    }

    /// The version that NAMES this rename to an operator: E1's deterministic id,
    /// the same anchor the pending-contract obligation records and the reviewer
    /// approves, falling back to [`trigger_version`](Self::trigger_version) so the
    /// answer resolves UNCONDITIONALLY even for an empty expand chain (an internal
    /// invariant violation, which must not make a gate fall open).
    ///
    /// Every gate that has to say WHICH rename it is talking about reads it here,
    /// so the approval scope, the capability refusal, and the reviewer-facing list
    /// cannot come to name different things.
    #[must_use]
    pub fn group_version(&self) -> &MigrationId {
        self.expand
            .first()
            .map_or(&self.trigger_version, |e1| &e1.version)
    }
}

/// An OPTIONAL backend capability - something a plan step may need that a deploy
/// target is not obliged to provide.
///
/// A plan says what it requires ([`PlanStep::required_capability`]) and a backend
/// says what it provides ([`MigrationBackend::provides`]), so the engine can refuse
/// a plan the target cannot run as a UNIT, before its first step commits. Those two
/// answers are what makes this an enum rather than a per-case `is_some()` test at
/// the step that needs it: asked at the step, the question arrives after every
/// earlier step has already committed.
///
/// [`PlanStep::required_capability`]: crate::step::PlanStep::required_capability
/// [`MigrationBackend::provides`]: crate::backend::MigrationBackend::provides
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendCapability {
    /// [`OnlineSchemaChange`] - driving a zero-downtime online schema operation.
    /// A target without it has no expand-contract path at all; the equivalent
    /// change is lowered to some offline shape (a table rebuild) or refused at
    /// plan time.
    OnlineSchemaChange,
}

impl BackendCapability {
    /// The operator-facing name of this capability.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OnlineSchemaChange => "online schema change",
        }
    }
}

impl std::fmt::Display for BackendCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// -- The dual-write trigger's derived identities and its function body ---------
//
// A cross-deploy online rename is authored by the engine and executed by a vendor,
// and BOTH sides have to arrive at the same trigger. The executor deliberately
// DERIVES the trigger identity from the same `OnlineIntent` the trigger was authored
// from rather than accepting it from its caller - that is what stops a direct seam
// caller from pointing the mirror at a trigger the engine never wrote - and its
// backfill guard re-derives the function BODY to prove the live trigger is the one
// the author emitted. A second copy of either derivation on the vendor side would be
// two answers to one question, which is precisely the check being defeated.
//
// So they live here, in the vocabulary both sides already name (`OnlineIntent`,
// `ExpandContractPlan`), rather than in the engine where only one side could reach
// them. The byte budget is a PARAMETER: the engine passes the limiting registered
// backend's declared cap and a vendor passes its own descriptor's, so neither side
// hard-codes 63 and neither has to resolve the other.

/// Deterministically derive the dual-write function name for a rename, capped to
/// `max_bytes` - the identifier byte budget the caller's own vendor declares. Stable across re-authoring (so the
/// `down` and the orchestrator target the same object), with a hash suffix to
/// disambiguate over-long natural names - mirroring the authoring layer's
/// `index_name` discipline.
pub fn dual_write_fn_name(table: &str, from: &str, to: &str, max_bytes: usize) -> String {
    capped_name(&format!("zsdw_{table}_{from}_{to}_fn"), max_bytes)
}

/// Deterministically derive the dual-write trigger name (see
/// [`dual_write_fn_name`]).
pub fn dual_write_trg_name(table: &str, from: &str, to: &str, max_bytes: usize) -> String {
    capped_name(&format!("zsdw_{table}_{from}_{to}_trg"), max_bytes)
}

/// Cap a natural name to <= `max_bytes` deterministically: verbatim when it fits,
/// else a readable prefix + a 10-hex-char hash of the full natural name (so
/// distinct long inputs stay distinct). Identical algorithm to the authoring layer's
/// `index_name`, factored for the function/trigger names.
fn capped_name(natural: &str, max_bytes: usize) -> String {
    use sha2::{Digest, Sha256};
    if natural.len() <= max_bytes {
        return natural.to_string();
    }
    let digest = Sha256::digest(natural.as_bytes());
    let suffix = hex::encode(&digest[..5]); // 10 hex chars
    let budget = max_bytes - (1 + suffix.len());
    let mut prefix = String::with_capacity(budget);
    for ch in natural.chars() {
        if prefix.len() + ch.len_utf8() > budget {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix}_{suffix}")
}

// `pub fn dual_write_function_body(from_q, to_q) -> String` USED TO LIVE HERE, and it
// was twenty lines of PL/pgSQL - `TG_OP`, `NEW`, `OLD`, `IS DISTINCT FROM`,
// `RETURN NEW` - in the crate whose stated rule is that "nothing here spells a
// keyword, quotes an identifier or names a dialect".
//
// It passed this crate's own neutrality census the whole time, because that census
// looks for vendor NAMES and PL/pgSQL contains none of them. The name it would have
// caught, `LANGUAGE plpgsql`, was one crate UP, in the engine's
// `render::expand_contract`, wrapped around this body.
//
// Both halves are in `zero-migrate-postgres` now, behind
// `SchemaRenderer::dual_write_trigger`, next to the backfill guard that compares a
// live trigger's source against them. The engine asks the resolved renderer and
// spells nothing.
