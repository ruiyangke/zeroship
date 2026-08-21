//! MySQL's line-1 defense — the descriptor path, trusted by construction.
//!
//! MySQL migrations reach the engine as descriptor-diff output, which the author
//! boundary and the backend's runtime checks already vet. There is no raw-SQL MySQL
//! author and no MySQL parser in this workspace, so there is no string deny-list to
//! run and [`MysqlGuard::check`] returns the EMPTY outcome.
//!
//! Handing MySQL the PostgreSQL guard instead would be worse than useless: `pg_query`
//! parses PostgreSQL, so valid MySQL DDL would come back as a syntax error and be
//! denied. That is why `SqlGuard::check` and
//! `SqlGuard::check_raw_island_sql_backstop` both refuse a non-PostgreSQL dialect
//! outright (`GuardError::RawSqlRejected`, carrying MySQL's id) rather than
//! mis-vetting the text —
//! a backstop for the wrong caller, kept unchanged.
//!
//! # This is not the whole of MySQL's data security
//!
//! `data_security.destructive_ops = forbid` IS enforced for MySQL, by
//! `zero_migrate_guard::guard::check_ir_data_security_policy`, over the structured IR.
//! Its gate reads `if cfg.dialect() != &POSTGRES` — it exists
//! because the guard here is empty and is handed no policy. Do not read the empty
//! outcome below as "MySQL enforces nothing"; read it as "MySQL enforces at the IR,
//! not at the SQL text".
//!
//! # Why this is a separate type from SQLite's
//!
//! Both dialects used to run one `SqliteDescriptorGuard` — a type named after the
//! other vendor, whose own doc had to explain that the name was wrong. Each vendor
//! now writes its own trusting guard, so a change to one dialect's posture cannot
//! silently become a change to the other's.

use zero_migrate_backend::guard::{GuardConfig, GuardError, GuardOutcome, MigrationGuard};

/// MySQL's line-1: trust the descriptor output.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlGuard;

impl MysqlGuard {
    /// Construct the descriptor guard (stateless).
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl MigrationGuard for MysqlGuard {
    fn check(&self, _up: &str) -> Result<GuardOutcome, GuardError> {
        // Descriptor-generated DDL is trusted at the author boundary; there is no
        // MySQL parser to vet text with. The empty clean outcome.
        Ok(GuardOutcome::default())
    }
}

/// This vendor's `BackendVendor::guard` factory.
///
/// The config is ignored: this guard holds no state and reads no knob, which is what
/// makes `check_ir_data_security_policy`'s non-PostgreSQL arm load-bearing.
#[must_use]
pub fn guard(_cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    Box::new(MysqlGuard::new())
}
