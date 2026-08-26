//! MySQL ships NO operational analyzer, and this file is where that is chosen.
//!
//! There is no MySQL parser in this workspace. The only analyzer the engine has
//! reads a `libpg_query` parse tree, and MySQL renders identifiers with backticks,
//! which is not valid PostgreSQL - so handing MySQL the PostgreSQL analyzers would
//! not degrade gracefully. Every statement would fail to parse and come back with no
//! advisories, for SQL THIS ENGINE EMITS and is about to run.
//!
//! # What is NOT being checked, stated plainly
//!
//! An operator deploying MySQL gets no lock-heavy-DDL warning, no
//! full-table-rewrite warning, no un-validated-constraint notice, no
//! missing-FK-index notice, and no backward-incompatible-rename warning. A MySQL
//! `ALTER TABLE` that takes a table-wide lock on a populated table is applied with
//! nothing anywhere telling the operator it is coming. That is a real gap and it is
//! named here rather than implied by an empty list.
//!
//! # Which is why the empty list is not what this returns
//!
//! [`MysqlAdvisor::advise`] returns
//! [`AdvisoryVerdict::NotAnalyzed`],
//! carrying this vendor's id and the sentence above. A caller cannot read that as
//! "looked and found nothing", because it is not a list at all until the caller has
//! handled the absence. The distinction is not theoretical: the engine's advisory
//! report on MySQL WAS a clean empty list, produced by parse failures, and it took a
//! bug report to notice.
//!
//! # This is not the whole of MySQL's operational safety
//!
//! The security line-1 is [`crate::guard`] (empty here too, and for its own stated
//! reasons), `data_security.destructive_ops` is enforced over the structured IR
//! rather than over SQL text, and the engine's approval gate still confirms every
//! destructive op. What is missing is the ADVISORY layer - the warnings an operator
//! reads before choosing to deploy - and only that.

use zero_migrate_backend::advisory::{
    AdvisoryVerdict, AnalyzerAbsent, IndexCoverage, OperationalAdvisor,
};

use crate::DIALECT;

/// This vendor's own words for what is not being checked. Read by BOTH trait
/// methods, so they cannot state different postures.
const NO_ANALYZER: &str = "no MySQL parser ships in this engine, so no operational \
                           analyzer was run against these statements";

/// MySQL's advisory posture: none, deliberately.
#[derive(Debug, Clone, Copy, Default)]
pub struct MysqlAdvisor;

impl MysqlAdvisor {
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

impl OperationalAdvisor for MysqlAdvisor {
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
pub static ADVISOR: MysqlAdvisor = MysqlAdvisor;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_statement_is_reported_as_clean() {
        // Backtick-quoted MySQL DDL that the PostgreSQL analyzer could not parse.
        // The point of the assertion is that the answer is NOT an empty advisory
        // list - it is the refusal to claim one.
        let AdvisoryVerdict::NotAnalyzed(absent) = ADVISOR.advise("DROP TABLE `orders`") else {
            panic!("MySQL ships no analyzer, so no statement may come back Analyzed");
        };
        assert_eq!(absent.dialect, DIALECT);
        assert!(absent.advisory().message.contains("UNCHECKED"));
    }

    #[test]
    fn both_methods_state_the_same_posture() {
        let absence = ADVISOR
            .analyzer_absence()
            .expect("MySQL must report an absent analyzer");
        let AdvisoryVerdict::NotAnalyzed(from_advise) = ADVISOR.advise("SELECT 1") else {
            panic!("advise must agree with analyzer_absence");
        };
        assert_eq!(absence, from_advise);
    }
}
