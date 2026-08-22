//! Backend-owned catalog semantics used by the neutral existence-probe decider.
//!
//! The decider composes a live [`SchemaSnapshot`](crate::snapshot::SchemaSnapshot)
//! with an authored guard, but four parts of that comparison are not portable:
//! whether a unique index also carries constraint identity, whether an unresolved
//! constraint drop must fail closed, how catalog constraint definitions normalize,
//! and whether a catalog silently truncates an authored identifier. Those answers
//! live here as a required backend contract instead of as vendor-name branches in
//! core.

use core::fmt;

/// Vendor policy for existence-guard catalog decisions.
///
/// Every method is required. In particular, there is no shared "ordinary SQL"
/// implementation for a fourth backend to inherit by omission: each backend must
/// state its own catalog identity and truncation behavior.
pub trait ExistenceProbePolicy: fmt::Debug + Sync {
    /// Whether a same-name unique index is also the named unique constraint.
    fn unique_index_carries_constraint_identity(&self) -> bool;

    /// Why a missing constraint row does not prove an `ifExists` target absent.
    ///
    /// `None` means this backend's snapshot covers the full constraint identity
    /// scope used by the probe, so a miss is proof of absence. `Some` supplies the
    /// backend-owned fail-closed diagnostic.
    fn unresolved_constraint_drop_reason(&self) -> Option<&'static str>;

    /// Normalize a declared/live constraint definition for this catalog's
    /// structural comparison.
    fn normalize_constraint_definition(&self, definition: &str) -> String;

    /// The catalog spelling created for `authored`, when the backend silently
    /// truncates it. `None` means the backend does not silently truncate this name.
    fn truncated_identifier(&self, authored: &str) -> Option<String>;
}
