//! Status + history read API — **read-only**.
//!
//! [`status`] answers "where is this project's schema?" — what's applied, what's
//! pending (in the exact order apply will run it), the current version, and what
//! has been rolled back. [`history`] returns the FULL append-only audit log
//! (every apply + every rollback event, in order), the tamper-evident record of
//! every state transition the journal ever saw.
//!
//! This module emits NO DDL and mutates nothing — it surfaces journal state. It
//! reuses the journal's NET-state reader ([`journal::applied`]) and the
//! executor's pending-ordering ([`crate::apply::executor::order_pending`]) so status's
//! view of "applied" and "pending" is byte-for-byte the view apply itself uses.

use std::collections::{BTreeSet, HashMap, HashSet};

// `status`/`history`/`read_status_snapshot` are generic over the `SqlSession` seam
// (`&D`), so a host (napi) driver can drive the "show me pending migrations" flow.
// They compile on the whole PG seam (`host-pg`).
#[cfg(pg_seam)]
use crate::driver::SqlSession;

use crate::apply::backend::BackfillProgressEntry;
use crate::apply::executor::{order_pending, ApplyError};
use crate::apply::journal::{
    self, AppliedEntry, HistoryEvent, JournalError, Phase, RolledBackEntry,
};
use crate::conn::ExecutorConfig;
use crate::model::migration::{Checksum, Migration, MigrationId};
use crate::render::plan::AppliedPlan;
use crate::render::step::{PlanStep, RenameStep};

/// The journal-visible kind of one executable step in an [`AppliedPlan`].
///
/// An online rename is deliberately flattened to the migrations its backend
/// actually journals. This keeps status aligned with apply instead of treating
/// the structured rename wrapper as a fictitious journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatusStepKind {
    /// An ordinary DDL migration.
    Ddl,
    /// A parameterized, one-shot data mutation.
    Dml,
    /// A resumable data backfill.
    Backfill,
    /// An import-time identity-generator reconciliation.
    SynchronizeIdentity,
    /// One PostgreSQL expand migration of an online rename.
    OnlineExpand,
    /// One PostgreSQL deferred-contract migration of an online rename.
    OnlineContract,
    /// The journal migration for an atomic SQLite table rebuild.
    SqliteRebuild,
}

impl PlanStatusStepKind {
    /// Stable wire spelling used by the Node status projection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ddl => "ddl",
            Self::Dml => "dml",
            Self::Backfill => "backfill",
            Self::SynchronizeIdentity => "synchronizeIdentity",
            Self::OnlineExpand => "onlineExpand",
            Self::OnlineContract => "onlineContract",
            Self::SqliteRebuild => "sqliteRebuild",
        }
    }
}

/// The expected journal identity of one lowered plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStatusManifestStep {
    /// Stable journal version for this executable step.
    pub version: MigrationId,
    /// User-facing step label.
    pub name: String,
    /// Authoritative checksum expected in the completed journal event.
    pub checksum: Checksum,
    /// The executable step kind.
    pub kind: PlanStatusStepKind,
    /// Whether this DDL identity is a genuine journaled repeatable.
    pub repeatable: bool,
    /// Cursor-stability mode for resumable backfills. `None` for every other
    /// executable step.
    pub cursor_stability_mode: Option<String>,
    /// The explicitly approved application/maintenance invariant name when the
    /// mode is `externalInvariant`.
    pub cursor_stability_invariant: Option<String>,
    /// Named operator assertion that concurrent identity allocation is quiesced.
    pub writes_quiesced: Option<String>,
}

/// A journal-reconcilable projection of one complete [`AppliedPlan`].
///
/// This projection is intentionally separate from `Migration`: DML and backfill
/// are real executable steps but cannot be represented by a flat migration list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStatusManifest {
    /// Stable logical identity of the authored plan.
    pub version: MigrationId,
    /// User-facing plan name.
    pub name: String,
    /// Authoritative checksum of the complete authored artifact.
    pub checksum: Checksum,
    /// Ordered journal-visible steps.
    pub steps: Vec<PlanStatusManifestStep>,
    /// Plan-level dependencies, expressed in logical plan identities.
    pub depends_on: Vec<MigrationId>,
}

impl PlanStatusManifest {
    /// Flatten a lowered plan to the exact identities its execution path journals.
    ///
    /// `depends_on` comes from [`crate::render::lower::LoweredArtifact`] rather
    /// than `AppliedPlan`: the guarded IR loader retains the author-declared
    /// dependency strings alongside the executable plan for the pending-contract
    /// interlock.
    ///
    /// # Errors
    /// Returns [`StatusError::PlanManifest`] when a dependency is not a valid
    /// migration/plan id.
    pub fn from_applied_plan(
        plan: &AppliedPlan,
        depends_on: &[String],
    ) -> Result<Self, StatusError> {
        fn migration_step(
            migration: &Migration,
            kind: PlanStatusStepKind,
        ) -> PlanStatusManifestStep {
            PlanStatusManifestStep {
                version: migration.version.clone(),
                name: migration.name.clone(),
                checksum: migration.checksum.clone(),
                kind,
                repeatable: migration.flags.repeatable,
                cursor_stability_mode: None,
                cursor_stability_invariant: None,
                writes_quiesced: None,
            }
        }

        let mut steps = Vec::new();
        for step in &plan.steps {
            match step {
                PlanStep::Ddl(migration) => {
                    steps.push(migration_step(migration, PlanStatusStepKind::Ddl));
                }
                PlanStep::Dml {
                    version,
                    checksum,
                    name,
                    ..
                } => steps.push(PlanStatusManifestStep {
                    version: version.clone(),
                    name: name.clone(),
                    checksum: checksum.clone(),
                    kind: PlanStatusStepKind::Dml,
                    repeatable: false,
                    cursor_stability_mode: None,
                    cursor_stability_invariant: None,
                    writes_quiesced: None,
                }),
                PlanStep::Backfill {
                    version,
                    checksum,
                    spec,
                } => {
                    let (mode, invariant) = match &spec.cursor_stability {
                        crate::model::ir::CursorStability::GuardUpdates => {
                            ("guardUpdates".to_string(), None)
                        }
                        crate::model::ir::CursorStability::ExternalInvariant { name } => {
                            ("externalInvariant".to_string(), Some(name.clone()))
                        }
                    };
                    steps.push(PlanStatusManifestStep {
                        version: version.clone(),
                        name: spec.name.clone(),
                        checksum: checksum.clone(),
                        kind: PlanStatusStepKind::Backfill,
                        repeatable: false,
                        cursor_stability_mode: Some(mode),
                        cursor_stability_invariant: invariant,
                        writes_quiesced: None,
                    });
                }
                PlanStep::AlterPrimaryKey(step) => {
                    steps.push(migration_step(&step.migration, PlanStatusStepKind::Ddl));
                }
                PlanStep::SynchronizeIdentity(step) => {
                    let mut status =
                        migration_step(&step.migration, PlanStatusStepKind::SynchronizeIdentity);
                    status.writes_quiesced = Some(step.writes_quiesced.clone());
                    steps.push(status);
                }
                PlanStep::OnlineRename(RenameStep::PgExpandContract(rename)) => {
                    steps.extend(
                        rename
                            .expand
                            .iter()
                            .map(|m| migration_step(m, PlanStatusStepKind::OnlineExpand)),
                    );
                    steps.extend(
                        rename
                            .contract
                            .iter()
                            .map(|m| migration_step(m, PlanStatusStepKind::OnlineContract)),
                    );
                }
                PlanStep::OnlineRename(RenameStep::SqliteRebuild(rebuild)) => {
                    steps.push(migration_step(
                        &rebuild.migration,
                        PlanStatusStepKind::SqliteRebuild,
                    ));
                }
            }
        }

        let depends_on = depends_on
            .iter()
            .map(|dependency| {
                MigrationId::parse(dependency).map_err(|error| {
                    StatusError::PlanManifest(format!(
                        "plan {} has invalid dependency {dependency:?}: {error}",
                        plan.version.as_str()
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            version: plan.version.clone(),
            name: plan.name.clone(),
            checksum: plan.checksum.clone(),
            steps,
            depends_on,
        })
    }
}

/// Net journal state of one expected plan step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatusStepState {
    /// No journal evidence exists for this step.
    Pending,
    /// A non-transactional `started` marker exists but completion does not.
    Inflight,
    /// The step has a matching completed journal event.
    Applied,
    /// The enclosing online rename was explicitly aborted, so this deferred
    /// contract step will never run for the authored plan.
    Aborted,
    /// The stable step id exists with a different checksum.
    Drifted,
}

impl PlanStatusStepState {
    /// Stable wire spelling used by the Node status projection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Inflight => "inflight",
            Self::Applied => "applied",
            Self::Aborted => "aborted",
            Self::Drifted => "drifted",
        }
    }
}

/// Reconciled status of one expected plan step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStatusStep {
    /// Stable journal version.
    pub version: MigrationId,
    /// User-facing step label.
    pub name: String,
    /// Executable step kind.
    pub kind: PlanStatusStepKind,
    /// Net state relative to the journal.
    pub state: PlanStatusStepState,
    /// The journal checksum when a row/marker exists. Kept for drift diagnostics.
    pub journal_checksum: Option<String>,
    /// Cursor-stability mode for a resumable backfill.
    pub cursor_stability_mode: Option<String>,
    /// Named external invariant prominently retained for operator status.
    pub cursor_stability_invariant: Option<String>,
    /// Named no-concurrent-writer assertion retained prominently for operators.
    pub writes_quiesced: Option<String>,
}

/// A net-applied or inflight journal identity absent from every supplied plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnexpectedJournalEntry {
    /// Stable journal identity that no supplied manifest owns.
    pub version: String,
    /// `Applied` for a completed event or `Inflight` for a started marker.
    pub state: PlanStatusStepState,
    /// Checksum retained for operator diagnostics.
    pub journal_checksum: String,
    /// Journaled migration kind, absent for an inflight marker.
    pub journal_kind: Option<journal::JournaledKind>,
}

