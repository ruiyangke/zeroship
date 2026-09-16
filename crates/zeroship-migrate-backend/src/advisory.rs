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
//! [`crate::guard::GuardOutcome`] - the neutral seam every backend's
//! [`crate::guard::MigrationGuard`] returns - carries `Vec<Advisory>`, so the TYPE
//! has to sit below every vendor. The analyzers that PRODUCE advisories do not: they
//! read a `libpg_query` parse tree and belong with the parser they depend on. A
//! descriptor-only backend emits none and needs no analyzer.
//!
//! [`OperationalAdvisor`] is the seam that makes that true rather than merely
//! stated. It is a REQUIRED [`crate::registry::BackendVendor`] field, so each
//! backend files its own answer and the engine reaches an analysis by asking the
//! registered vendor - never by naming one. PostgreSQL's implementation delegates to
//! its own `analysis::analyze` module, which holds the `libpg_query` analyzers. This
//! contract keeps those analyzers in the vendor crate: the engine reaches an
//! analysis by asking the registered vendor, never by naming a parser itself.
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
//! The analyzers that call these constructors live in the vendor crate, so the
//! constructors are `pub`. No invariant is lost: every [`Advisory`] field is `pub`, so any
//! caller could always write the struct literal directly. The constructors are
//! convenience, never a capability.

/// The severity of an [`Advisory`]. Advisory-only - neither level denies or
/// gates; both are informational signals about an operational footgun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Severity {
    /// A risky operation likely to cause downtime, data loss, or break running
    /// code (a lock-heavy rewrite, a destructive drop, a backward-incompatible
    /// rename). The migration still applies - this is a heads-up, not a denial.
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
    /// deny-list `rule` namespace - these never deny.
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

/// The stable advisory rule ids - **data, not logic**, mirroring the guard's
/// `denylist::rule` convention. These are NOT security rules; they never deny.
pub mod rule {
    /// `data_security.destructive_ops = "warn"` surfaced a destructive operation.
    pub const DATA_SECURITY_DESTRUCTIVE_OPS_WARN: &str = "DATA_SECURITY_DESTRUCTIVE_OPS_WARN";
    /// `data_security.destructive_ops = "warn"` surfaced an unclassified operation.
    pub const DATA_SECURITY_UNCLASSIFIED_OPS_WARN: &str = "DATA_SECURITY_UNCLASSIFIED_OPS_WARN";
    /// `DROP TABLE`/`DROP COLUMN`/`DROP CONSTRAINT` - irreversible data loss.
    pub const DESTRUCTIVE_DROP: &str = "DESTRUCTIVE_DROP";
    /// `RENAME COLUMN`/`RENAME TABLE` - breaks code reading the old name.
    pub const BACKWARD_INCOMPATIBLE_RENAME: &str = "BACKWARD_INCOMPATIBLE_RENAME";
    /// `ALTER COLUMN ... TYPE` - may lose data / rewrites the table.
    pub const LOSSY_TYPE_CHANGE: &str = "LOSSY_TYPE_CHANGE";
    /// `ADD COLUMN NOT NULL` with no default - fails on a non-empty table.
    pub const ADD_NOT_NULL_NO_DEFAULT: &str = "ADD_NOT_NULL_NO_DEFAULT";
    /// `ALTER COLUMN ... SET NOT NULL` - full table scan under lock.
    pub const SET_NOT_NULL_FULL_SCAN: &str = "SET_NOT_NULL_FULL_SCAN";
    /// `ADD CONSTRAINT` (FK/UNIQUE/CHECK) without `NOT VALID` - validates all
    /// existing rows under lock.
    pub const CONSTRAINT_NOT_VALIDATED: &str = "CONSTRAINT_NOT_VALIDATED";
    /// Plain `CREATE INDEX` (not `CONCURRENTLY`) - blocks writes for the build.
    pub const NON_CONCURRENT_INDEX: &str = "NON_CONCURRENT_INDEX";
    /// An `ACCESS EXCLUSIVE` table rewrite forced by a volatile-default
    /// `ADD COLUMN` - the only statement that raises THIS rule.
    ///
    /// `ALTER COLUMN ... TYPE` rewrites the table too, and says so, but reports it
    /// under [`LOSSY_TYPE_CHANGE`]: one statement, one advisory, carrying both
    /// the data-loss risk and the rewrite. Verified against live PostgreSQL by
    /// comparing `pg_relation_filenode` before and after each statement - a
    /// constant `DEFAULT` does not rewrite on PG11+ and correctly raises nothing,
    /// a volatile one does and raises this.
    pub const TABLE_REWRITE: &str = "TABLE_REWRITE";
    /// An FK referencing column with no supporting index in the same migration.
    pub const FK_WITHOUT_INDEX: &str = "FK_WITHOUT_INDEX";
    /// `TRUNCATE` - deletes all rows; irreversible and not MVCC-rolled-back the
    /// way a `DELETE` is (it resets storage; under some setups it cannot be
    /// rolled back cleanly).
    pub const TRUNCATE_DATA_LOSS: &str = "TRUNCATE_DATA_LOSS";
    /// A lock-heavy maintenance op - `CLUSTER`, `VACUUM FULL`, or a non-concurrent
    /// `REINDEX` - that takes an ACCESS EXCLUSIVE / heavy lock for its duration.
    pub const LOCK_HEAVY_MAINTENANCE: &str = "LOCK_HEAVY_MAINTENANCE";
    /// The backend that was asked ships NO operational analyzer, so nothing here
    /// was evaluated. See [`super::AnalyzerAbsent`] - this is the one rule id in
    /// this module that reports the ABSENCE of analysis rather than a finding.
    ///
    /// The lower-case spelling is deliberate and is pinned by a host test
    /// (`packages/zero-migrate-cli/tests/host/locking-advisory-surface.test.ts`),
    /// which is why it is a named const rather than a literal typed at each site.
    pub const ANALYZER_DIALECT_UNSUPPORTED: &str = "analyzer_dialect_unsupported";
}

