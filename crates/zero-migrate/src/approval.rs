//! The caller's approval decision — shared by the engine gate AND the executor's
//! own defense-in-depth gate.
//!
//! [`Approval`] lived in [`crate::engine`] originally, where it gated only the
//! public [`MigrationEngine`](crate::engine::MigrationEngine) surface. But the
//! executor's [`crate::apply::executor::apply`] is itself a public entry point a
//! caller can drive directly, bypassing the engine gate. The executor already
//! re-runs the guard +
//! the least-privilege role rather than trusting the engine; the approval gate is
//! the same pattern, so it must live at the executor layer too. Hoisting
//! [`Approval`] into its own module lets both layers share the one type without an
//! engine→executor dependency inversion.

/// The caller's approval decision for a destructive migration batch.
///
/// A destructive plan (a `DROP`/`TRUNCATE`/lossy-type-change `up`, or any
/// rollback — a `down` is inherently destructive) needs [`Approval::Approved`] to
/// run; a safe additive `up` runs with [`Approval::None`]. The AI never
/// auto-applies destructive ops — it passes [`Approval::None`] and
/// surfaces the approval-required error to a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// No approval given — runs only a non-destructive batch.
    None,
    /// Explicitly approved — a destructive batch may run.
    Approved,
}

/// **Per-version approval scoping (anti-bypass).** An orthogonal,
/// fail-closed restriction layered on top of [`Approval::Approved`]: it answers
/// "WHICH destructive ops did the operator individually review?" so approving one
/// reviewed online rename can NEVER blanket-authorize an *unrelated* co-bundled
/// destructive op (a `dropColumn`/`dropTable`) the operator never saw.
///
/// The coarse [`Approval`] enum stays a plain "is destruction permitted at all"
/// gate threaded through every call site; this is a separate, narrow refinement
/// carried only on the gated apply paths. The two compose: a destructive op runs
/// iff `Approval::Approved` AND the scope admits its version-id. A NON-destructive
/// op is never affected — scope only ever *further restricts* destruction, never
/// widens it.
///
/// **Fail-closed by construction.** [`ApprovalScope::Versions`] authorizes ONLY
/// the listed version-ids; an empty set authorizes NOTHING destructive even under
/// `Approval::Approved`. There is no "unrecognized scope ⇒ allow" arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalScope {
    /// Blanket: any destructive op may run (today's behavior for the trusted
    /// single-actor surfaces — dev CLI `--yes`, rollback, shadow dry-run,
    /// resolve-pending). Fail-OPEN by *explicit operator intent* at a trusted
    /// vector; it is the default for every existing caller, preserving
    /// byte-identical behavior.
    All,
    /// Fail-CLOSED: a destructive op runs only if its plan/step version-id is in
    /// this set. Any destructive op whose version is NOT in the set is refused
    /// even under [`Approval::Approved`]. The new out-of-band approved-apply
    /// surface constructs this from the operator's reviewed version-id set.
    Versions(std::collections::BTreeSet<String>),
}

impl ApprovalScope {
    /// Does this scope admit a DESTRUCTIVE op stamped with `version`?
    ///
    /// [`ApprovalScope::All`] admits every version (vacuously true — today's
    /// blanket behavior). [`ApprovalScope::Versions`] admits ONLY a version
    /// present in the reviewed set (fail-closed: an empty set admits nothing).
    ///
    /// This is consulted ONLY for destructive ops; a non-destructive op never
    /// reaches it (scope cannot block additive work).
    #[must_use]
    pub fn admits(&self, version: &str) -> bool {
        match self {
            Self::All => true,
            Self::Versions(set) => set.contains(version),
        }
    }
}