/// Aggregate state of one supplied plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconciledPlanState {
    /// Every expected step has a matching completed event.
    Applied,
    /// Every non-contract step completed and at least one deferred online
    /// contract was explicitly aborted. This is terminal, but not applied.
    Aborted,
    /// No step has started.
    Pending,
    /// Some journal evidence exists, but not every step is complete.
    Partial,
    /// At least one stable step id has a mismatched checksum.
    Drifted,
    /// A supplied dependency has not completed.
    Blocked,
    /// A dependency plan was omitted, so the current journal schema cannot prove
    /// whether it completed. This is deliberately fail-closed.
    UnknownDependency,
}

impl ReconciledPlanState {
    /// Stable wire spelling used by the Node status projection.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Aborted => "aborted",
            Self::Pending => "pending",
            Self::Partial => "partial",
            Self::Drifted => "drifted",
            Self::Blocked => "blocked",
            Self::UnknownDependency => "unknownDependency",
        }
    }
}

/// Reconciled status of one supplied plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciledPlan {
    /// Stable logical plan id.
    pub version: MigrationId,
    /// User-facing plan name.
    pub name: String,
    /// Aggregate plan state.
    pub state: ReconciledPlanState,
    /// Ordered status of every journal-visible step.
    pub steps: Vec<PlanStatusStep>,
    /// Dependencies absent from the supplied manifest set. They cannot be mapped
    /// from current step-only journal rows back to a logical plan id.
    pub missing_dependencies: Vec<MigrationId>,
}

/// Plan-aware status over a supplied set of complete lowered plans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPlanStatus {
    /// Last fully applied supplied plan in canonical dependency/input order.
    pub current_version: Option<MigrationId>,
    /// Fully applied logical plan ids.
    pub applied: Vec<MigrationId>,
    /// Every runnable or blocked logical plan id, including drifted and unknown
    /// plans. Terminally aborted plans are excluded.
    pub pending: Vec<MigrationId>,
    /// Terminally aborted logical plan ids.
    pub aborted: Vec<MigrationId>,
    /// Versions whose latest journal event is a rollback.
    pub rolled_back: Vec<String>,
    /// Per-plan and per-step detail.
    pub plans: Vec<ReconciledPlan>,
    /// Completed or inflight journal identities absent from every supplied plan.
    pub unexpected_journal: Vec<UnexpectedJournalEntry>,
    /// Outstanding cross-deploy online contracts.
    pub pending_contracts: Vec<PendingContractStatus>,
    /// Plans blocked specifically by an outstanding online-contract dependency.
    pub blocked: Vec<BlockedPlan>,
}

/// One terminal pending-contract event used to reconcile an authored plan.
///
/// Terminal resolutions overlay their matching deferred C1/C2 steps because
/// the resolver journals one atomic migration instead of the individual steps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPendingContract {
    /// Durable E2 obligation key used to derive resolver-owned step ids.
    pub pending_version: String,
    /// Stable logical plan identity that owns the online rename.
    pub plan_version: String,
    /// Deferred C1/C2 journal identities owned by the obligation.
    pub contract_versions: Vec<String>,
    /// Terminal action recorded for the obligation.
    pub resolution: journal::Resolution,
}

/// Where a project's schema stands relative to a supplied migration set.
///
/// `applied` and `pending` are computed from **NET journal state** (a rolled-back
/// version is NOT applied and re-enters `pending`); `rolled_back` lists versions
/// whose latest event is a rollback. The three are derived from the same journal
/// read the executor uses, so status never disagrees with what apply would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// The highest net-applied version (the schema's current point), or `None`
    /// when nothing is applied. "Highest" is `UUIDv7`/`MigrationId` order — the
    /// same total order apply advances through.
    pub current_version: Option<MigrationId>,
    /// Net-applied entries (latest event = `completed`), in version order. Reuses
    /// [`journal::applied`]'s entries (version, checksum, phase).
    pub applied: Vec<AppliedEntry>,
    /// Versions in the supplied set that are NOT net-applied, in the SAME
    /// topological order apply will run them ([`crate::apply::executor::order_pending`]).
    /// A rolled-back version that is still in the set reappears here.
    pub pending: Vec<MigrationId>,
    /// Versions whose latest event is a rollback (net rolled-back), with the
    /// rollback event's detail.
    pub rolled_back: Vec<RolledBackEntry>,
    /// **Cross-deploy online-rename pending contracts.** Each outstanding
    /// obligation (EXPAND applied, contract C1/C2 not yet applied), flagged
    /// `orphaned` when the supplied migration set no longer carries the rename
    /// whose contract is pending. A distinct surfaced state — the operator must
    /// `resolve-pending` (or re-add the rename op for an orphan). Always empty on
    /// SQLite (no pending partition).
    pub pending_contracts: Vec<PendingContractStatus>,
    /// **Plans blocked on a pending-contract dependency.** A plan B with
    /// `depends_on: [A]` where A is an online rename whose contract is still
    /// pending is NOT applied yet but is a DISTINCT, retained
    /// `blocked-awaiting-approval` state (NOT failed); it unblocks once A's
    /// contract applies. Always empty on SQLite.
    pub blocked: Vec<BlockedPlan>,
}

/// One cross-deploy online-rename pending-contract obligation surfaced by
/// [`status`]. `orphaned` is computed against the supplied migration set:
/// an obligation whose `pending_version` is NOT among the supplied set's versions
/// is orphaned (the rename was removed after its EXPAND applied) and emits the
/// [`OrphanedPendingContract`](crate::plan::pending::OrphanedPendingContract) payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingContractStatus {
    /// The table whose online-rename contract is outstanding.
    pub table: String,
    /// The obligation key — the E2 trigger version of the pending rename.
    pub pending_version: String,
    /// `true` ⇒ the supplied set no longer carries this rename (orphaned);
    /// `false` ⇒ a routine outstanding obligation awaiting its contract.
    pub orphaned: bool,
}

/// One plan blocked on a pending-contract dependency surfaced by
/// [`status`] — a retained `blocked-awaiting-approval` state, not a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedPlan {
    /// The blocked plan's version (B).
    pub blocked: MigrationId,
    /// The dependency plan's version (A) whose contract is pending.
    pub dependency: MigrationId,
    /// The dependency's outstanding pending-contract version (the E2 trigger id).
    pub pending_version: String,
}