// ---------------------------------------------------------------------------
// The analyzer CONTRACT.
// ---------------------------------------------------------------------------

/// A backend that ships no operational analyzer, saying so in its own words.
///
/// # Why this type exists rather than an empty `Vec<Advisory>`
///
/// This is a MEASURED defect, not a hypothetical one. The analyzers parse
/// PostgreSQL. MySQL renders identifiers with backticks, which is not valid
/// PostgreSQL, so every statement failed to parse and the analyzer returned an
/// empty vector - for SQL THIS ENGINE EMITS and was about to run. The result was a
/// clean advisory report on MySQL that meant "could not read any of this",
/// indistinguishable from "looked and found nothing".
///
/// So a backend without an analyzer does not return an empty list. It returns
/// [`AdvisoryVerdict::NotAnalyzed`] carrying one of these, and a caller cannot
/// reach a list of advisories without having first handled the case where there
/// was no analysis at all.
///
/// # Why it carries a `DialectId`
///
/// This is PROVENANCE - data recording WHICH backend has no analyzer - and it never
/// dispatches on the value. The same argument
/// `crate::vendor::VendorError::VendorOpsUnsupported` records at length applies
/// verbatim: typed as a closed enum, a fourth backend would have no variant to name
/// itself with and so no value it could legally return from the required method.
/// Each vendor reads it from that module's own `DIALECT` const, so the
/// one-dialect-literal rule is unaffected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzerAbsent {
    /// Which backend has no analyzer.
    pub dialect: zeroship_migrate_ir::dialect::DialectId,
    /// That backend's own statement of what is NOT being checked. Written at the
    /// vendor's definition site, so the sentence an operator reads is attributable
    /// to the backend it is about.
    pub reason: &'static str,
}

