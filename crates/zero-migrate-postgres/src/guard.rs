//! PostgreSQL's line-1 defense — the `libpg_query` deny-list, behind the contract.
//!
//! This is the vendor half of `zero_migrate_backend::guard::MigrationGuard`. The
//! parse-time deny-list, the cross-schema confinement, the classifier and the
//! operational analyzers all live in `zero-migrate-guard`, which owns the
//! `libpg_query` C dependency. What lives HERE is the seven-line adapter that files
//! that machinery under this vendor's `BackendVendor`, so the engine reaches it the
//! same way it reaches this vendor's two renderers: through the registry, never by
//! naming a dialect.

use zero_migrate_backend::guard::{GuardConfig, GuardError, GuardOutcome, MigrationGuard};
use zero_migrate_guard::guard::SqlGuard;

/// The PostgreSQL line-1: the `libpg_query` deny-list + cross-schema confinement +
/// classify + analyze, mapped onto the neutral `GuardOutcome`.
///
/// Behavior-identical to calling `SqlGuard::check` — `check` only drops the
/// PostgreSQL-specific `classes` from the returned report, because `DdlKind` is a
/// `libpg_query` vocabulary no other engine could populate. The consumers that DO
/// want `classes` (`flags_for`, the author/submit/loader flag derivation, the
/// `guard_security` matrix) keep calling `SqlGuard::check` directly.
#[derive(Debug, Clone)]
pub struct PgGuard(SqlGuard);

impl PgGuard {
    /// Wrap a `SqlGuard` as the PostgreSQL [`MigrationGuard`].
    #[must_use]
    pub const fn new(inner: SqlGuard) -> Self {
        Self(inner)
    }

    /// Build the PostgreSQL guard from a [`GuardConfig`] (the common case).
    #[must_use]
    pub const fn from_config(cfg: GuardConfig) -> Self {
        Self(SqlGuard::new(cfg))
    }
}

impl MigrationGuard for PgGuard {
    fn check(&self, up: &str) -> Result<GuardOutcome, GuardError> {
        let report = self.0.check(up)?;
        // Drop the PostgreSQL-specific `classes`; expose only the neutral fields the
        // engine seam consumes.
        Ok(GuardOutcome {
            destructive: report.destructive,
            advisories: report.advisories,
        })
    }
}

/// This vendor's `BackendVendor::guard` factory.
///
/// The guard is built per call because it holds the per-migration [`GuardConfig`] it
/// decides against, which is why the registry field is a `fn` pointer rather than a
/// `&'static dyn`.
#[must_use]
pub fn guard(cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    Box::new(PgGuard::from_config(cfg.clone()))
}
