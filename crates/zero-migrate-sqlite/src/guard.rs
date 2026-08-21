//! SQLite's line-1 defense — the descriptor-diff path, trusted by construction.
//!
//! SQLite migrations are produced ONLY by the declarative differ
//! (`DeclarativeAuthor::diff`); there is no raw-SQL SQLite author. `libpg_query`
//! cannot parse SQLite, so there is no string deny-list to run: the line-1 vet is the
//! descriptor emitter at the author boundary, and the line-2 defense is the
//! `SqliteBackend`'s runtime authorizer applied per statement at execution.
//!
//! So [`SqliteGuard::check`] returns the EMPTY outcome. That is a deliberate grant of
//! trust, and this file is where it is granted — written out, in this vendor's own
//! crate, attributable to this vendor.
//!
//! # This is not the whole of SQLite's data security
//!
//! Reading only this file would suggest SQLite has no data-security posture at all.
//! It does. `data_security.destructive_ops = forbid` is enforced for SQLite by
//! `zero_migrate_guard::guard::check_ir_data_security_policy`, over the structured IR
//! rather than over SQL text, and its gate is written
//! `if cfg.dialect() != &POSTGRES` — i.e. it exists precisely
//! BECAUSE the guard here is empty and is handed no policy. Enforcement over the IR is
//! the stronger place for a descriptor-only dialect anyway: there is no text to
//! misparse, only ops.
//!
//! # Why this is not shared with MySQL
//!
//! It used to be. Both dialects ran one `SqliteDescriptorGuard`, whose own doc
//! admitted it "serves BOTH descriptor-only engines — SQLite and MySQL — despite the
//! name". Sharing a type named after one vendor made a MySQL reviewer read the
//! dispatch as a bug at a glance. Each vendor now writes its own, which costs a dozen
//! lines and means a change to SQLite's posture cannot silently become a change to
//! MySQL's.

use zero_migrate_backend::guard::{GuardConfig, GuardError, GuardOutcome, MigrationGuard};

/// SQLite's line-1: trust the descriptor-diff output.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteGuard;

impl SqliteGuard {
    /// Construct the descriptor guard (stateless).
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl MigrationGuard for SqliteGuard {
    fn check(&self, _up: &str) -> Result<GuardOutcome, GuardError> {
        // Descriptor-diff-generated DDL is trusted (author boundary line-1 + backend
        // authorizer line-2). No string check, no denial - the empty clean outcome.
        // Destructive/approval flags come from the migration's OWN author flags,
        // combined by the engine's `plan()`, not from here.
        Ok(GuardOutcome::default())
    }
}

/// This vendor's `BackendVendor::guard` factory.
///
/// The config is ignored: this guard holds no state and reads no knob. That is
/// exactly what makes `check_ir_data_security_policy`'s non-PostgreSQL arm load-bearing
/// rather than redundant.
#[must_use]
pub fn guard(_cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    Box::new(SqliteGuard::new())
}