impl AnalyzerAbsent {
    /// The absence, rendered as the one [`Advisory`] an operator-facing report
    /// carries in place of findings.
    ///
    /// Advisory-shaped on purpose: every consumer of this contract already has a
    /// channel for `Advisory`, and the absence has to travel down that SAME channel
    /// or it is not seen. `Notice` rather than `Warning` because nothing here says a
    /// migration is dangerous - it says nobody looked.
    #[must_use]
    pub fn advisory(&self) -> Advisory {
        Advisory {
            rule: rule::ANALYZER_DIALECT_UNSUPPORTED,
            severity: Severity::Notice,
            message: format!(
                "operational advisories are not available for {}: {}. \
                 An empty advisory list here means UNCHECKED, not clean",
                self.dialect.as_str(),
                self.reason
            ),
            suggestion: None,
        }
    }
}

/// What a backend's analyzer says about one statement, or that it has none.
///
/// The two arms are not interchangeable and the enum is what stops them being
/// confused: `Analyzed(vec![])` means the analyzer ran and found nothing;
/// [`NotAnalyzed`](Self::NotAnalyzed) means no analyzer ran. See [`AnalyzerAbsent`]
/// for the defect that made the distinction load-bearing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvisoryVerdict {
    /// The backend's analyzers ran. This is the complete finding - an empty vector
    /// here genuinely means "clean".
    Analyzed(Vec<Advisory>),
    /// The backend ships no analyzer for this input. NOT clean - unchecked.
    NotAnalyzed(AnalyzerAbsent),
}

impl AdvisoryVerdict {
    /// The verdict flattened into the advisory list a report shows, with the
    /// absence rendered as its own [`Advisory`].
    ///
    /// This is the accessor a report path must use, because it cannot lose the
    /// absence: the not-analyzed arm becomes a non-empty list carrying
    /// [`rule::ANALYZER_DIALECT_UNSUPPORTED`]. The [`Analyzed`](Self::Analyzed)
    /// payload is public, so destructuring the variant directly also yields a bare
    /// `Vec<Advisory>` - and silently drops the distinction this enum exists for.
    #[must_use]
    pub fn into_report(self) -> Vec<Advisory> {
        match self {
            Self::Analyzed(advisories) => advisories,
            Self::NotAnalyzed(absent) => vec![absent.advisory()],
        }
    }
}

/// The index-coverage facts a PLAN-WIDE advisory suppression needs from a backend.
///
/// The per-statement [`rule::FK_WITHOUT_INDEX`] analyzer only sees one statement, so
/// it can suppress the notice only for an index created in that SAME statement. A
/// caller holding a whole plan aggregates [`Self::indexed_columns`] across every
/// migration and re-tests each migration's [`Self::fk_columns_needing_index`]
/// against it. Reading SQL to answer either is a parser fact, which is why it is the
/// backend that answers and not the caller.
///
/// # Why this needs no `NotAnalyzed` arm
///
/// Because an empty answer here can only ever suppress LESS. The suppression fires
/// only when `fk_columns_needing_index` is non-empty AND every column in it appears
/// in the plan-wide `indexed_columns`; a backend that returns
/// [`Self::none`] therefore leaves every advisory in place. That is the fail-loud
/// direction, so the ambiguity [`AnalyzerAbsent`] exists to prevent cannot arise.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexCoverage {
    /// Every column that gains a covering index anywhere in the analyzed SQL - a
    /// leading `CREATE INDEX` column, an inline or table-level `PRIMARY KEY`/
    /// `UNIQUE`, or an `ALTER TABLE ... ADD CONSTRAINT`/`ADD INDEX`.
    pub indexed_columns: Vec<String>,
    /// The FK **referencing** columns that want a supporting index - the
    /// [`rule::FK_WITHOUT_INDEX`] subjects.
    pub fk_columns_needing_index: Vec<String>,
}

impl IndexCoverage {
    /// The empty coverage a backend with no analyzer reports. See the type's docs
    /// for why this is safe where an empty `Vec<Advisory>` would not be.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }
}