/// Error from the status/history read API.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    /// A journal read failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// Computing the pending order failed (an unsatisfiable `depends_on` or a
    /// dependency cycle in the supplied set) — surfaced, not swallowed, so status
    /// reports the same ordering fault apply would hit.
    #[error("pending ordering: {0}")]
    Ordering(#[source] ApplyError),
    /// Acquiring or releasing the backend's project lock failed.
    #[error("status project lock: {0}")]
    ProjectLock(#[source] ApplyError),
    /// A supplied plan manifest is ambiguous or cannot be reconciled safely.
    #[error("plan status manifest: {0}")]
    PlanManifest(String),
}

/// Reconcile complete lowered-plan manifests against net journal state.
///
/// The function is pure: backend readers call it after obtaining one net-state
/// snapshot, while unit tests exercise the exact same classification without a
/// database. Plans are ordered by their supplied dependency DAG; ties retain the
/// caller's order. Step order is always the order apply executes.
///
/// A dependency absent from `manifests` is not guessed from unrelated journal
/// step ids. Current journal rows do not carry their logical `plan_version`, so an
/// omitted dependency is surfaced as [`ReconciledPlanState::UnknownDependency`]
/// until the dependency plan is supplied.
///
/// # Errors
/// Returns [`StatusError::PlanManifest`] for duplicate plan/step identities or a
/// dependency cycle among supplied plans.
pub fn reconcile_applied_plans(
    manifests: &[PlanStatusManifest],
    journal_entries: &[AppliedEntry],
    outstanding: &[journal::PendingContract],
) -> Result<AppliedPlanStatus, StatusError> {
    reconcile_applied_plans_with_progress(manifests, journal_entries, &[], outstanding)
}

/// Reconcile complete lowered-plan manifests against journal and backfill
/// progress evidence.
///
/// A matching progress row means its stable step has started even when no
/// ordinary journal event exists yet. A missing or different progress checksum
/// is drift. A matching completed journal event remains authoritative over the
/// mutable progress table.
///
/// # Errors
///
/// Returns [`StatusError::PlanManifest`] for duplicate plan/step identities or a
/// dependency cycle among supplied plans.
pub fn reconcile_applied_plans_with_progress(
    manifests: &[PlanStatusManifest],
    journal_entries: &[AppliedEntry],
    backfill_progress: &[BackfillProgressEntry],
    outstanding: &[journal::PendingContract],
) -> Result<AppliedPlanStatus, StatusError> {
    reconcile_applied_plans_with_snapshot(
        manifests,
        journal_entries,
        backfill_progress,
        outstanding,
        &[],
    )
}

/// Reconcile one coherent backend snapshot, including net rollbacks.
///
/// This is the complete pure status fold used by the backend reader. The
/// convenience functions above omit mutable progress or rollback evidence when
/// their caller does not have it.
///
/// # Errors
/// Returns [`StatusError::PlanManifest`] for duplicate plan/step identities or a
/// dependency cycle among supplied plans.
pub fn reconcile_applied_plans_with_snapshot(
    manifests: &[PlanStatusManifest],
    journal_entries: &[AppliedEntry],
    backfill_progress: &[BackfillProgressEntry],
    outstanding: &[journal::PendingContract],
    rolled_back: &[String],
) -> Result<AppliedPlanStatus, StatusError> {
    reconcile_applied_plans_with_resolutions(
        manifests,
        journal_entries,
        backfill_progress,
        outstanding,
        &[],
        rolled_back,
    )
}

/// Reconcile one coherent backend snapshot, including terminal online-contract
/// resolutions.
///
/// A terminal resolution overlays the matching deferred contract steps with its
/// outcome. An abort removes the plan from the pending queue once every other
/// step is applied. Atomic resolver entries, plus legacy per-step abort entries,
/// are recognized as lifecycle evidence instead of unexpected journal data.
///
/// # Errors
/// Returns [`StatusError::PlanManifest`] for duplicate plan/step identities or a
/// dependency cycle among supplied plans.
pub fn reconcile_applied_plans_with_resolutions(
    manifests: &[PlanStatusManifest],
    journal_entries: &[AppliedEntry],
    backfill_progress: &[BackfillProgressEntry],
    outstanding: &[journal::PendingContract],
    resolved: &[ResolvedPendingContract],
    rolled_back: &[String],
) -> Result<AppliedPlanStatus, StatusError> {
    let order = order_plan_manifests(manifests)?;

    let mut seen_steps: HashMap<&str, (&str, &str)> = HashMap::new();
    for manifest in manifests {
        for step in &manifest.steps {
            if let Some((other_plan, other_name)) = seen_steps.insert(
                step.version.as_str(),
                (manifest.version.as_str(), step.name.as_str()),
            ) {
                return Err(StatusError::PlanManifest(format!(
                    "step version {} is shared by plan {} ({other_name:?}) and plan {} ({:?})",
                    step.version.as_str(),
                    other_plan,
                    manifest.version.as_str(),
                    step.name
                )));
            }
        }
    }

    let journal_by_version: HashMap<&str, &AppliedEntry> = journal_entries
        .iter()
        .map(|entry| (entry.version.as_str(), entry))
        .collect();
    let progress_by_version: HashMap<&str, &BackfillProgressEntry> = backfill_progress
        .iter()
        .map(|entry| (entry.version.as_str(), entry))
        .collect();
    let supplied_by_version: HashMap<&str, usize> = manifests
        .iter()
        .enumerate()
        .map(|(index, manifest)| (manifest.version.as_str(), index))
        .collect();
    let mut applied_contracts_by_plan: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut aborted_contracts_by_plan: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut known_resolver_versions = HashSet::new();
    for terminal in resolved {
        match terminal.resolution {
            journal::Resolution::Applied => {
                applied_contracts_by_plan
                    .entry(terminal.plan_version.as_str())
                    .or_default()
                    .extend(terminal.contract_versions.iter().map(String::as_str));
                known_resolver_versions.insert(
                    crate::render::expand_contract::resolve_pending_apply_atomic_version(
                        &terminal.pending_version,
                    )
                    .as_str()
                    .to_string(),
                );
            }
            journal::Resolution::Aborted => {
                aborted_contracts_by_plan
                    .entry(terminal.plan_version.as_str())
                    .or_default()
                    .extend(terminal.contract_versions.iter().map(String::as_str));
                known_resolver_versions.insert(
                    crate::render::expand_contract::resolve_pending_abort_atomic_version(
                        &terminal.pending_version,
                    )
                    .as_str()
                    .to_string(),
                );
                known_resolver_versions.extend(terminal.contract_versions.iter().enumerate().map(
                    |(ordinal, _)| {
                        crate::render::expand_contract::resolve_pending_abort_version(
                            &terminal.pending_version,
                            ordinal,
                        )
                        .as_str()
                        .to_string()
                    },
                ));
            }
        }
    }

    let mut states_by_version: HashMap<&str, ReconciledPlanState> = HashMap::new();
    let mut plans = Vec::with_capacity(manifests.len());

    for index in order {
        let manifest = &manifests[index];
        let mut steps = Vec::with_capacity(manifest.steps.len());
        for expected in &manifest.steps {
            let (mut state, journal_checksum) = match journal_by_version
                .get(expected.version.as_str())
            {
                Some(entry) if entry.phase == Phase::Completed => {
                    let journaled_repeatable = entry
                        .kind
                        .is_some_and(journal::JournaledKind::is_repeatable);
                    if journaled_repeatable != expected.repeatable {
                        (PlanStatusStepState::Drifted, Some(entry.checksum.clone()))
                    } else if entry.checksum != expected.checksum.as_str() {
                        if expected.repeatable {
                            // A changed genuine repeatable is the re-apply signal,
                            // not checksum tampering.
                            (PlanStatusStepState::Pending, Some(entry.checksum.clone()))
                        } else {
                            (PlanStatusStepState::Drifted, Some(entry.checksum.clone()))
                        }
                    } else {
                        (PlanStatusStepState::Applied, Some(entry.checksum.clone()))
                    }
                }
                Some(entry) if entry.checksum != expected.checksum.as_str() => {
                    (PlanStatusStepState::Drifted, Some(entry.checksum.clone()))
                }
                Some(entry) => (PlanStatusStepState::Inflight, Some(entry.checksum.clone())),
                None => match progress_by_version.get(expected.version.as_str()) {
                    Some(progress)
                        if progress.checksum.as_deref() != Some(expected.checksum.as_str()) =>
                    {
                        (PlanStatusStepState::Drifted, progress.checksum.clone())
                    }
                    Some(progress) if !progress.complete => {
                        (PlanStatusStepState::Inflight, progress.checksum.clone())
                    }
                    // Completion is not authoritative until the ordinary journal
                    // event exists. Treat this repairable gap as still inflight.
                    Some(progress) => (PlanStatusStepState::Inflight, progress.checksum.clone()),
                    None => (PlanStatusStepState::Pending, None),
                },
            };
            let explicitly_aborted = expected.kind == PlanStatusStepKind::OnlineContract
                && aborted_contracts_by_plan
                    .get(manifest.version.as_str())
                    .is_some_and(|versions| versions.contains(expected.version.as_str()));
            let explicitly_applied = expected.kind == PlanStatusStepKind::OnlineContract
                && applied_contracts_by_plan
                    .get(manifest.version.as_str())
                    .is_some_and(|versions| versions.contains(expected.version.as_str()));
            if state != PlanStatusStepState::Drifted {
                if explicitly_aborted {
                    state = PlanStatusStepState::Aborted;
                } else if explicitly_applied {
                    state = PlanStatusStepState::Applied;
                }
            }
            steps.push(PlanStatusStep {
                version: expected.version.clone(),
                name: expected.name.clone(),
                kind: expected.kind,
                state,
                journal_checksum,
                cursor_stability_mode: expected.cursor_stability_mode.clone(),
                cursor_stability_invariant: expected.cursor_stability_invariant.clone(),
                writes_quiesced: expected.writes_quiesced.clone(),
            });
        }

        let raw_state = if steps
            .iter()
            .any(|step| step.state == PlanStatusStepState::Drifted)
        {
            ReconciledPlanState::Drifted
        } else if steps.is_empty() {
            // Production IR lowering emits a journaled no-op anchor for an empty
            // selected dialect leg. An empty manifest therefore indicates a stale
            // or malformed producer and must not be treated as applied without any
            // checksum evidence.
            ReconciledPlanState::Pending
        } else if steps
            .iter()
            .all(|step| step.state == PlanStatusStepState::Applied)
        {
            ReconciledPlanState::Applied
        } else if steps
            .iter()
            .any(|step| step.state == PlanStatusStepState::Aborted)
            && steps.iter().all(|step| {
                matches!(
                    step.state,
                    PlanStatusStepState::Applied | PlanStatusStepState::Aborted
                )
            })
        {
            ReconciledPlanState::Aborted
        } else if steps
            .iter()
            .all(|step| step.state == PlanStatusStepState::Pending)
        {
            ReconciledPlanState::Pending
        } else {
            ReconciledPlanState::Partial
        };

        let missing_dependencies: Vec<MigrationId> = manifest
            .depends_on
            .iter()
            .filter(|dependency| !supplied_by_version.contains_key(dependency.as_str()))
            .cloned()
            .collect();
        let supplied_dependency_incomplete = manifest.depends_on.iter().any(|dependency| {
            supplied_by_version.contains_key(dependency.as_str())
                && states_by_version.get(dependency.as_str()) != Some(&ReconciledPlanState::Applied)
        });

        // Drift and an explicit abort are terminal facts about this plan. A
        // dependency problem must not hide either one or put an aborted plan
        // back into the runnable pending partition.
        let state = if matches!(
            raw_state,
            ReconciledPlanState::Drifted | ReconciledPlanState::Aborted
        ) {
            raw_state
        } else if !missing_dependencies.is_empty() {
            ReconciledPlanState::UnknownDependency
        } else if supplied_dependency_incomplete {
            ReconciledPlanState::Blocked
        } else {
            raw_state
        };
        states_by_version.insert(manifest.version.as_str(), state);
        plans.push(ReconciledPlan {
            version: manifest.version.clone(),
            name: manifest.name.clone(),
            state,
            steps,
            missing_dependencies,
        });
    }

    let applied: Vec<MigrationId> = plans
        .iter()
        .filter(|plan| plan.state == ReconciledPlanState::Applied)
        .map(|plan| plan.version.clone())
        .collect();
    let pending: Vec<MigrationId> = plans
        .iter()
        .filter(|plan| {
            !matches!(
                plan.state,
                ReconciledPlanState::Applied | ReconciledPlanState::Aborted
            )
        })
        .map(|plan| plan.version.clone())
        .collect();
    let aborted: Vec<MigrationId> = plans
        .iter()
        .filter(|plan| plan.state == ReconciledPlanState::Aborted)
        .map(|plan| plan.version.clone())
        .collect();
    let current_version = applied.last().cloned();
    let (pending_contracts, blocked) =
        derive_pending_contract_status_for_plans(outstanding, manifests);
    let mut unexpected_journal: Vec<UnexpectedJournalEntry> = journal_entries
        .iter()
        .filter(|entry| {
            !seen_steps.contains_key(entry.version.as_str())
                && !known_resolver_versions.contains(entry.version.as_str())
        })
        .map(|entry| UnexpectedJournalEntry {
            version: entry.version.clone(),
            state: if entry.phase == Phase::Completed {
                PlanStatusStepState::Applied
            } else {
                PlanStatusStepState::Inflight
            },
            journal_checksum: entry.checksum.clone(),
            journal_kind: entry.kind,
        })
        .collect();
    unexpected_journal.sort_by(|left, right| left.version.cmp(&right.version));

    Ok(AppliedPlanStatus {
        current_version,
        applied,
        pending,
        aborted,
        rolled_back: rolled_back.to_vec(),
        plans,
        unexpected_journal,
        pending_contracts,
        blocked,
    })
}

/// Read net journal state through a dialect backend and reconcile complete plans.
///
/// This is the plan-aware peer of [`status_via_backend`]. It deliberately accepts
/// [`PlanStatusManifest`] rather than `Migration`, so data-only and mixed plans are
/// never flattened away.
///
/// # Errors
/// Journal/bootstrap failures and the pure reconciliation errors documented by
/// [`reconcile_applied_plans`].
pub async fn status_plans_via_backend<B: crate::apply::backend::MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    manifests: &[PlanStatusManifest],
) -> Result<AppliedPlanStatus, StatusError> {
    backend.ensure_journal(cfg).await?;
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(StatusError::ProjectLock)?;

    let snapshot = status_plans_via_backend_locked(backend, cfg, manifests).await;

    let release = backend.release_project_lock(cfg).await;
    match (snapshot, release) {
        (Ok(status), Ok(())) => Ok(status),
        (Ok(_), Err(error)) => Err(StatusError::ProjectLock(error)),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => {
            tracing::warn!(
                error = %release_error,
                "zero-migrate: failed to release project lock after status snapshot error"
            );
            Err(error)
        }
    }
}

