//! SQLite ships NO operational analyzer, and this file is where that is chosen.
//!
//! `libpg_query` cannot parse SQLite, and there is no SQLite analyzer in this
//! workspace, so there is nothing to run. Handing SQLite the PostgreSQL analyzers
//! would report every statement as unremarkable by failing to read it.
//!
//! # What is NOT being checked, stated plainly
//!
//! An operator deploying SQLite gets no lock-heavy-DDL warning, no
//! full-table-rewrite warning, no un-validated-constraint notice, no
//! missing-FK-index notice, and no backward-incompatible-rename warning.
//!
//! The gap is narrower here than it is for MySQL, and the reason is worth stating
//! rather than assuming: SQLite migrations are produced ONLY by the declarative
//! differ, there is no raw-SQL SQLite author, and the differ's 12-step table
//! rebuilds are already routed through the engine's destructive/approval gate. So
//! the shapes an analyzer would warn about are largely shapes the engine has already
//! made the operator approve. LARGELY, not entirely — this is an argument that the
//! absence costs less on SQLite, not that it costs nothing.
//!
//! # Which is why the empty list is not what this returns
//!
//! [`SqliteAdvisor::advise`] returns
//! [`AdvisoryVerdict::NotAnalyzed`](zero_migrate_backend::advisory::AdvisoryVerdict::NotAnalyzed),
//! carrying this vendor's id and the sentence above, so a caller cannot read the
//! absence as a clean bill of health.
//!
//! # Why this is not shared with MySQL
//!
//! For the same reason the two guards are not shared. Both backends refuse, but they
//! refuse for different reasons and at different cost, and a single shared type would
//! make a change to one backend's posture silently become a change to the other's.

use zero_migrate_backend::advisory::{
    AdvisoryVerdict, AnalyzerAbsent, IndexCoverage, OperationalAdvisor,
};
use zero_migrate_ir::dialect::{DialectId, SQLITE};

const DIALECT: DialectId = SQLITE;

/// This vendor's own words for what is not being checked. Read by BOTH trait
/// methods, so they cannot state different postures.
const NO_ANALYZER: &str = "no SQLite parser ships in this engine, so no operational \
                           analyzer was run against these statements";

/// SQLite's advisory posture: none, deliberately.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteAdvisor;

impl SqliteAdvisor {
    /// The single description of this backend's absent analyzer, so
    /// [`OperationalAdvisor::advise`] and [`OperationalAdvisor::analyzer_absence`]
    /// answer from one place.
    fn absent() -> AnalyzerAbsent {
        AnalyzerAbsent {
            dialect: DIALECT,
            reason: NO_ANALYZER,
        }
    }
}

impl OperationalAdvisor for SqliteAdvisor {
    fn advise(&self, _sql: &str) -> AdvisoryVerdict {
        AdvisoryVerdict::NotAnalyzed(Self::absent())
    }

    fn analyzer_absence(&self) -> Option<AnalyzerAbsent> {
        Some(Self::absent())
    }

    fn index_coverage(&self, _sql: &str) -> IndexCoverage {
        // Empty, and safe: an empty coverage can only ever suppress LESS. See
        // `IndexCoverage`'s own docs for why this needs no not-analyzed arm.
        IndexCoverage::none()
    }
}

/// This vendor's `BackendVendor::advisor`.
pub static ADVISOR: SqliteAdvisor = SqliteAdvisor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_statement_is_reported_as_clean() {
        let AdvisoryVerdict::NotAnalyzed(absent) = ADVISOR.advise("DROP TABLE \"orders\"") else {
            panic!("SQLite ships no analyzer, so no statement may come back Analyzed");
        };
        assert_eq!(absent.dialect, DIALECT);
        assert!(absent.advisory().message.contains("UNCHECKED"));
    }

    #[test]
    fn both_methods_state_the_same_posture() {
        let absence = ADVISOR
            .analyzer_absence()
            .expect("SQLite must report an absent analyzer");
        let AdvisoryVerdict::NotAnalyzed(from_advise) = ADVISOR.advise("SELECT 1") else {
            panic!("advise must agree with analyzer_absence");
        };
        assert_eq!(absence, from_advise);
    }
}
