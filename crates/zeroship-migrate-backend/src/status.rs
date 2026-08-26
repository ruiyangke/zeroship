//! The status VOCABULARY - what a journal read reduces to, and the two derived
//! cross-deploy views over it.
//!
//! The status VERBS are the engine's: `zeroship_migrate::ops::status` composes them over
//! `MigrationBackend`, and they stay there. What lives here is the shape those verbs
//! and every vendor journal reader agree on.
//!
//! It travelled for the same reason `order_pending` did. `zeroship-migrate-postgres`'s
//! `status_sql` reads its own journal under a `REPEATABLE READ READ ONLY` snapshot -
//! a statement no other vendor accepts, so the read cannot be generic - and then
//! answers the SAME question in the SAME vocabulary the neutral verb answers. Two
//! copies of that vocabulary would be two answers that drift; the engine's verb and
//! the vendor's snapshot path fill in one [`MigrationStatus`].
//!
//! Nothing here names a dialect, opens a connection, or emits a statement.

use zeroship_migrate_ir::migration::{Migration, MigrationId};

use crate::executor::ApplyError;
use crate::journal::{AppliedEntry, JournalError, RolledBackEntry};

/// Where a project's schema stands relative to a supplied migration set.
///
/// `applied` and `pending` are computed from **NET journal state** (a rolled-back
/// version is NOT applied and re-enters `pending`); `rolled_back` lists versions
/// whose latest event is a rollback. The three are derived from the same journal
/// read the executor uses, so status never disagrees with what apply would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// The highest net-applied version (the schema's current point), or `None`
    /// when nothing is applied. "Highest" is `UUIDv7`/`MigrationId` order - the
    /// same total order apply advances through.
    pub current_version: Option<MigrationId>,
    /// Net-applied entries (latest event = `completed`), in version order. Reuses
    /// the backend's [`applied`](crate::backend::MigrationBackend::applied)
    /// entries (version, checksum, phase).
    pub applied: Vec<AppliedEntry>,
    /// Versions in the supplied set that are NOT net-applied, in the SAME
    /// topological order apply will run them ([`order_pending`](crate::executor::order_pending)).
    /// A rolled-back version that is still in the set reappears here.
    pub pending: Vec<MigrationId>,
    /// Versions whose latest event is a rollback (net rolled-back), with the
    /// rollback event's detail.
    pub rolled_back: Vec<RolledBackEntry>,
    /// **Cross-deploy online-rename pending contracts.** Each outstanding
    /// obligation (EXPAND applied, contract C1/C2 not yet applied), flagged
    /// `orphaned` when the supplied migration set no longer carries the rename
    /// whose contract is pending. A distinct surfaced state - the operator must
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
/// `status_via_backend`. `orphaned` is computed against the supplied migration
/// set:
/// an obligation whose `pending_version` is NOT among the supplied set's versions
/// is orphaned (the rename was removed after its EXPAND applied) and emits the
/// `zeroship_migrate::plan::pending::OrphanedPendingContract` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingContractStatus {
    /// The table whose online-rename contract is outstanding.
    pub table: String,
    /// The obligation key - the E2 trigger version of the pending rename.
    pub pending_version: String,
    /// `true` => the supplied set no longer carries this rename (orphaned);
    /// `false` => a routine outstanding obligation awaiting its contract.
    pub orphaned: bool,
}

/// One plan blocked on a pending-contract dependency surfaced by
/// `status_via_backend` - a retained `blocked-awaiting-approval` state, not a
/// failure.
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
    /// dependency cycle in the supplied set) - surfaced, not swallowed, so status
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

/// Derive the [`MigrationStatus::pending_contracts`] + [`MigrationStatus::blocked`]
/// fields from the OUTSTANDING obligation set and the supplied migration set.
/// Pure - shared by the PG snapshot path and any
/// backend path that can read the obligation set.
///
/// - **Orphan:** an obligation whose **`plan_version`** is NOT
///   among the supplied set's versions is orphaned (the rename op was removed
///   after its EXPAND applied). Fail-closed: it is surfaced as a distinct state,
///   never silently dropped.
/// - **Blocked:** a supplied migration B whose `depends_on` references an
///   outstanding obligation's **`plan_version`** (the dependency A's plan-group
///   version) is blocked until A's contract applies - a retained
///   `blocked-awaiting-approval` state.
///
/// **Why `plan_version`, not `pending_version`.** The obligation key
/// `pending_version` is the E2 trigger SUB-step id - a deep id that no plan-level
/// migration set ever exposes (a plan is ONE `Migration` per file,
/// keyed on the file/plan version, never on a rename's interior sub-step). Keying
/// orphan on `pending_version` made EVERY outstanding obligation falsely
/// `orphaned`, and keying `blocked` on it made the blocked state NEVER fire (an
/// author declares `depends_on` on plan A's PLAN version, not A's E2 sub-version).
/// `plan_version` is the rename's plan-group version (E1-anchored, deterministic),
/// which a re-lowered IR's `lower_plan().version` reproduces and a
/// `depends_on: [A]` references - so both checks key on the identity the supplied
/// set actually carries.
pub fn derive_pending_contract_status(
    outstanding: &[crate::journal::PendingContract],
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
            // PLAN version - the stable identity the loaded set
            // exposes, NOT the interior E2 sub-version.
            orphaned: !supplied.contains(pc.plan_version.as_str()),
        })
        .collect();

    // Map every outstanding obligation's PLAN version -> its E2 obligation key, so a
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
