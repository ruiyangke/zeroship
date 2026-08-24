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
//! `zero_migrate_backend::guard::check_ir_data_security_policy`, over the structured IR.
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
use zero_migrate_ir::migration::MigrationFlags;

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

    /// REFUSED, not waved through.
    ///
    /// A raw island is `Op::Raw` — PostgreSQL text. Reaching MySQL with it is a
    /// mis-dispatch, not a trusted operation, so this refuses with MySQL's own id
    /// rather than returning `Ok`. Returning `Ok` here would grant MySQL an unchecked
    /// raw door that no MySQL author can even open.
    fn check_raw_island_sql(&self, _sql: &str) -> Result<(), GuardError> {
        Err(GuardError::RawSqlRejected {
            dialect: crate::DIALECT,
        })
    }

    /// REFUSED, for the same reason as [`MysqlGuard::check_raw_island_sql`]: a raw
    /// function body reaching this vendor is PostgreSQL text on the wrong path.
    fn check_raw_island_body(&self, _body: &str, _raw: &str) -> Result<(), GuardError> {
        Err(GuardError::RawSqlRejected {
            dialect: crate::DIALECT,
        })
    }

    /// `false` because MySQL HAS NO RAW DOOR, not because raw SQL is trusted here.
    ///
    /// This is the "I have no raw door" answer the neutral net-state walk asks for.
    /// `Op::Raw` cannot reach a MySQL apply: [`MysqlGuard::check_raw_island_sql`]
    /// above refuses it, and there is no MySQL raw author to emit one. So there is no
    /// island whose net table state could escape the walk, and nothing needs parsing
    /// to establish that.
    ///
    /// What this consequently does NOT check: nothing. Every MySQL op the walk sees is
    /// structured, and the walk reads all of them.
    fn raw_island_escapes_rls_net_state(&self, _sql: &str) -> bool {
        false
    }

    /// `false` — this guard is constructed WITHOUT the composed policy, so it cannot
    /// read `data_security.destructive_ops` at all, let alone refuse on it.
    ///
    /// The neutral posture walk in
    /// [`check_ir_data_security_policy`](zero_migrate_backend::guard::check_ir_data_security_policy)
    /// is consequently the ONLY enforcement that knob has on this backend. Answering
    /// `true` would turn it off and make the knob silently inert — which is exactly
    /// what it was before that walk existed: a `DROP TABLE` applied under the default
    /// `forbid`.
    fn refuses_destructive_ops_itself(&self) -> bool {
        false
    }

    /// CONSERVATIVE, because MySQL cannot classify SQL text at all.
    ///
    /// There is no MySQL parser in this workspace, so no `destructive` /
    /// `non_transactional` / rename facet can be READ OUT of a `up` blob. Rather than
    /// return the default flag set — which would silently assert "not destructive, no
    /// approval needed" about text nobody inspected — this returns
    /// `requires_approval: true`.
    ///
    /// What is NO LONGER CHECKED, stated plainly: a raw-SQL-authored MySQL migration
    /// gets NO destructive classification, NO non-transactional detection and NO
    /// bare-rename or `SET NOT NULL` gate. It is gated on approval instead, so a human
    /// looks at every one. MySQL's real posture is the descriptor path plus
    /// `check_ir_data_security_policy` over the structured IR; the raw-SQL author is
    /// not a MySQL authoring route.
    fn flags_for_sql(&self, _up: &str) -> Result<MigrationFlags, GuardError> {
        Ok(MigrationFlags {
            requires_approval: true,
            ..MigrationFlags::default()
        })
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
