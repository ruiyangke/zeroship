//! SQLite's line-1 defense - the descriptor-diff path, trusted by construction.
//!
//! SQLite migrations are produced ONLY by the declarative differ
//! (`DeclarativeAuthor::diff`); there is no raw-SQL SQLite author. `libpg_query`
//! cannot parse SQLite, so there is no string deny-list to run: the line-1 vet is the
//! descriptor emitter at the author boundary, and the line-2 defense is the
//! `SqliteBackend`'s runtime authorizer applied per statement at execution.
//!
//! So [`SqliteGuard::check`] returns the EMPTY outcome. That is a deliberate grant of
//! trust, and this file is where it is granted - written out, in this vendor's own
//! crate, attributable to this vendor.
//!
//! # This is not the whole of SQLite's data security
//!
//! Reading only this file would suggest SQLite has no data-security posture at all.
//! It does. `data_security.destructive_ops = forbid` is enforced for SQLite by
//! `zero_migrate_backend::guard::check_ir_data_security_policy`, over the structured IR
//! rather than over SQL text, and its gate is written
//! `if cfg.dialect() != &POSTGRES` - i.e. it exists precisely
//! BECAUSE the guard here is empty and is handed no policy. Enforcement over the IR is
//! the stronger place for a descriptor-only dialect anyway: there is no text to
//! misparse, only ops.
//!
//! # Why this is not shared with MySQL
//!
//! It used to be. Both dialects ran one `SqliteDescriptorGuard`, whose own doc
//! admitted it "serves BOTH descriptor-only engines - SQLite and MySQL - despite the
//! name". Sharing a type named after one vendor made a MySQL reviewer read the
//! dispatch as a bug at a glance. Each vendor now writes its own, which costs a dozen
//! lines and means a change to SQLite's posture cannot silently become a change to
//! MySQL's.

use zero_migrate_backend::guard::{GuardConfig, GuardError, GuardOutcome, MigrationGuard};
use zero_migrate_ir::migration::MigrationFlags;

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

    /// REFUSED, not waved through.
    ///
    /// A raw island is `Op::Raw` - PostgreSQL text. `libpg_query` cannot parse
    /// SQLite and SQLite has no raw author, so an island arriving here is a
    /// mis-dispatch rather than a trusted operation. Returning `Ok` would grant SQLite
    /// an unchecked raw door that no SQLite author can open, so this refuses with
    /// SQLite's own id.
    fn check_raw_island_sql(&self, _sql: &str) -> Result<(), GuardError> {
        Err(GuardError::RawSqlRejected {
            dialect: crate::DIALECT,
        })
    }

    /// REFUSED, for the same reason as [`SqliteGuard::check_raw_island_sql`]: a raw
    /// function body reaching this vendor is PostgreSQL text on the wrong path.
    fn check_raw_island_body(&self, _body: &str, _raw: &str) -> Result<(), GuardError> {
        Err(GuardError::RawSqlRejected {
            dialect: crate::DIALECT,
        })
    }

    /// `false` because SQLite HAS NO RAW DOOR, not because raw SQL is trusted here.
    ///
    /// This is the "I have no raw door" answer the neutral net-state walk asks for.
    /// SQLite migrations are produced ONLY by the declarative differ, and
    /// [`SqliteGuard::check_raw_island_sql`] above refuses an island outright, so there
    /// is no island whose net table state could escape the walk - and establishing
    /// that costs no parse, which is the point of asking the vendor rather than
    /// reaching for `libpg_query` from neutral code.
    ///
    /// What this consequently does NOT check: nothing. Every SQLite op the walk sees
    /// is structured, and the walk reads all of them.
    fn raw_island_escapes_rls_net_state(&self, _sql: &str) -> bool {
        false
    }

    /// `false` - this guard is constructed WITHOUT the composed policy, so it cannot
    /// read `data_security.destructive_ops` at all, let alone refuse on it.
    ///
    /// The neutral posture walk in
    /// [`check_ir_data_security_policy`](zero_migrate_backend::guard::check_ir_data_security_policy)
    /// is consequently the ONLY enforcement that knob has on this backend. Answering
    /// `true` would turn it off and make the knob silently inert - which is exactly
    /// what it was before that walk existed: a `DROP TABLE` applied under the default
    /// `forbid`.
    fn refuses_destructive_ops_itself(&self) -> bool {
        false
    }

    /// CONSERVATIVE, because SQLite cannot classify SQL text at all.
    ///
    /// `libpg_query` parses PostgreSQL, and there is no SQLite parser here, so no
    /// `destructive` / `non_transactional` / rename facet can be READ OUT of an `up`
    /// blob. Rather than return the default flag set - which would silently assert
    /// "not destructive, no approval needed" about text nobody inspected - this
    /// returns `requires_approval: true`.
    ///
    /// What is NO LONGER CHECKED, stated plainly: a raw-SQL-authored SQLite migration
    /// gets NO destructive classification, NO non-transactional detection and NO
    /// bare-rename or `SET NOT NULL` gate. It is gated on approval instead, so a human
    /// looks at every one. SQLite's real posture is the descriptor-diff path, the
    /// backend's runtime authorizer, and `check_ir_data_security_policy` over the
    /// structured IR; the raw-SQL author is not a SQLite authoring route.
    fn flags_for_sql(&self, _up: &str) -> Result<MigrationFlags, GuardError> {
        Ok(MigrationFlags {
            requires_approval: true,
            ..MigrationFlags::default()
        })
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
