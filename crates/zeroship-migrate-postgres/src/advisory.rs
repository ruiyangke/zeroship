//! PostgreSQL's operational analyzers, behind the contract.
//!
//! This is the vendor half of `zero_migrate_backend::advisory::OperationalAdvisor`.
//! The analyzers themselves - the Atlas-style advisory lint suite that reads a
//! `libpg_query` parse tree - live in [`crate::analysis::analyze`], alongside the
//! parser they depend on. What lives HERE is the adapter that files them under this
//! vendor's
//! `BackendVendor`, so the engine reaches an analysis the same way it reaches this
//! vendor's guard and its renderers: through the registry, never by naming a dialect.
//!
//! # Why this adapter is the point of the exercise
//!
//! Before it, the engine called `analyze` directly. Three call sites in
//! `render/declarative.rs` reached `crate::analysis::analyze::...`, which resolved
//! through a re-export straight into the `libpg_query` crate - so the engine ran the
//! PostgreSQL parser over MySQL and SQLite DDL and reported the resulting parse
//! failures as an empty, clean-looking advisory list. That coupling is also what
//! pinned `zero-migrate-guard` in place: the analyzers could not move under this
//! vendor while core called them by name, because core naming a vendor crate is
//! forbidden. With this adapter in place they did move - that crate is gone, and
//! `crate::analysis` is where its analyzers landed.
//!
//! # This module names no dialect
//!
//! It has nothing to name. PostgreSQL is the backend that HAS an analyzer, so its
//! answer is always `Analyzed` and it never needs to identify itself in a refusal.
//! The two backends that do refuse carry their own `DIALECT` const for exactly that
//! provenance.

use crate::analysis::analyze::{analyze, fk_columns_needing_index, indexed_columns};
use zero_migrate_backend::advisory::{
    AdvisoryVerdict, AnalyzerAbsent, IndexCoverage, OperationalAdvisor,
};

/// PostgreSQL's operational analyzers.
///
/// Stateless: it reads SQL and nothing else, which is why the registry holds it as a
/// `&'static dyn` rather than building one per call the way it does the guard.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgAdvisor;

impl OperationalAdvisor for PgAdvisor {
    fn advise(&self, sql: &str) -> AdvisoryVerdict {
        // Unparseable SQL yields no advisories rather than a refusal: the guard
        // already denies it, and the analyzers are best-effort enrichment on top of
        // a statement that parses. `Analyzed` is still the honest arm - PostgreSQL
        // HAS an analyzer and it ran.
        AdvisoryVerdict::Analyzed(analyze(sql))
    }

    fn analyzer_absence(&self) -> Option<AnalyzerAbsent> {
        // This backend analyzes. `advise` above agrees: it returns `Analyzed`
        // unconditionally, so the two cannot disagree.
        None
    }

    fn index_coverage(&self, sql: &str) -> IndexCoverage {
        IndexCoverage {
            indexed_columns: indexed_columns(sql),
            fk_columns_needing_index: fk_columns_needing_index(sql),
        }
    }
}

/// This vendor's `BackendVendor::advisor`.
pub static ADVISOR: PgAdvisor = PgAdvisor;

#[cfg(test)]
mod tests {
    use super::*;
    use zero_migrate_backend::advisory::rule;

    #[test]
    fn a_destructive_drop_is_analyzed_and_reported() {
        let AdvisoryVerdict::Analyzed(advisories) = ADVISOR.advise("DROP TABLE t") else {
            panic!("PostgreSQL ships an analyzer, so it must return the Analyzed arm");
        };
        assert!(
            advisories.iter().any(|a| a.rule == rule::DESTRUCTIVE_DROP),
            "a DROP TABLE must raise DESTRUCTIVE_DROP, got {advisories:?}"
        );
    }

    /// The two methods must agree, and the check is cheap enough to keep.
    #[test]
    fn the_analyzer_is_present_by_both_methods() {
        assert!(ADVISOR.analyzer_absence().is_none());
        assert!(matches!(
            ADVISOR.advise(""),
            AdvisoryVerdict::Analyzed(ref a) if a.is_empty()
        ));
    }

    #[test]
    fn index_coverage_reads_both_halves_of_the_fk_suppression() {
        let coverage = ADVISOR.index_coverage("CREATE INDEX i ON t (owner_id)");
        assert!(
            coverage
                .indexed_columns
                .iter()
                .any(|c| c.eq_ignore_ascii_case("owner_id")),
            "a CREATE INDEX must report its leading column, got {coverage:?}"
        );

        let coverage = ADVISOR.index_coverage(
            "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (owner_id) REFERENCES o(id)",
        );
        assert!(
            coverage
                .fk_columns_needing_index
                .iter()
                .any(|c| c.eq_ignore_ascii_case("owner_id")),
            "an ADD CONSTRAINT FOREIGN KEY must report its referencing column, got {coverage:?}"
        );
    }
}
