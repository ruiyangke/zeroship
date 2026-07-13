//! Structured machine-readable payloads for the cross-deploy
//! pending-contract interlock.
//!
//! The design goal is fully-automatable migrations, so every
//! stuck state in the online-rename lifecycle emits a STRUCTURED envelope an AI
//! orchestrator can act on — not only a prose string. The human-readable
//! message is the **projection** of the structured payload (the `Display` impls
//! here), and each payload carries enough to EXECUTE the remedy (an
//! `apply_action` / `abort_action` naming the exact `migrate …` command).
//!
//! The three payloads:
//! - [`PendingContractRefusal`] (`TABLE_HAS_PENDING_CONTRACT`) — a new op touches
//!   a table with an outstanding pending contract; the deploy is fail-closed
//!   refused.
//! - [`DependencyPendingContract`] (`DEPENDENCY_PENDING_CONTRACT`) — a plan B
//!   `depends_on`s a plan A whose online-rename contract is still pending, so A
//!   is not fully satisfied and B is BLOCKED.
//! - [`OrphanedPendingContract`] (`ORPHANED_PENDING_CONTRACT`) — a later bundle no
//!   longer carries the rename whose contract is pending; the obligation is
//!   orphaned, surfaced by `status` as a distinct state.
//!
//! The wire `code` strings are the contract; they are pinned by a unit test.

use serde::{Deserialize, Serialize};

/// The `code` literal for a [`PendingContractRefusal`].
pub const CODE_TABLE_HAS_PENDING_CONTRACT: &str = "TABLE_HAS_PENDING_CONTRACT";
/// The `code` literal for a [`DependencyPendingContract`].
pub const CODE_DEPENDENCY_PENDING_CONTRACT: &str = "DEPENDENCY_PENDING_CONTRACT";
/// The `code` literal for an [`OrphanedPendingContract`].
pub const CODE_ORPHANED_PENDING_CONTRACT: &str = "ORPHANED_PENDING_CONTRACT";

/// An executable remediation action — a `migrate …` command + the version it
/// targets — so an automated orchestrator can self-resolve where policy allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionPayload {
    /// The `migrate …` command to run (e.g. `migrate resolve-pending --apply`).
    pub command: String,
    /// The version the command targets (the pending/orphan `pending_version`).
    pub version: String,
}

/// `TABLE_HAS_PENDING_CONTRACT` — the fail-closed refusal payload.
///
/// Emitted when the current deploy's op list touches a table that still has an
/// outstanding online-rename contract from a prior deploy. The deploy applies
/// NOTHING; the operator must apply the pending contract first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingContractRefusal {
    /// Always [`CODE_TABLE_HAS_PENDING_CONTRACT`].
    pub code: String,
    /// The table with the outstanding pending contract (the refusal key).
    pub table: String,
    /// The obligation key — the E2 trigger version of the pending rename.
    pub pending_version: String,
    /// The remediation tag (`"apply_pending"`).
    pub remediation: String,
    /// The executable action: apply the pending contract.
    pub apply_action: ActionPayload,
}

impl PendingContractRefusal {
    /// Build the refusal payload for `table` whose contract `pending_version` is
    /// outstanding.
    #[must_use]
    pub fn new(table: impl Into<String>, pending_version: impl Into<String>) -> Self {
        let pending_version = pending_version.into();
        Self {
            code: CODE_TABLE_HAS_PENDING_CONTRACT.to_string(),
            table: table.into(),
            pending_version: pending_version.clone(),
            remediation: "apply_pending".to_string(),
            apply_action: ActionPayload {
                command: "migrate resolve-pending --apply".to_string(),
                version: pending_version,
            },
        }
    }
}

impl std::fmt::Display for PendingContractRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "table `{}` has an in-flight online rename (contract pending from a prior \
             deploy, version `{}`); apply that contract before authoring further changes \
             to `{}` — run `{} {}`",
            self.table,
            self.pending_version,
            self.table,
            self.apply_action.command,
            self.apply_action.version
        )
    }
}

/// `DEPENDENCY_PENDING_CONTRACT` — the blocked-`depends_on` payload.
///
/// Emitted when plan `blocked` declares `depends_on: [dependency]` and the
/// dependency is an online rename whose contract is still pending — so the
/// dependency is NOT fully satisfied and the blocked plan cannot apply yet. This
/// is a DISTINCT, retained `blocked-awaiting-approval` state, NOT a failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyPendingContract {
    /// Always [`CODE_DEPENDENCY_PENDING_CONTRACT`].
    pub code: String,
    /// The blocked plan's version (B).
    pub blocked: String,
    /// The dependency plan's version (A).
    pub dependency: String,
    /// The dependency's outstanding pending-contract version.
    pub pending_version: String,
    /// The remediation tag (`"apply_dependency_contract"`).
    pub remediation: String,
}

impl DependencyPendingContract {
    /// Build the blocked-dependency payload.
    #[must_use]
    pub fn new(
        blocked: impl Into<String>,
        dependency: impl Into<String>,
        pending_version: impl Into<String>,
    ) -> Self {
        Self {
            code: CODE_DEPENDENCY_PENDING_CONTRACT.to_string(),
            blocked: blocked.into(),
            dependency: dependency.into(),
            pending_version: pending_version.into(),
            remediation: "apply_dependency_contract".to_string(),
        }
    }
}

