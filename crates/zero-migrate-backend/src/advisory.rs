//! The neutral ADVISORY vocabulary every backend's line-1 guard reports in.
//!
//! An [`Advisory`] is what a guard says when a migration is *operationally risky
//! but not a security threat*: data loss, a backward-incompatible rename,
//! lock-heavy DDL, a full-table rewrite, an un-validated constraint, a missing FK
//! index. It carries a stable [`rule`] id, a [`Severity`], a human message and an
//! optional safer-alternative suggestion.
//!
//! # Why the vocabulary is here and the analyzers are not
//!
//! [`crate::guard::GuardOutcome`] — the neutral seam every backend's
//! [`crate::guard::MigrationGuard`] returns — carries `Vec<Advisory>`, so the TYPE
//! has to sit below every vendor. The analyzers that PRODUCE advisories do not: they
//! read a `libpg_query` parse tree and live in `zero-migrate-postgres` alongside the
//! parser they depend on. A descriptor-only backend emits none and needs no analyzer.
//!
//! # These are ADVISORY, NEVER load-bearing for security
//!
//! The security boundary is a vendor's deny-list (PostgreSQL's parse-time deny-list
//! plus cross-schema confinement) and the least-privilege `migrator` role. The
//! destructive-data-loss gate is the engine's approval gate. **Nothing here denies,
//! blocks, or gates anything.** An analyzer that fails to fire is a quality
//! regression, NOT a security hole; a spurious advisory is noise, never a denial.
//!
//! # Every constructor below is `pub`, and that costs nothing
//!
//! Before the split these constructors were private / `pub(crate)`, because the
//! analyzers that call them shared a crate with the type. They no longer do, so they
//! are `pub`. No invariant is lost: [`Advisory`]'s four fields are ALL `pub`, so any
//! caller could always write the struct literal directly. The constructors are
//! convenience, never a capability.

/// The severity of an [`Advisory`]. Advisory-only — neither level denies or
/// gates; both are informational signals about an operational footgun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    /// A risky operation likely to cause downtime, data loss, or break running
    /// code (a lock-heavy rewrite, a destructive drop, a backward-incompatible
    /// rename). The migration still applies — this is a heads-up, not a denial.
    Warning,
    /// A softer performance/footprint note (e.g. an FK column with no supporting
    /// index). Worth fixing, lower urgency than a [`Severity::Warning`].
    Notice,
}

/// One operational advisory emitted by an analyzer.
///
/// Carries a stable [`rule`](Self::rule) id (so callers can suppress/route a
/// specific analyzer), a [`severity`](Self::severity), a human
/// [`message`](Self::message) describing the footgun, and an optional
/// [`suggestion`](Self::suggestion) naming the safer alternative (usually the
/// expand-contract path).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Advisory {
    /// The stable analyzer rule id (see [`rule`]). Distinct from the guard's
    /// deny-list `rule` namespace — these never deny.
    pub rule: &'static str,
    /// How urgent the advisory is.
    pub severity: Severity,
    /// A human-readable description of the operational risk.
    pub message: String,
    /// The safer alternative to suggest, if any.
    pub suggestion: Option<String>,
}

impl Advisory {
    /// Build a `Warning`-severity advisory.
    pub fn warning(rule: &'static str, message: String, suggestion: &str) -> Self {
        Self {
            rule,
            severity: Severity::Warning,
            message,
            suggestion: Some(suggestion.to_string()),
        }
    }

    /// Build a `Notice`-severity advisory.
    pub fn notice(rule: &'static str, message: String, suggestion: &str) -> Self {
        Self {
            rule,
            severity: Severity::Notice,
            message,
            suggestion: Some(suggestion.to_string()),
        }
    }

    /// Build the structured warning emitted by `data_security.destructive_ops = "warn"`.
    pub fn destructive_ops_warn(operation: &str, statement: &str) -> Self {
        Self {
            rule: rule::DATA_SECURITY_DESTRUCTIVE_OPS_WARN,
            severity: Severity::Warning,
            message: format!(
                "data_security.destructive_ops=warn: {operation} is destructive and requires review: {statement}"
            ),
            suggestion: Some(
                "review this destructive migration explicitly, or set destructive_ops=\"forbid\" to refuse it"
                    .to_string(),
            ),
        }
    }