/// Read and reconcile plan status while the caller holds the project lock.
///
/// This is the host-adapter seam for callers that must keep catalog
/// introspection, manifest lowering, and journal reconciliation inside one lock
/// bracket. Callers must bootstrap the journal before taking the lock and must
/// release the lock on every exit path.
///
/// # Errors
/// The same journal and reconciliation errors as [`status_plans_via_backend`].
#[doc(hidden)]
pub async fn status_plans_via_backend_locked<B: crate::apply::backend::MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    manifests: &[PlanStatusManifest],
) -> Result<AppliedPlanStatus, StatusError> {
    // Every apply/rollback/progress/obligation mutation uses this same project
    // lock. Holding it across all readers makes their combined result one coherent
    // backend snapshot even on dialects without a shared read transaction seam.
    let entries = backend.applied(cfg).await?;
    let rolled_back = backend.net_rolled_back_versions(cfg).await?;
    let backfill_progress = backend.backfill_progress(cfg).await?;
    let (outstanding, resolved) = if let Some(pending_contracts) = backend.pending_contracts() {
        let outstanding = pending_contracts.outstanding_pending_contracts(cfg).await?;
        let resolved = pending_contracts
            .resolved_pending_contracts(cfg)
            .await?
            .into_iter()
            .map(|terminal| ResolvedPendingContract {
                pending_version: terminal.contract.pending_version,
                plan_version: terminal.contract.plan_version,
                contract_versions: terminal.contract.contract_versions,
                resolution: terminal.resolution,
            })
            .collect();
        (outstanding, resolved)
    } else {
        (Vec::new(), Vec::new())
    };
    reconcile_applied_plans_with_resolutions(
        manifests,
        &entries,
        &backfill_progress,
        &outstanding,
        &resolved,
        &rolled_back,
    )
}

/// Stable topological ordering of supplied plans. Among plans whose supplied
/// dependencies are satisfied, caller order wins. Dependencies absent from the
/// supplied set are intentionally omitted from the graph and later classified as
/// `unknownDependency`; they are not assumed satisfied.
fn order_plan_manifests(manifests: &[PlanStatusManifest]) -> Result<Vec<usize>, StatusError> {
    let mut by_version: HashMap<&str, usize> = HashMap::new();
    for (index, manifest) in manifests.iter().enumerate() {
        if let Some(other) = by_version.insert(manifest.version.as_str(), index) {
            return Err(StatusError::PlanManifest(format!(
                "duplicate plan version {} at supplied positions {other} and {index}",
                manifest.version.as_str()
            )));
        }
    }

    let mut indegree = vec![0usize; manifests.len()];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); manifests.len()];
    for (index, manifest) in manifests.iter().enumerate() {
        let mut seen = HashSet::new();
        for dependency in &manifest.depends_on {
            if !seen.insert(dependency.as_str()) {
                continue;
            }
            if let Some(&dependency_index) = by_version.get(dependency.as_str()) {
                indegree[index] += 1;
                dependents[dependency_index].push(index);
            }
        }
    }

    let mut ready: BTreeSet<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect();
    let mut ordered = Vec::with_capacity(manifests.len());
    while let Some(index) = ready.pop_first() {
        ordered.push(index);
        for &dependent in &dependents[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.insert(dependent);
            }
        }
    }

    if ordered.len() != manifests.len() {
        let cyclic: Vec<&str> = indegree
            .iter()
            .enumerate()
            .filter_map(|(index, degree)| {
                (*degree > 0).then_some(manifests[index].version.as_str())
            })
            .collect();
        return Err(StatusError::PlanManifest(format!(
            "dependency cycle among supplied plans: {}",
            cyclic.join(", ")
        )));
    }
    Ok(ordered)
}

/// Compute the [`MigrationStatus`] of `migrations` against the journal — what is
/// applied, pending, current, and rolled back (design scenarios 45/46).
///
/// **Read-only.** Bootstraps the journal idempotently (so a fresh project reports
/// cleanly), then derives every field from NET journal state. `applied` reuses
/// [`journal::applied`]; `pending` reuses the executor's [`order_pending`] (same
/// topo order as apply); `current_version` is the highest net-applied version;
/// `rolled_back` is from [`journal::net_rolled_back`].
///
/// **Consistent snapshot.** The two journal reads (`applied` and
/// `net_rolled_back`) run inside ONE `REPEATABLE READ READ ONLY` transaction, so a
/// concurrent apply/rollback committing between them can never split the view into
/// an inconsistent applied-vs-rolled-back bucketing. The transaction is driven
/// explicitly over the shared `&Client` (`BEGIN … COMMIT`), mirroring how the
/// executor drives its apply/rollback transactions. `ensure_journal` (which emits
/// `CREATE … IF NOT EXISTS` DDL) runs BEFORE the snapshot, since a `READ ONLY`
/// transaction forbids DDL and bootstrap must stay idempotent regardless.
///
/// "Current" = highest-VERSION net-applied (`UUIDv7`/`MigrationId` total order),
/// NOT most-recently-applied. The two coincide unless a `depends_on` graph drove
/// apply order away from version order.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection. This function takes whatever
/// [`Client`] it is handed and never elevates to the `migrator` role; schema
/// scoping by `cfg.meta_schema` keeps reads bound to this project's journal, but
/// the privilege of the connection is the caller's obligation.
///
/// # Errors
/// - [`StatusError::Journal`] on a journal read/bootstrap failure.
/// - [`StatusError::Ordering`] if the supplied set's `depends_on` is
///   unsatisfiable or cyclic (the same fault apply would surface).
#[cfg(pg_seam)]
pub async fn status<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    journal::ensure_journal(conn, cfg).await?;

    // One consistent snapshot over both journal reads (applied + rolled_back). A
    // REPEATABLE READ READ ONLY txn pins a single MVCC view, so a concurrent
    // commit between the two reads can't produce a split bucket view.
    conn.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|e| StatusError::Journal(JournalError::Db(e.into())))?;
    let snapshot = read_status_snapshot(conn, cfg, migrations).await;
    finish_status_snapshot(conn, snapshot).await
}

#[cfg(pg_seam)]
async fn finish_status_snapshot<D: SqlSession, T>(
    conn: &D,
    snapshot: Result<T, StatusError>,
) -> Result<T, StatusError> {
    match snapshot {
        Ok(status) => {
            if let Err(commit_error) = conn.batch("COMMIT").await {
                if let Err(rollback_error) = conn.batch("ROLLBACK").await {
                    tracing::warn!(
                        error = %rollback_error,
                        "zero-migrate: PostgreSQL status rollback after COMMIT failure failed"
                    );
                }
                return Err(StatusError::Journal(JournalError::Db(commit_error.into())));
            }
            Ok(status)
        }
        Err(snapshot_error) => {
            if let Err(rollback_error) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback_error,
                    "zero-migrate: PostgreSQL status snapshot rollback failed"
                );
            }
            Err(snapshot_error)
        }
    }
}

#[cfg(all(test, pg_seam))]
mod legacy_snapshot_transaction_tests {
    use super::*;
    use crate::driver::{Bind, DbError, Row};
    use std::cell::{Cell, RefCell};

    struct RecordingSession {
        batches: RefCell<Vec<String>>,
        fail_commit: Cell<bool>,
    }

    impl RecordingSession {
        fn new(fail_commit: bool) -> Self {
            Self {
                batches: RefCell::new(Vec::new()),
                fail_commit: Cell::new(fail_commit),
            }
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.batches.borrow_mut().push(sql.to_string());
            if sql == "COMMIT" && self.fail_commit.get() {
                return Err(DbError::message("injected status COMMIT failure"));
            }
            Ok(())
        }

        async fn exec(&self, _sql: &str, _binds: &[Bind]) -> Result<u64, DbError> {
            Err(DbError::message("unexpected exec"))
        }

        async fn exec_text(&self, _sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            Err(DbError::message("unexpected exec_text"))
        }

        async fn query(&self, _sql: &str, _binds: &[Bind]) -> Result<Vec<Row>, DbError> {
            Err(DbError::message("unexpected query"))
        }

        async fn query_one(&self, _sql: &str, _binds: &[Bind]) -> Result<Row, DbError> {
            Err(DbError::message("unexpected query_one"))
        }
    }

    #[compio::test]
    async fn snapshot_error_rolls_back_instead_of_committing() {
        let conn = RecordingSession::new(false);
        let result: Result<(), StatusError> = finish_status_snapshot(
            &conn,
            Err(StatusError::PlanManifest(
                "injected snapshot failure".into(),
            )),
        )
        .await;

        assert!(matches!(result, Err(StatusError::PlanManifest(_))));
        assert_eq!(conn.batches.borrow().as_slice(), ["ROLLBACK"]);
    }

    #[compio::test]
    async fn commit_failure_is_surfaced_and_cleanup_is_attempted() {
        let conn = RecordingSession::new(true);
        let error = finish_status_snapshot(&conn, Ok::<_, StatusError>(()))
            .await
            .expect_err("COMMIT failure must not be swallowed");

        assert!(error.to_string().contains("injected status COMMIT failure"));
        assert_eq!(conn.batches.borrow().as_slice(), ["COMMIT", "ROLLBACK"]);
    }
}