impl std::fmt::Display for DependencyPendingContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "plan `{}` depends on plan `{}`, which has a pending online-rename contract \
             (version `{}`) and is therefore not fully satisfied; `{}` is \
             blocked-awaiting-approval until `{}`'s contract is applied",
            self.blocked, self.dependency, self.pending_version, self.blocked, self.dependency
        )
    }
}

/// `ORPHANED_PENDING_CONTRACT` — the orphaned-obligation payload.
///
/// Emitted when a later bundle no longer carries the rename whose contract is
/// pending: the obligation is orphaned. The engine neither silently drops it
/// (which would leave a live dual-write trigger + shadow column forever) nor
/// silently applies it (intent is ambiguous). It is surfaced by `status` as a
/// distinct state with two remedies: re-add the rename op, or abort it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanedPendingContract {
    /// Always [`CODE_ORPHANED_PENDING_CONTRACT`].
    pub code: String,
    /// The table whose rename contract is orphaned.
    pub table: String,
    /// The orphaned obligation's version (the E2 trigger version).
    pub orphan_version: String,
    /// The remediation tags (`["readd_rename_op", "resolve_pending_abort"]`).
    pub remediation: Vec<String>,
    /// The executable action: abort the orphaned contract (drop shadow col +
    /// trigger).
    pub abort_action: ActionPayload,
}

impl OrphanedPendingContract {
    /// Build the orphaned-obligation payload.
    #[must_use]
    pub fn new(table: impl Into<String>, orphan_version: impl Into<String>) -> Self {
        let orphan_version = orphan_version.into();
        Self {
            code: CODE_ORPHANED_PENDING_CONTRACT.to_string(),
            table: table.into(),
            orphan_version: orphan_version.clone(),
            remediation: vec![
                "readd_rename_op".to_string(),
                "resolve_pending_abort".to_string(),
            ],
            abort_action: ActionPayload {
                command: "migrate resolve-pending --abort".to_string(),
                version: orphan_version,
            },
        }
    }
}

impl std::fmt::Display for OrphanedPendingContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "table `{}` has an ORPHANED online-rename contract (version `{}`): a later \
             deploy no longer carries the rename. Re-add the rename op, or abort it — run \
             `{} {}`",
            self.table, self.orphan_version, self.abort_action.command, self.abort_action.version
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire `code` strings are the contract the AI loop matches on. Pin
    /// them byte-exact.
    #[test]
    fn payload_codes_are_exact() {
        assert_eq!(CODE_TABLE_HAS_PENDING_CONTRACT, "TABLE_HAS_PENDING_CONTRACT");
        assert_eq!(
            CODE_DEPENDENCY_PENDING_CONTRACT,
            "DEPENDENCY_PENDING_CONTRACT"
        );
        assert_eq!(CODE_ORPHANED_PENDING_CONTRACT, "ORPHANED_PENDING_CONTRACT");
    }

    /// The refusal payload carries an EXECUTABLE `apply_action`: the command +
    /// version an orchestrator runs to unblock.
    #[test]
    fn refusal_carries_executable_apply_action() {
        let r = PendingContractRefusal::new("users", "mig_expandV2");
        assert_eq!(r.code, CODE_TABLE_HAS_PENDING_CONTRACT);
        assert_eq!(r.table, "users");
        assert_eq!(r.pending_version, "mig_expandV2");
        assert_eq!(r.remediation, "apply_pending");
        assert_eq!(r.apply_action.command, "migrate resolve-pending --apply");
        assert_eq!(r.apply_action.version, "mig_expandV2");
    }

    /// The orphan payload carries an executable `abort_action` + both remedies.
    #[test]
    fn orphan_carries_abort_action_and_both_remedies() {
        let o = OrphanedPendingContract::new("users", "mig_expandV2");
        assert_eq!(o.code, CODE_ORPHANED_PENDING_CONTRACT);
        assert_eq!(o.abort_action.command, "migrate resolve-pending --abort");
        assert_eq!(o.abort_action.version, "mig_expandV2");
        assert_eq!(o.remediation, vec!["readd_rename_op", "resolve_pending_abort"]);
    }

    /// Each payload round-trips through JSON (the orchestrator parses it off the
    /// wire), and the human `Display` projection names the table/version.
    #[test]
    fn payloads_serialize_and_project_to_human_message() {
        let r = PendingContractRefusal::new("users", "mig_e2");
        let json = serde_json::to_string(&r).unwrap();
        let back: PendingContractRefusal = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        assert!(r.to_string().contains("users"));
        assert!(r.to_string().contains("mig_e2"));

        let d = DependencyPendingContract::new("mig_b", "mig_a", "mig_e2");
        let back: DependencyPendingContract =
            serde_json::from_str(&serde_json::to_string(&d).unwrap()).unwrap();
        assert_eq!(d, back);
        assert!(d.to_string().contains("mig_b"));
        assert!(d.to_string().contains("mig_a"));

        let o = OrphanedPendingContract::new("users", "mig_e2");
        let back: OrphanedPendingContract =
            serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
        assert_eq!(o, back);
        assert!(o.to_string().contains("ORPHANED"));
    }
}