    /// Build the structured warning emitted when
    /// `data_security.destructive_ops = "warn"` sees an unclassified statement.
    pub fn destructive_ops_unknown_warn(statement: &str) -> Self {
        Self {
            rule: rule::DATA_SECURITY_UNCLASSIFIED_OPS_WARN,
            severity: Severity::Warning,
            message: format!(
                "data_security.destructive_ops=warn: statement is not positively classified as non-destructive and requires review: {statement}"
            ),
            suggestion: Some(
                "review this migration explicitly; destructive_ops=\"forbid\" refuses unclassified statements fail-closed"
                    .to_string(),
            ),
        }
    }
}

/// The stable advisory rule ids — **data, not logic**, mirroring the guard's
/// `denylist::rule` convention. These are NOT security rules; they never deny.
pub mod rule {
    /// `data_security.destructive_ops = "warn"` surfaced a destructive operation.
    pub const DATA_SECURITY_DESTRUCTIVE_OPS_WARN: &str = "DATA_SECURITY_DESTRUCTIVE_OPS_WARN";
    /// `data_security.destructive_ops = "warn"` surfaced an unclassified operation.
    pub const DATA_SECURITY_UNCLASSIFIED_OPS_WARN: &str = "DATA_SECURITY_UNCLASSIFIED_OPS_WARN";
    /// `DROP TABLE`/`DROP COLUMN`/`DROP CONSTRAINT` — irreversible data loss.
    pub const DESTRUCTIVE_DROP: &str = "DESTRUCTIVE_DROP";
    /// `RENAME COLUMN`/`RENAME TABLE` — breaks code reading the old name.
    pub const BACKWARD_INCOMPATIBLE_RENAME: &str = "BACKWARD_INCOMPATIBLE_RENAME";
    /// `ALTER COLUMN … TYPE` — may lose data / rewrites the table.
    pub const LOSSY_TYPE_CHANGE: &str = "LOSSY_TYPE_CHANGE";
    /// `ADD COLUMN NOT NULL` with no default — fails on a non-empty table.
    pub const ADD_NOT_NULL_NO_DEFAULT: &str = "ADD_NOT_NULL_NO_DEFAULT";
    /// `ALTER COLUMN … SET NOT NULL` — full table scan under lock.
    pub const SET_NOT_NULL_FULL_SCAN: &str = "SET_NOT_NULL_FULL_SCAN";
    /// `ADD CONSTRAINT` (FK/UNIQUE/CHECK) without `NOT VALID` — validates all
    /// existing rows under lock.
    pub const CONSTRAINT_NOT_VALIDATED: &str = "CONSTRAINT_NOT_VALIDATED";
    /// Plain `CREATE INDEX` (not `CONCURRENTLY`) — blocks writes for the build.
    pub const NON_CONCURRENT_INDEX: &str = "NON_CONCURRENT_INDEX";
    /// An `ACCESS EXCLUSIVE` table rewrite forced by a volatile-default
    /// `ADD COLUMN` — the only statement that raises THIS rule.
    ///
    /// `ALTER COLUMN … TYPE` rewrites the table too, and says so, but reports it
    /// under [`LOSSY_TYPE_CHANGE`]: one statement, one advisory, carrying both
    /// the data-loss risk and the rewrite. Verified against live PostgreSQL by
    /// comparing `pg_relation_filenode` before and after each statement — a
    /// constant `DEFAULT` does not rewrite on PG11+ and correctly raises nothing,
    /// a volatile one does and raises this.
    pub const TABLE_REWRITE: &str = "TABLE_REWRITE";
    /// An FK referencing column with no supporting index in the same migration.
    pub const FK_WITHOUT_INDEX: &str = "FK_WITHOUT_INDEX";
    /// `TRUNCATE` — deletes all rows; irreversible and not MVCC-rolled-back the
    /// way a `DELETE` is (it resets storage; under some setups it cannot be
    /// rolled back cleanly).
    pub const TRUNCATE_DATA_LOSS: &str = "TRUNCATE_DATA_LOSS";
    /// A lock-heavy maintenance op — `CLUSTER`, `VACUUM FULL`, or a non-concurrent
    /// `REINDEX` — that takes an ACCESS EXCLUSIVE / heavy lock for its duration.
    pub const LOCK_HEAVY_MAINTENANCE: &str = "LOCK_HEAVY_MAINTENANCE";
}