/// Backend-generic [`status`]: compute the [`MigrationStatus`] over ANY
/// [`MigrationBackend`](crate::apply::backend::MigrationBackend), reading net journal
/// state through the trait (`ensure_journal` + `applied` + `superseded_versions`)
/// rather than a PG `&Client`. This is the multi-engine peer of [`status`] — the
/// public CLI's SQLite leg routes here, where the PG leg keeps the
/// `REPEATABLE READ READ ONLY` snapshot path above (the SQLite actor serializes
/// structurally, so a single net-state read is already a consistent view).
///
/// `applied` / `pending` / `current_version` are derived with the SAME rules and
/// the SAME [`order_pending`] the executor uses, so status never disagrees with
/// what apply would do. `rolled_back` is left empty here because the neutral
/// trait exposes only rollback version ids, not the full [`RolledBackEntry`]
/// detail required by [`MigrationStatus`]. A rolled-back version is already
/// absent from `applied` net-state, so it still correctly re-enters `pending`.
///
/// # Errors
/// - [`StatusError::Journal`] on a journal bootstrap/read failure.
/// - [`StatusError::Ordering`] if the set's `depends_on` is unsatisfiable/cyclic
///   (the same fault apply would surface).
pub async fn status_via_backend<B: crate::apply::backend::MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    backend.ensure_journal(cfg).await?;
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(StatusError::ProjectLock)?;

    let snapshot = status_via_backend_locked(backend, cfg, migrations).await;
    let release = backend.release_project_lock(cfg).await;
    match (snapshot, release) {
        (Ok(status), Ok(())) => Ok(status),
        (Ok(_), Err(error)) => Err(StatusError::ProjectLock(error)),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => {
            tracing::warn!(
                error = %release_error,
                "zero-migrate: failed to release project lock after status error"
            );
            Err(error)
        }
    }
}

/// Reconcile migration-only status while the caller holds the backend's project
/// lock. Callers must bootstrap the journal before acquiring the lock and release
/// the lock on every exit path.
///
/// This is the dialect-neutral host-adapter seam for journal-only status. It emits
/// SQL only through the supplied backend, so a MySQL session never receives the
/// PostgreSQL status transaction or journal queries.
///
/// # Errors
/// The same journal and ordering errors as [`status_via_backend`].
#[doc(hidden)]
pub async fn status_via_backend_locked<B: crate::apply::backend::MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    let entries = backend.applied(cfg).await?;
    let applied: Vec<AppliedEntry> = entries
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .cloned()
        .collect();

    let current_version = applied
        .iter()
        .filter_map(|e| MigrationId::parse(&e.version).ok())
        .max();

    let completed: HashMap<&str, &AppliedEntry> =
        applied.iter().map(|e| (e.version.as_str(), e)).collect();
    let journal_superseded = backend.superseded_versions(cfg).await?;
    let superseded_owned =
        crate::apply::executor::compute_superseded(migrations, &journal_superseded);
    let superseded: std::collections::HashSet<&str> =
        superseded_owned.iter().map(String::as_str).collect();
    let ordered =
        order_pending(migrations, &completed, &superseded).map_err(StatusError::Ordering)?;
    let pending: Vec<MigrationId> = ordered.iter().map(|m| m.version.clone()).collect();

    // Outstanding pending contracts + blocked plans, read
    // through the neutral capability. If the backend has no pending-contract
    // capability, this is structurally empty and can never false-gate a deploy.
    let outstanding = if let Some(pending_contracts) = backend.pending_contracts() {
        pending_contracts.outstanding_pending_contracts(cfg).await?
    } else {
        Vec::new()
    };
    let (pending_contracts, blocked) = derive_pending_contract_status(&outstanding, migrations);

    Ok(MigrationStatus {
        current_version,
        applied,
        pending,
        // The neutral trait exposes only rollback ids, not the full detail this
        // legacy reply shape requires. A rolled-back version is already dropped
        // from `applied` net-state (it reappears in `pending`).
        rolled_back: Vec::new(),
        pending_contracts,
        blocked,
    })
}

/// The body of [`status`]'s consistent-snapshot read: both journal reads + the
/// derived fields, run inside the caller's open `REPEATABLE READ READ ONLY` txn.
#[cfg(pg_seam)]
async fn read_status_snapshot<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    let entries = journal::applied(conn, cfg).await?;
    // NET-applied entries only (drop lone `started` inflight markers — those are
    // crash-recovery keys, not settled applied state).
    let applied: Vec<AppliedEntry> = entries
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .cloned()
        .collect();

    // current_version = highest net-applied version (MigrationId order).
    let current_version = applied
        .iter()
        .filter_map(|e| MigrationId::parse(&e.version).ok())
        .max();

    // pending = set − net-applied − superseded, in the SAME order apply uses.
    // order_pending wants a map of completed entries keyed by version; build it from
    // the net-applied entries (NOT the raw rows — a rolled-back version must count
    // as pending, and net state already excludes it).
    let completed: HashMap<&str, &AppliedEntry> =
        applied.iter().map(|e| (e.version.as_str(), e)).collect();
    // Supersession (squash): a version superseded by a net-applied squash OR
    // by an in-set squash is NOT pending — status must agree with apply. Reuses the
    // executor's `compute_superseded` so the two views never diverge.
    let journal_superseded = journal::superseded_versions(conn, cfg).await?;
    let superseded_owned =
        crate::apply::executor::compute_superseded(migrations, &journal_superseded);
    let superseded: std::collections::HashSet<&str> =
        superseded_owned.iter().map(String::as_str).collect();
    let ordered =
        order_pending(migrations, &completed, &superseded).map_err(StatusError::Ordering)?;
    let pending: Vec<MigrationId> = ordered.iter().map(|m| m.version.clone()).collect();

    let rolled_back = journal::net_rolled_back(conn, cfg).await?;

    // Surface the outstanding cross-deploy pending contracts
    // (with orphan detection) + the plans blocked on a pending-contract
    // dependency. Read inside this same REPEATABLE READ READ ONLY snapshot so the
    // obligation view is consistent with the applied/rolled-back buckets.
    let outstanding = journal::outstanding_pending_contracts(conn, cfg).await?;
    let (pending_contracts, blocked) = derive_pending_contract_status(&outstanding, migrations);

    Ok(MigrationStatus {
        current_version,
        applied,
        pending,
        rolled_back,
        pending_contracts,
        blocked,
    })
}

/// Derive the [`MigrationStatus::pending_contracts`] + [`MigrationStatus::blocked`]
/// fields from the OUTSTANDING obligation set and the supplied migration set.
/// Pure — shared by the PG snapshot path and any
/// backend path that can read the obligation set.
///
/// - **Orphan:** an obligation whose **`plan_version`** is NOT
///   among the supplied set's versions is orphaned (the rename op was removed
///   after its EXPAND applied). Fail-closed: it is surfaced as a distinct state,
///   never silently dropped.
/// - **Blocked:** a supplied migration B whose `depends_on` references an
///   outstanding obligation's **`plan_version`** (the dependency A's plan-group
///   version) is blocked until A's contract applies — a retained
///   `blocked-awaiting-approval` state.
///
/// **Why `plan_version`, not `pending_version`.** The obligation key
/// `pending_version` is the E2 trigger SUB-step id — a deep id that no plan-level
/// migration set ever exposes (a plan is ONE `Migration` per file,
/// keyed on the file/plan version, never on a rename's interior sub-step). Keying
/// orphan on `pending_version` made EVERY outstanding obligation falsely
/// `orphaned`, and keying `blocked` on it made the blocked state NEVER fire (an
/// author declares `depends_on` on plan A's PLAN version, not A's E2 sub-version).
/// `plan_version` is the rename's plan-group version (E1-anchored, deterministic),
/// which a re-lowered IR's `lower_plan().version` reproduces and a
/// `depends_on: [A]` references — so both checks key on the identity the supplied
/// set actually carries.
fn derive_pending_contract_status(
    outstanding: &[journal::PendingContract],
    migrations: &[Migration],
) -> (Vec<PendingContractStatus>, Vec<BlockedPlan>) {
    let supplied: std::collections::HashSet<&str> =
        migrations.iter().map(|m| m.version.as_str()).collect();

    let pending_contracts: Vec<PendingContractStatus> = outstanding
        .iter()
        .map(|pc| PendingContractStatus {
            table: pc.table.clone(),
            pending_version: pc.pending_version.clone(),
            // Orphaned when the supplied set no longer carries this rename's
            // PLAN version — the stable identity the loaded set
            // exposes, NOT the interior E2 sub-version.
            orphaned: !supplied.contains(pc.plan_version.as_str()),
        })
        .collect();

    // Map every outstanding obligation's PLAN version → its E2 obligation key, so a
    // `depends_on: [A's plan version]` resolves to the pending_version the blocked
    // payload reports (the operator runs `resolve-pending` against pending_version).
    let outstanding_by_plan: std::collections::HashMap<&str, &str> = outstanding
        .iter()
        .map(|pc| (pc.plan_version.as_str(), pc.pending_version.as_str()))
        .collect();
    let mut blocked = Vec::new();
    for m in migrations {
        for dep in &m.depends_on {
            if let Some(pending_version) = outstanding_by_plan.get(dep.as_str()) {
                blocked.push(BlockedPlan {
                    blocked: m.version.clone(),
                    dependency: dep.clone(),
                    pending_version: (*pending_version).to_string(),
                });
            }
        }
    }
    (pending_contracts, blocked)
}