/// What a backend says about a migration's OPERATIONAL risk - the advisory half of
/// the backend contract, alongside [`crate::guard::MigrationGuard`]'s security half.
///
/// # Why this is a contract and not a function in the engine
///
/// The analyzers read a `libpg_query` parse tree. An engine that called them
/// directly would be running the PostgreSQL parser over every backend's DDL,
/// which is both wrong (see [`AnalyzerAbsent`]) and a vendor coupling the engine
/// cannot have: core must not name a backend to get an analysis. It asks the
/// registered vendor, and the vendor answers.
///
/// # Every implementor states its own posture, and absence is a CHOICE
///
/// [`crate::registry::BackendVendor`]'s `advisor` field is not optional and neither
/// method here has a default body, so a backend that ships no analyzer has to write
/// that out - in its own crate, with its own reason, visible in the diff. Nothing
/// can acquire the not-analyzed posture by omission, which is the same discipline
/// `BackendVendor::guard` describes at length and for the same reason.
pub trait OperationalAdvisor: std::fmt::Debug + Send + Sync {
    /// The operational advisories in one statement or one migration's `up`.
    ///
    /// Never denies and never gates - see this module's header. A backend with no
    /// analyzer returns [`AdvisoryVerdict::NotAnalyzed`] REGARDLESS of the input,
    /// including for the empty string, which is what makes
    /// [`Self::analyzer_absence`] answerable without SQL.
    fn advise(&self, sql: &str) -> AdvisoryVerdict;

    /// Whether this backend ships an operational analyzer AT ALL, answered without
    /// SQL. `None` = it analyzes; `Some(absent)` = it does not.
    ///
    /// Separate from [`Self::advise`] because a caller reporting on a SET of
    /// statements must be able to tell an operator the whole set is unchecked
    /// BEFORE it renders the first statement - and must still say so when the set
    /// renders to nothing. Asking `advise("")` would answer the same question by
    /// accident rather than by name.
    ///
    /// An implementor MUST agree with its own [`Self::advise`]. Returning `None`
    /// here and `NotAnalyzed` there would report a set as checked and every
    /// statement in it as unchecked.
    fn analyzer_absence(&self) -> Option<AnalyzerAbsent>;

    /// The index-coverage facts a plan-wide [`rule::FK_WITHOUT_INDEX`] suppression
    /// needs. See [`IndexCoverage`], including why it carries no not-analyzed arm.
    fn index_coverage(&self, sql: &str) -> IndexCoverage;
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_migrate_ir::dialect::DialectId;

    fn absent() -> AnalyzerAbsent {
        AnalyzerAbsent {
            dialect: DialectId::new("duckdb"),
            reason: "no DuckDB parser ships in this engine",
        }
    }

    /// The whole point of the enum: flattening a not-analyzed verdict cannot
    /// produce the empty list that started as a clean report and meant nothing was
    /// read.
    #[test]
    fn a_not_analyzed_verdict_flattens_to_a_non_empty_report() {
        let report = AdvisoryVerdict::NotAnalyzed(absent()).into_report();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].rule, rule::ANALYZER_DIALECT_UNSUPPORTED);
        assert_eq!(report[0].severity, Severity::Notice);
        assert!(report[0].message.contains("duckdb"));
        assert!(report[0].message.contains("UNCHECKED"));
        assert!(report[0].message.contains("no DuckDB parser"));
    }

    /// And the contrast that makes the distinction worth carrying: an analyzed
    /// verdict with no findings IS the empty list, and means clean.
    #[test]
    fn an_analyzed_verdict_with_no_findings_flattens_to_the_empty_report() {
        assert!(AdvisoryVerdict::Analyzed(Vec::new())
            .into_report()
            .is_empty());
    }

    /// The absence names the backend it is about. A notice that does not say WHICH
    /// backend went unchecked cannot be acted on.
    #[test]
    fn the_absence_notice_names_its_own_backend() {
        let advisory = absent().advisory();
        assert!(advisory.message.contains("duckdb"));
        assert_eq!(advisory.suggestion, None);
    }
}
