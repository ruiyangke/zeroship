//! The caller's approval decision — shared by the engine gate AND the executor's
//! own defense-in-depth gate (design §1.6).
//!
//! [`Approval`] lived in [`crate::engine`] originally, where it gated only the
//! public [`MigrationEngine`](crate::engine::MigrationEngine) surface. But the
//! executor ([`crate::executor::apply`] / [`crate::executor::rollback`]) is itself
//! a public entry point a caller (or a rollback→reapply retry loop) can drive
//! directly, bypassing the engine gate. The executor already re-runs the guard +
//! the least-privilege role rather than trusting the engine; the approval gate is
//! the same pattern, so it must live at the executor layer too. Hoisting
//! [`Approval`] into its own module lets both layers share the one type without an
//! engine→executor dependency inversion.

/// The caller's approval decision for a destructive migration batch.
///
/// A destructive plan (a `DROP`/`TRUNCATE`/lossy-type-change `up`, or any
/// rollback — a `down` is inherently destructive) needs [`Approval::Approved`] to
/// run; a safe additive `up` runs with [`Approval::None`]. The AI never
/// auto-applies destructive ops (design §1.6) — it passes [`Approval::None`] and
/// surfaces the approval-required error to a human.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    /// No approval given — runs only a non-destructive batch.
    None,
    /// Explicitly approved — a destructive batch may run.
    Approved,
}