/// Plan-manifest peer of [`derive_pending_contract_status`]. Logical plan ids and
/// dependencies are available directly, so online-contract orphan/block reporting
/// remains correct without projecting data steps to fake migrations.
fn derive_pending_contract_status_for_plans(
    outstanding: &[journal::PendingContract],
    manifests: &[PlanStatusManifest],
) -> (Vec<PendingContractStatus>, Vec<BlockedPlan>) {
    let supplied: HashSet<&str> = manifests
        .iter()
        .map(|manifest| manifest.version.as_str())
        .collect();
    let pending_contracts = outstanding
        .iter()
        .map(|contract| PendingContractStatus {
            table: contract.table.clone(),
            pending_version: contract.pending_version.clone(),
            orphaned: !supplied.contains(contract.plan_version.as_str()),
        })
        .collect();

    let outstanding_by_plan: HashMap<&str, &str> = outstanding
        .iter()
        .map(|contract| {
            (
                contract.plan_version.as_str(),
                contract.pending_version.as_str(),
            )
        })
        .collect();
    let mut blocked = Vec::new();
    for manifest in manifests {
        for dependency in &manifest.depends_on {
            if let Some(pending_version) = outstanding_by_plan.get(dependency.as_str()) {
                blocked.push(BlockedPlan {
                    blocked: manifest.version.clone(),
                    dependency: dependency.clone(),
                    pending_version: (*pending_version).to_string(),
                });
            }
        }
    }
    (pending_contracts, blocked)
}

/// Read the FULL append-only event log (every apply + rollback event) in
/// `event_seq` order — the audit trail.
///
/// **Read-only.** Unlike [`status`], this does NOT collapse to net state: a
/// version applied → rolled back → re-applied shows all three events. Bootstraps
/// the journal idempotently first so a fresh project returns an empty log.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection. Like [`status`] and
/// [`snapshot_schema`](crate::snapshot_schema), this takes whatever [`Client`] it
/// is handed and never elevates to the `migrator` role; the reads are scoped to
/// `cfg.meta_schema`, but the connection's privilege is the caller's obligation.
///
/// # Errors
/// [`StatusError::Journal`] on a journal read/bootstrap failure.
#[cfg(pg_seam)]
pub async fn history<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
) -> Result<Vec<HistoryEvent>, StatusError> {
    journal::ensure_journal(conn, cfg).await?;
    Ok(journal::history(conn, cfg).await?)
}

#[cfg(test)]
mod plan_status_tests {
    use super::*;
    use crate::model::backfill::BackfillSpec;
    use crate::model::migration::{ChecksumInput, MigrationFlags};
    use crate::render::lower::{IrAuthor, LiveSchema};
    use crate::render::step::DialectScope;
    use crate::schema::query::SqlDialect;

    fn id(seed: &str) -> MigrationId {
        MigrationId::derive("status_test", seed.as_bytes())
    }

    fn checksum(body: &str) -> Checksum {
        let flags = MigrationFlags::default();
        Checksum::of(&ChecksumInput {
            up: body,
            down: None,
            flags: &flags,
            owner_app: "app_status",
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        })
    }

    fn step(seed: &str, kind: PlanStatusStepKind, anchor: &Checksum) -> PlanStatusManifestStep {
        PlanStatusManifestStep {
            version: id(seed),
            name: seed.to_string(),
            checksum: anchor.clone(),
            kind,
            repeatable: false,
            cursor_stability_mode: None,
            cursor_stability_invariant: None,
            writes_quiesced: None,
        }
    }

    fn manifest(
        seed: &str,
        steps: Vec<PlanStatusManifestStep>,
        depends_on: Vec<MigrationId>,
    ) -> PlanStatusManifest {
        PlanStatusManifest {
            version: id(&format!("plan_{seed}")),
            name: seed.to_string(),
            checksum: checksum(seed),
            steps,
            depends_on,
        }
    }

    fn journal(step: &PlanStatusManifestStep, phase: Phase) -> AppliedEntry {
        AppliedEntry {
            version: step.version.as_str().to_string(),
            checksum: step.checksum.as_str().to_string(),
            phase,
            kind: None,
        }
    }

    fn progress(
        step: &PlanStatusManifestStep,
        checksum: Option<&Checksum>,
        complete: bool,
    ) -> BackfillProgressEntry {
        BackfillProgressEntry {
            version: step.version.as_str().to_string(),
            checksum: checksum.map(|value| value.as_str().to_string()),
            complete,
        }
    }

    fn resolved_abort(plan: &PlanStatusManifest) -> ResolvedPendingContract {
        let contract_versions = plan
            .steps
            .iter()
            .filter(|step| step.kind == PlanStatusStepKind::OnlineContract)
            .map(|step| step.version.as_str().to_string())
            .collect();
        ResolvedPendingContract {
            pending_version: id("abort_trigger").as_str().to_string(),
            plan_version: plan.version.as_str().to_string(),
            contract_versions,
            resolution: journal::Resolution::Aborted,
        }
    }

    fn resolved_apply(plan: &PlanStatusManifest) -> ResolvedPendingContract {
        let mut resolved = resolved_abort(plan);
        resolved.resolution = journal::Resolution::Applied;
        resolved
    }

    fn atomic_resolver_journal(
        resolved: &ResolvedPendingContract,
        resolution: journal::Resolution,
    ) -> AppliedEntry {
        let version = match resolution {
            journal::Resolution::Applied => {
                crate::render::expand_contract::resolve_pending_apply_atomic_version(
                    &resolved.pending_version,
                )
            }
            journal::Resolution::Aborted => {
                crate::render::expand_contract::resolve_pending_abort_atomic_version(
                    &resolved.pending_version,
                )
            }
        };
        AppliedEntry {
            version: version.as_str().to_string(),
            checksum: checksum(resolution.as_str()).as_str().to_string(),
            phase: Phase::Completed,
            kind: Some(crate::apply::journal::JournaledKind::Apply),
        }
    }

    fn resolver_abort_journal(resolved: &ResolvedPendingContract) -> Vec<AppliedEntry> {
        resolved
            .contract_versions
            .iter()
            .enumerate()
            .map(|(ordinal, _)| AppliedEntry {
                version: crate::render::expand_contract::resolve_pending_abort_version(
                    &resolved.pending_version,
                    ordinal,
                )
                .as_str()
                .to_string(),
                checksum: checksum(&format!("abort {ordinal}")).as_str().to_string(),
                phase: Phase::Completed,
                kind: Some(crate::apply::journal::JournaledKind::Apply),
            })
            .collect()
    }

    fn repeatable_manifest(seed: &str, checksum: Checksum) -> PlanStatusManifest {
        let migration = Migration {
            version: id(&format!("{seed}_step")),
            name: seed.to_string(),
            up: format!("CREATE OR REPLACE VIEW {seed} AS SELECT 1"),
            down: None,
            checksum: checksum.clone(),
            flags: MigrationFlags {
                repeatable: true,
                ..MigrationFlags::default()
            },
            owner_app: "app_status".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        };
        let plan = AppliedPlan {
            version: id(&format!("{seed}_plan")),
            name: seed.to_string(),
            steps: vec![PlanStep::Ddl(migration)],
            database_requirements: Default::default(),
            checksum,
            flags: MigrationFlags::default(),
            dialect_scope: DialectScope::Both,
            rollbackable: false,
            owner_app: "app_status".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        };
        PlanStatusManifest::from_applied_plan(&plan, &[]).expect("repeatable manifest")
    }

    #[test]
    fn mixed_plan_reconciles_every_step_and_only_completes_when_all_are_applied() {
        let anchor = checksum("mixed");
        let plan = manifest(
            "mixed",
            vec![
                step("ddl", PlanStatusStepKind::Ddl, &anchor),
                step("dml", PlanStatusStepKind::Dml, &anchor),
                step("backfill", PlanStatusStepKind::Backfill, &anchor),
            ],
            Vec::new(),
        );
        let partial_journal = vec![
            journal(&plan.steps[0], Phase::Completed),
            journal(&plan.steps[1], Phase::Completed),
        ];
        let partial = reconcile_applied_plans(std::slice::from_ref(&plan), &partial_journal, &[])
            .expect("partial status");
        assert_eq!(partial.plans[0].state, ReconciledPlanState::Partial);
        assert_eq!(
            partial.plans[0].steps[2].state,
            PlanStatusStepState::Pending
        );
        assert!(partial.applied.is_empty());
        assert_eq!(partial.pending, vec![plan.version.clone()]);

        let complete_journal: Vec<_> = plan
            .steps
            .iter()
            .map(|expected| journal(expected, Phase::Completed))
            .collect();
        let complete = reconcile_applied_plans(std::slice::from_ref(&plan), &complete_journal, &[])
            .expect("complete status");
        assert_eq!(complete.plans[0].state, ReconciledPlanState::Applied);
        assert_eq!(complete.current_version, Some(plan.version.clone()));
        assert_eq!(complete.applied, vec![plan.version]);
        assert!(complete.aborted.is_empty());
        assert!(complete.pending.is_empty());
    }

