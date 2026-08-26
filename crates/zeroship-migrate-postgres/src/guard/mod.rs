//! PostgreSQL's line-1 defense - the `libpg_query` deny-list, behind the contract.
//!
//! This is the vendor half of `zeroship_migrate_backend::guard::MigrationGuard`: [`PgGuard`]
//! is the adapter that files this vendor's machinery under its `BackendVendor`, so
//! the engine reaches it the same way it reaches this vendor's two renderers -
//! through the registry, never by naming a dialect.
//!
//! # Why the machinery itself is HERE now
//!
//! The parse-time deny-list, the cross-schema confinement, the classifier and the
//! operational analyzers used to live in a separate `zero-migrate-guard` crate,
//! which the engine depended on directly. That crate is gone: everything in it
//! needed `libpg_query` to parse PostgreSQL, which makes all of it this vendor's
//! code, and a neutral engine has no business depending on a PostgreSQL parser.
//!
//! [`sql`] holds that machinery ([`SqlGuard`], the deny-walk, the namespace
//! authority rules); [`denylist`] holds the rule ids it refuses with; and
//! [`crate::analysis`] holds the classifier and the operational analyzers.
//!
//! This crate is the sole PRODUCTION owner of `libpg_query`. The shape gates that
//! once justified a second `[dependencies]` entry in `crates/zero-migrate/Cargo.toml`
//! - the precondition boolean-SELECT gate, the idempotence/txn scans, the backfill
//! cursor check - are `src/backend/{precondition,session,backfill_sql}.rs` in THIS
//! crate. The engine's remaining entry is `[dev-dependencies]`, for test parses.
//!
//! This doc used to say the opposite, and read as a live blocker long after the
//! relocation closed it. Re-measure rather than trust it:
//! `git grep -n '^pg_query' -- crates` names every manifest that declares it, and the
//! section header above each hit says whether it is production or test-only.
//!
//! What did NOT come with it is the neutral seam - [`GuardConfig`], [`GuardError`],
//! [`GuardOutcome`], [`MigrationGuard`] and the structured-IR data-security walk all
//! live in `zeroship_migrate_backend::guard`, below every vendor, because every vendor's
//! guard is configured by the same policy and reports in the same vocabulary.

pub mod denylist;
pub mod sql;

pub use sql::{
    check_raw_view_body, check_raw_view_body_text, extract_string_literals, flags_for,
    namespace_rule, GuardReport, RawViewBodyDefect, SqlGuard,
};

use zeroship_migrate_backend::guard::{GuardConfig, GuardError, GuardOutcome, MigrationGuard};
use zeroship_migrate_ir::migration::MigrationFlags;

/// The PostgreSQL line-1: the `libpg_query` deny-list + cross-schema confinement +
/// classify + analyze, mapped onto the neutral `GuardOutcome`.
///
/// Behavior-identical to calling `SqlGuard::check` - `check` only drops the
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

    /// The full deny-walk's narrower sibling: the "even trusted text may not do THIS"
    /// set, run over one rendered raw island when the posture has already skipped the
    /// belt. Nothing is waved through here - a non-PostgreSQL dialect handed this
    /// guard is refused outright rather than mis-vetted against PG grammar.
    fn check_raw_island_sql(&self, sql: &str) -> Result<(), GuardError> {
        self.0.check_raw_island_sql_backstop(sql)
    }

    /// PL/pgSQL is only best-effort parseable as SQL, so this is the body scanner
    /// rather than a parse: inspect dynamic-SQL literals, then token scan for
    /// deny-listed names.
    fn check_raw_island_body(&self, body: &str, raw: &str) -> Result<(), GuardError> {
        self.0.check_raw_island_body_backstop(body, raw)
    }

    /// PostgreSQL HAS a raw door (`Op::Raw`), so it must answer this from the
    /// parser: an island is attributed to the relations its parse names, and one
    /// naming only unobligated relations is admitted. Unparseable text, an unpinnable
    /// schema, and a statement naming no relation at all are all treated as inside the
    /// reach - fail-closed, because none of them can be proved harmless.
    fn raw_island_escapes_rls_net_state(&self, sql: &str) -> bool {
        self.0.raw_island_within_require_rls(sql)
    }

    /// `true` - this guard reads `data_security.destructive_ops` in its own SQL-text
    /// walk and refuses there, naming the rendered statement.
    ///
    /// The neutral posture walk in
    /// [`check_ir_data_security_policy`](zeroship_migrate_backend::guard::check_ir_data_security_policy)
    /// therefore skips this backend: a second, EARLIER denial would replace a refusal
    /// that names the statement with one that names an op index, changing a message
    /// existing assertions pin, for no behavioural gain. The knob is enforced either
    /// way, which is the only reason `true` is safe to answer here.
    fn refuses_destructive_ops_itself(&self) -> bool {
        true
    }

    /// PostgreSQL can classify its own statements, so the flags are read back out of
    /// the text: destructive => `requires_approval`, any non-transactional statement =>
    /// `transactional: false`, plus the bare-rename and `SET NOT NULL` gates.
    ///
    /// A *denial* is not raised here - the engine's `plan()` re-runs the guard and
    /// records denials so a caller sees every problem at once. A denied-but-parseable
    /// migration gets conservative flags and is still minted, so `plan` can report the
    /// denial precisely. Only an UNPARSEABLE `up` errors.
    fn flags_for_sql(&self, up: &str) -> Result<MigrationFlags, GuardError> {
        match self.0.check(up) {
            Ok(report) => Ok(flags_for(&report)),
            Err(GuardError::Parse(e)) => Err(GuardError::Parse(e)),
            Err(_) => Ok(MigrationFlags {
                // Conservative: treat as requiring approval until plan/gate decides.
                requires_approval: true,
                ..MigrationFlags::default()
            }),
        }
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