    #[test]
    fn resolved_apply_overlays_contract_steps_and_filters_atomic_resolver_journal() {
        let anchor = checksum("atomic apply");
        let plan = manifest(
            "applied rename",
            vec![
                step("apply_expand", PlanStatusStepKind::OnlineExpand, &anchor),
                step(
                    "apply_contract_trigger",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
                step(
                    "apply_contract_column",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
            ],
            Vec::new(),
        );
        let resolved = resolved_apply(&plan);
        let entries = vec![
            journal(&plan.steps[0], Phase::Completed),
            atomic_resolver_journal(&resolved, journal::Resolution::Applied),
        ];

        let status = reconcile_applied_plans_with_resolutions(
            std::slice::from_ref(&plan),
            &entries,
            &[],
            &[],
            std::slice::from_ref(&resolved),
            &[],
        )
        .expect("atomic applied status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Applied);
        assert!(status.plans[0]
            .steps
            .iter()
            .all(|step| step.state == PlanStatusStepState::Applied));
        assert_eq!(status.applied, vec![plan.version]);
        assert!(status.pending.is_empty());
        assert!(status.aborted.is_empty());
        assert!(status.unexpected_journal.is_empty());
    }

    #[test]
    fn resolved_apply_does_not_hide_contract_checksum_drift() {
        let anchor = checksum("atomic apply drift");
        let plan = manifest(
            "applied rename drift",
            vec![
                step(
                    "apply_drift_expand",
                    PlanStatusStepKind::OnlineExpand,
                    &anchor,
                ),
                step(
                    "apply_drift_contract",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
            ],
            Vec::new(),
        );
        let resolved = resolved_apply(&plan);
        let entries = vec![
            journal(&plan.steps[0], Phase::Completed),
            AppliedEntry {
                version: plan.steps[1].version.as_str().to_string(),
                checksum: checksum("edited atomic contract").as_str().to_string(),
                phase: Phase::Completed,
                kind: None,
            },
        ];

        let status = reconcile_applied_plans_with_resolutions(
            std::slice::from_ref(&plan),
            &entries,
            &[],
            &[],
            std::slice::from_ref(&resolved),
            &[],
        )
        .expect("atomic apply drift status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Drifted);
        assert_eq!(status.plans[0].steps[1].state, PlanStatusStepState::Drifted);
        assert_eq!(status.pending, vec![plan.version]);
        assert!(status.applied.is_empty());
        assert!(status.aborted.is_empty());
    }

    #[test]
    fn resolved_abort_is_terminal_and_filters_its_resolver_journal_steps() {
        let anchor = checksum("online rename");
        let plan = manifest(
            "aborted rename",
            vec![
                step("expand_add", PlanStatusStepKind::OnlineExpand, &anchor),
                step("expand_trigger", PlanStatusStepKind::OnlineExpand, &anchor),
                step(
                    "contract_trigger",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
                step(
                    "contract_column",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
            ],
            Vec::new(),
        );
        let resolved = resolved_abort(&plan);
        let mut entries = vec![
            journal(&plan.steps[0], Phase::Completed),
            journal(&plan.steps[1], Phase::Completed),
        ];
        entries.extend(resolver_abort_journal(&resolved));
        entries.push(atomic_resolver_journal(
            &resolved,
            journal::Resolution::Aborted,
        ));

        let status = reconcile_applied_plans_with_resolutions(
            std::slice::from_ref(&plan),
            &entries,
            &[],
            &[],
            std::slice::from_ref(&resolved),
            &[],
        )
        .expect("aborted status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Aborted);
        assert_eq!(
            status.plans[0]
                .steps
                .iter()
                .map(|step| step.state)
                .collect::<Vec<_>>(),
            vec![
                PlanStatusStepState::Applied,
                PlanStatusStepState::Applied,
                PlanStatusStepState::Aborted,
                PlanStatusStepState::Aborted,
            ]
        );
        assert!(status.applied.is_empty());
        assert!(status.pending.is_empty());
        assert_eq!(status.aborted, vec![plan.version]);
        assert_eq!(status.current_version, None);
        assert!(status.unexpected_journal.is_empty());
    }

    #[test]
    fn aborted_plan_does_not_satisfy_a_dependent_plan() {
        let anchor = checksum("aborted dependency");
        let base = manifest(
            "aborted base",
            vec![
                step("base_expand", PlanStatusStepKind::OnlineExpand, &anchor),
                step("base_contract", PlanStatusStepKind::OnlineContract, &anchor),
            ],
            Vec::new(),
        );
        let dependent = manifest(
            "dependent on abort",
            vec![step("dependent_step", PlanStatusStepKind::Ddl, &anchor)],
            vec![base.version.clone()],
        );
        let resolved = resolved_abort(&base);
        let entries = vec![journal(&base.steps[0], Phase::Completed)];

        let status = reconcile_applied_plans_with_resolutions(
            &[dependent.clone(), base.clone()],
            &entries,
            &[],
            &[],
            std::slice::from_ref(&resolved),
            &[],
        )
        .expect("aborted dependency status");

        assert_eq!(status.plans[0].version, base.version);
        assert_eq!(status.plans[0].state, ReconciledPlanState::Aborted);
        assert_eq!(status.plans[1].version, dependent.version);
        assert_eq!(status.plans[1].state, ReconciledPlanState::Blocked);
        assert_eq!(status.pending, vec![dependent.version]);
    }

    #[test]
    fn aborted_plan_remains_terminal_when_a_dependency_is_no_longer_supplied() {
        let anchor = checksum("aborted missing dependency");
        let plan = manifest(
            "aborted missing dependency",
            vec![
                step("missing_expand", PlanStatusStepKind::OnlineExpand, &anchor),
                step(
                    "missing_contract",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
            ],
            vec![id("removed_dependency")],
        );
        let resolved = resolved_abort(&plan);
        let entries = vec![journal(&plan.steps[0], Phase::Completed)];

        let status = reconcile_applied_plans_with_resolutions(
            std::slice::from_ref(&plan),
            &entries,
            &[],
            &[],
            std::slice::from_ref(&resolved),
            &[],
        )
        .expect("terminal aborted status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Aborted);
        assert_eq!(status.aborted, vec![plan.version]);
        assert!(status.pending.is_empty());
    }

    #[test]
    fn resolved_abort_does_not_hide_contract_checksum_drift() {
        let anchor = checksum("abort drift");
        let plan = manifest(
            "aborted drift",
            vec![
                step("drift_expand", PlanStatusStepKind::OnlineExpand, &anchor),
                step(
                    "drift_contract",
                    PlanStatusStepKind::OnlineContract,
                    &anchor,
                ),
            ],
            Vec::new(),
        );
        let resolved = resolved_abort(&plan);
        let entries = vec![
            journal(&plan.steps[0], Phase::Completed),
            AppliedEntry {
                version: plan.steps[1].version.as_str().to_string(),
                checksum: checksum("edited contract").as_str().to_string(),
                phase: Phase::Completed,
                kind: None,
            },
        ];

        let status = reconcile_applied_plans_with_resolutions(
            std::slice::from_ref(&plan),
            &entries,
            &[],
            &[],
            std::slice::from_ref(&resolved),
            &[],
        )
        .expect("aborted drift status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Drifted);
        assert_eq!(status.plans[0].steps[1].state, PlanStatusStepState::Drifted);
        assert_eq!(status.pending, vec![plan.version]);
    }

    #[test]
    fn stable_step_checksum_mismatch_is_drift_not_pending() {
        let anchor = checksum("expected");
        let plan = manifest(
            "drift",
            vec![step("data", PlanStatusStepKind::Dml, &anchor)],
            Vec::new(),
        );
        let entries = vec![AppliedEntry {
            version: plan.steps[0].version.as_str().to_string(),
            checksum: checksum("edited").as_str().to_string(),
            phase: Phase::Completed,
            kind: None,
        }];
        let status = reconcile_applied_plans(std::slice::from_ref(&plan), &entries, &[])
            .expect("drift status");
        assert_eq!(status.plans[0].state, ReconciledPlanState::Drifted);
        assert_eq!(status.plans[0].steps[0].state, PlanStatusStepState::Drifted);
        assert_eq!(status.pending, vec![plan.version]);
    }

    #[test]
    fn changed_genuine_repeatable_is_pending_instead_of_drifted() {
        let expected = checksum("new repeatable definition");
        let recorded = checksum("old repeatable definition");
        let plan = repeatable_manifest("repeatable_view", expected);
        let entries = vec![AppliedEntry {
            version: plan.steps[0].version.as_str().to_string(),
            checksum: recorded.as_str().to_string(),
            phase: Phase::Completed,
            kind: Some(crate::apply::journal::JournaledKind::Repeatable),
        }];

        let status = reconcile_applied_plans(std::slice::from_ref(&plan), &entries, &[])
            .expect("repeatable status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Pending);
        assert_eq!(status.plans[0].steps[0].state, PlanStatusStepState::Pending);
        assert_eq!(status.pending, vec![plan.version]);
    }

    #[test]
    fn changing_between_once_only_and_repeatable_is_drift() {
        let anchor = checksum("same definition");
        let repeatable = repeatable_manifest("kind_flip_repeatable", anchor.clone());
        let once_only = manifest(
            "kind_flip_once",
            vec![step(
                "kind_flip_once_step",
                PlanStatusStepKind::Ddl,
                &anchor,
            )],
            Vec::new(),
        );

        let repeatable_as_once = vec![AppliedEntry {
            version: repeatable.steps[0].version.as_str().to_string(),
            checksum: anchor.as_str().to_string(),
            phase: Phase::Completed,
            kind: Some(crate::apply::journal::JournaledKind::Apply),
        }];
        let once_as_repeatable = vec![AppliedEntry {
            version: once_only.steps[0].version.as_str().to_string(),
            checksum: anchor.as_str().to_string(),
            phase: Phase::Completed,
            kind: Some(crate::apply::journal::JournaledKind::Repeatable),
        }];

        let repeatable_status =
            reconcile_applied_plans(std::slice::from_ref(&repeatable), &repeatable_as_once, &[])
                .expect("repeatable-to-once status");
        let once_status =
            reconcile_applied_plans(std::slice::from_ref(&once_only), &once_as_repeatable, &[])
                .expect("once-to-repeatable status");

        assert_eq!(
            repeatable_status.plans[0].state,
            ReconciledPlanState::Drifted
        );
        assert_eq!(once_status.plans[0].state, ReconciledPlanState::Drifted);
    }

    #[test]
    fn empty_manifest_without_a_journal_anchor_is_pending() {
        let plan = manifest("empty", Vec::new(), Vec::new());
        let status = reconcile_applied_plans(std::slice::from_ref(&plan), &[], &[])
            .expect("empty manifest status");
        assert_eq!(status.plans[0].state, ReconciledPlanState::Pending);
        assert_eq!(status.pending, vec![plan.version]);
    }

    #[test]
    fn incomplete_backfill_progress_is_inflight_and_makes_the_plan_partial() {
        let anchor = checksum("backfill");
        let plan = manifest(
            "backfill",
            vec![step("backfill_step", PlanStatusStepKind::Backfill, &anchor)],
            Vec::new(),
        );
        let progress = vec![progress(&plan.steps[0], Some(&anchor), false)];

        let status =
            reconcile_applied_plans_with_progress(std::slice::from_ref(&plan), &[], &progress, &[])
                .expect("progress status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Partial);
        assert_eq!(
            status.plans[0].steps[0].state,
            PlanStatusStepState::Inflight
        );
        assert_eq!(status.pending, vec![plan.version]);
    }

    #[test]
    fn backfill_progress_checksum_mismatch_is_drift_without_a_journal_event() {
        let anchor = checksum("expected backfill");
        let edited = checksum("edited backfill");
        let plan = manifest(
            "backfill drift",
            vec![step("backfill_step", PlanStatusStepKind::Backfill, &anchor)],
            Vec::new(),
        );
        let progress = vec![progress(&plan.steps[0], Some(&edited), false)];

        let status =
            reconcile_applied_plans_with_progress(std::slice::from_ref(&plan), &[], &progress, &[])
                .expect("progress drift status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Drifted);
        assert_eq!(status.plans[0].steps[0].state, PlanStatusStepState::Drifted);
        assert_eq!(
            status.plans[0].steps[0].journal_checksum.as_deref(),
            Some(edited.as_str())
        );
    }

    #[test]
    fn backfill_progress_without_a_checksum_is_drift() {
        let anchor = checksum("anchored backfill");
        let plan = manifest(
            "legacy progress",
            vec![step("backfill_step", PlanStatusStepKind::Backfill, &anchor)],
            Vec::new(),
        );
        let progress = vec![progress(&plan.steps[0], None, false)];

        let status =
            reconcile_applied_plans_with_progress(std::slice::from_ref(&plan), &[], &progress, &[])
                .expect("legacy progress status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Drifted);
        assert_eq!(status.plans[0].steps[0].state, PlanStatusStepState::Drifted);
        assert_eq!(status.plans[0].steps[0].journal_checksum, None);
    }

    #[test]
    fn matching_completed_progress_without_a_journal_event_is_still_inflight() {
        let anchor = checksum("repairable backfill");
        let plan = manifest(
            "repairable backfill",
            vec![step("backfill_step", PlanStatusStepKind::Backfill, &anchor)],
            Vec::new(),
        );
        let progress = vec![progress(&plan.steps[0], Some(&anchor), true)];

        let status =
            reconcile_applied_plans_with_progress(std::slice::from_ref(&plan), &[], &progress, &[])
                .expect("repairable progress status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Partial);
        assert_eq!(
            status.plans[0].steps[0].state,
            PlanStatusStepState::Inflight
        );
    }

    #[test]
    fn completed_journal_event_wins_over_mutable_progress() {
        let anchor = checksum("completed backfill");
        let edited = checksum("stale progress");
        let plan = manifest(
            "completed backfill",
            vec![step("backfill_step", PlanStatusStepKind::Backfill, &anchor)],
            Vec::new(),
        );
        let entries = vec![journal(&plan.steps[0], Phase::Completed)];
        let progress = vec![progress(&plan.steps[0], Some(&edited), false)];

        let status = reconcile_applied_plans_with_progress(
            std::slice::from_ref(&plan),
            &entries,
            &progress,
            &[],
        )
        .expect("completed status");

        assert_eq!(status.plans[0].state, ReconciledPlanState::Applied);
        assert_eq!(status.plans[0].steps[0].state, PlanStatusStepState::Applied);
    }

    #[test]
    fn unexpected_completed_and_inflight_versions_and_rollbacks_are_retained() {
        let completed = AppliedEntry {
            version: id("unexpected_completed").as_str().to_string(),
            checksum: checksum("unexpected completed").as_str().to_string(),
            phase: Phase::Completed,
            kind: Some(crate::apply::journal::JournaledKind::Apply),
        };
        let inflight = AppliedEntry {
            version: id("unexpected_inflight").as_str().to_string(),
            checksum: checksum("unexpected inflight").as_str().to_string(),
            phase: Phase::Started,
            kind: None,
        };
        let rolled_back = id("rolled_back").as_str().to_string();

        let status = reconcile_applied_plans_with_snapshot(
            &[],
            &[completed.clone(), inflight.clone()],
            &[],
            &[],
            std::slice::from_ref(&rolled_back),
        )
        .expect("unexpected journal status");

        assert_eq!(status.rolled_back, vec![rolled_back]);
        assert_eq!(status.unexpected_journal.len(), 2);
        let completed_status = status
            .unexpected_journal
            .iter()
            .find(|entry| entry.version == completed.version)
            .expect("completed entry");
        assert_eq!(completed_status.state, PlanStatusStepState::Applied);
        let inflight_status = status
            .unexpected_journal
            .iter()
            .find(|entry| entry.version == inflight.version)
            .expect("inflight entry");
        assert_eq!(inflight_status.state, PlanStatusStepState::Inflight);
    }

    #[test]
    fn dependencies_reorder_supplied_plans_and_block_until_dependency_completes() {
        let anchor = checksum("dependency");
        let base = manifest(
            "base",
            vec![step("base_step", PlanStatusStepKind::Ddl, &anchor)],
            Vec::new(),
        );
        let dependent = manifest(
            "dependent",
            vec![step("dependent_step", PlanStatusStepKind::Dml, &anchor)],
            vec![base.version.clone()],
        );
        // Supply the dependent first: the status order still follows its DAG.
        let status = reconcile_applied_plans(&[dependent.clone(), base.clone()], &[], &[])
            .expect("blocked status");
        assert_eq!(status.plans[0].version, base.version);
        assert_eq!(status.plans[0].state, ReconciledPlanState::Pending);
        assert_eq!(status.plans[1].version, dependent.version);
        assert_eq!(status.plans[1].state, ReconciledPlanState::Blocked);
    }

    #[test]
    fn omitted_dependency_is_unknown_instead_of_assumed_from_step_rows() {
        let anchor = checksum("unknown dependency");
        let omitted = id("omitted_plan");
        let dependent = manifest(
            "dependent",
            vec![step("dependent_data", PlanStatusStepKind::Dml, &anchor)],
            vec![omitted.clone()],
        );
        let status = reconcile_applied_plans(std::slice::from_ref(&dependent), &[], &[])
            .expect("unknown status");
        assert_eq!(
            status.plans[0].state,
            ReconciledPlanState::UnknownDependency
        );
        assert_eq!(status.plans[0].missing_dependencies, vec![omitted]);
    }

    #[test]
    fn manifest_projection_retains_backfill_cursor_stability() {
        let anchor = checksum("artifact");
        let dml_version = id("artifact_dml");
        let backfill_version = id("artifact_backfill");
        let plan = AppliedPlan {
            version: id("artifact_plan"),
            name: "artifact".to_string(),
            steps: vec![
                PlanStep::Dml {
                    version: dml_version.clone(),
                    checksum: anchor.clone(),
                    name: "update widgets".to_string(),
                    template: "UPDATE widgets SET ready = $1".to_string(),
                    binds: vec![crate::render::step::BindValue::Bool(true)],
                    target_schema: "app".to_string(),
                    target_table: "widgets".to_string(),
                    conflict_target: None,
                    mutates_data: true,
                    transactional: true,
                    destructive: false,
                    requires_approval: false,
                    owner_app: "app_status".to_string(),
                },
                PlanStep::Backfill {
                    version: backfill_version.clone(),
                    checksum: anchor.clone(),
                    spec: BackfillSpec {
                        schema: "app".to_string(),
                        table: "widgets".to_string(),
                        cursor_columns: vec!["id".to_string()],
                        cursor_stability: crate::model::ir::CursorStability::ExternalInvariant {
                            name: "writers_hold_cursor_key".to_string(),
                        },
                        cursor_contract: None,
                        batch_size: 100,
                        set_clause: "ready = true".to_string(),
                        per_row: Default::default(),
                        filter: None,
                        name: "backfill widgets".to_string(),
                    },
                },
            ],
            database_requirements: Default::default(),
            checksum: anchor,
            flags: MigrationFlags::default(),
            dialect_scope: DialectScope::Both,
            rollbackable: false,
            owner_app: "app_status".to_string(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
        };

        let projected =
            PlanStatusManifest::from_applied_plan(&plan, &[]).expect("manifest projection");
        assert_eq!(projected.steps.len(), 2);
        assert_eq!(projected.steps[0].version, dml_version);
        assert_eq!(projected.steps[0].kind, PlanStatusStepKind::Dml);
        assert_eq!(projected.steps[1].version, backfill_version);
        assert_eq!(projected.steps[1].kind, PlanStatusStepKind::Backfill);
        assert_eq!(
            projected.steps[1].cursor_stability_mode.as_deref(),
            Some("externalInvariant")
        );
        assert_eq!(
            projected.steps[1].cursor_stability_invariant.as_deref(),
            Some("writers_hold_cursor_key")
        );

        let status = reconcile_applied_plans(&[projected], &[], &[]).expect("status");
        assert_eq!(
            status.plans[0].steps[1].cursor_stability_mode.as_deref(),
            Some("externalInvariant")
        );
        assert_eq!(
            status.plans[0].steps[1]
                .cursor_stability_invariant
                .as_deref(),
            Some("writers_hold_cursor_key")
        );
    }

    #[test]
    fn manifest_and_status_retain_synchronize_identity_quiescence_assertion() {
        let ir: crate::model::ir::MigrationIr = serde_json::from_str(
            r#"{"ir_version":1,"name":"sync_imported_orders","ops":[
              {"op":"synchronizeIdentity","schema":"app","table":"orders",
               "column":"id","writesQuiesced":"orders_import_window"}
            ]}"#,
        )
        .expect("synchronizeIdentity IR");
        let plan = IrAuthor::new(
            "app",
            "app_status",
            SqlDialect::Postgres,
            &crate::test_fixtures::no_inject("app"),
        )
        .lower_plan(&ir, &LiveSchema::default())
        .expect("synchronizeIdentity plan");

        let projected =
            PlanStatusManifest::from_applied_plan(&plan, &[]).expect("manifest projection");
        assert_eq!(projected.steps.len(), 1);
        assert_eq!(
            projected.steps[0].kind,
            PlanStatusStepKind::SynchronizeIdentity
        );
        assert_eq!(
            projected.steps[0].writes_quiesced.as_deref(),
            Some("orders_import_window")
        );

        let status = reconcile_applied_plans(&[projected], &[], &[]).expect("status");
        assert_eq!(
            status.plans[0].steps[0].writes_quiesced.as_deref(),
            Some("orders_import_window")
        );
    }
}
