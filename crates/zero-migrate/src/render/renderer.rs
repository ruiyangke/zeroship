//! The backend CONTRACT — the future `zero-migrate-backend`.
//!
//! This module holds the vocabulary and the traits, and it deliberately holds no
//! SQL. Nothing here names a vendor, spells a keyword, or quotes an identifier;
//! the three shipping implementations live in [`crate::render::backends`], one
//! module per dialect, and the dispatch table lives there too.
//!
//! `renderer()` is re-exported from here so the crate's existing
//! `render::renderer::renderer(dialect)` call sites — in `lower`, `dml` and
//! `value_format` — keep resolving unchanged. The re-export is a compatibility
//! path, not a dependency on the backends' internals: this module's own code
//! cannot see them.
//!
//! # SPELLING vs SEMANTICS
//!
//! A method belongs on [`DmlRenderer`] when the answer is "how does this vendor
//! WRITE it" — `now()` vs `CURRENT_TIMESTAMP`, `bytea` vs `blob`. It does NOT
//! belong here when the answer is "what does this vendor MEAN by it": catalog
//! normalization, drift comparison and equivalence are core's decisions even
//! though they read the dialect, and they stay parameterized in core. See
//! `render::value_format` for the worked counter-example.
//!
//! # And a THIRD class that is neither: the capability tautology
//!
//! A method that emits no bytes and whose three impls differ only by their own
//! `DIALECT` const is not a vendor decision at all. `validate_view_materialized`
//! was one: every impl read
//! `DIALECT.supports(Capability::MaterializedView)` and built a CORE error type
//! from it, so resolving a renderer only to ask the vendor about ITSELF put a
//! dispatch between a question core could already answer — core holds the
//! dialect, and [`DialectSupports`] reads the same descriptor the vendor read.
//!
//! It now lives in `render::lower` as a plain dialect-parameterized fn. The
//! distinction matters for `docs/proposals/pluggable-backends.md` step 4 because
//! this class is DELETED rather than inverted: a backend crate never has to
//! export it, and core never has to reach a registry to run it. MEASURED at
//! `30ca3b06`: exactly ONE of this contract's methods was in the class, and
//! removing it changed no emitted byte — 1232 / 143 / 60 / 37 across `--lib`,
//! `authoring_surface`, `dialect_matrix` and `fold_offline`, unchanged, with the
//! control (an unconditional refusal in the moved fn) reddening 11 of them.
//!
//! It is NOT a free win for the cycle, and that is the part worth carrying
//! forward. Deleting the method removed two `renderer(dialect)` CALLS but zero
//! `renderer(dialect)` LOOKUPS: both sites bind the renderer for sibling spelling
//! methods on the next line, so `render::lower` holds the same seven lookups it
//! held before. The unit that blocks the crate split is the LOOKUP, not the call
//! site, and the two counts are not the same number.

use crate::model::expr::{CastTarget, ExtractField, ScalarFn};
use crate::model::ir::{IrScalar, Op, TableRef};
use crate::render::dml::DmlError;
use crate::render::lower::IrLowerError;
use crate::schema::query::SqlDialect;

/// The dialect feature predicates the migration lowerer asks.
///
/// PROMOTED to public vocabulary in `zero_migrate_ir::backend` — unchanged in
/// spirit and unchanged in membership (the same 25 predicates, the same
/// spellings). It is re-exported here so the ~250 in-crate `Capability::…` uses
/// keep naming it through `render::renderer`.
pub use zero_migrate_ir::backend::Capability;

/// The exhaustive dispatch over the shipping backends, re-exported from
/// [`crate::render::backends`] so existing `render::renderer::renderer(..)` call
/// sites resolve unchanged.
pub(crate) use crate::render::backends::renderer;

/// Ask a dialect a capability QUESTION.
///
/// The answer no longer lives in an exhaustive `match` on the vendor: it is read
/// off the vendor's [`BackendDescriptor`](zero_migrate_ir::backend::BackendDescriptor),
/// which is the whole point of promoting the matrix. A fourth backend answers by
/// declaring a descriptor in its own crate, not by editing an arm here.
pub(crate) trait DialectSupports {
    fn supports(self, cap: Capability) -> bool;
}

impl DialectSupports for SqlDialect {
    fn supports(self, cap: Capability) -> bool {
        self.descriptor().capabilities.contains(cap)
    }
}

/// Dialect-specific DML/view/trigger rendering.
///
/// No method has a default body: adding a dialect requires an explicit impl for
/// every render decision. The single exhaustive dispatch match lives in
/// [`crate::render::backends`], so a third [`SqlDialect`] variant breaks there at
/// compile time until its renderer is implemented and wired.
pub(crate) trait DmlRenderer {
    fn quote_ident(&self, ident: &str) -> String;
    fn qualify_table(&self, project_schema: &str, table: &str) -> Result<String, DmlError>;
    fn cast_target(&self, target: CastTarget) -> &'static str;

    /// The positional placeholder for the `n`-th (1-based) bind — `$n` / `?n` / `?`.
    fn placeholder(&self, n: usize) -> String;

    /// An inline SQL string literal that does not depend on the server's
    /// string-escape mode.
    fn inline_string_literal(&self, s: &str) -> String;

    /// An inline exact-decimal literal. A vendor that stores decimals as TEXT
    /// wants it quoted; the others want the digits verbatim.
    fn inline_decimal_literal(&self, d: &str) -> String;

    /// An inline binary literal, native on every vendor so a backfill or column
    /// default never coerces bytes through text.
    fn inline_bytes_literal(&self, bytes: &[u8]) -> String;

    /// The vendor's membership-test shape for an already-rendered `expr` against
    /// a homogeneous literal list. `joiner` is the caller's canonical separator.
    fn render_in_list(
        &self,
        expr: &str,
        elems: &[IrScalar],
        negated: bool,
        joiner: &str,
    ) -> Result<String, DmlError>;

    /// The vendor's regular-expression match operator, or a refusal if it has none.
    fn render_regex_match(&self, expr: &str, pattern: &str) -> Result<String, DmlError>;

    /// The vendor's spelling of a portable date-part extraction.
    fn render_extract(&self, field: ExtractField, expr: &str) -> String;

    /// The vendor's string-concatenation spelling for two rendered operands.
    fn render_concat(&self, l: &str, r: &str) -> String;

    /// The vendor's NULL-safe inequality spelling for two rendered operands.
    fn render_distinct_from(&self, l: &str, r: &str) -> String;

    /// A vendor-specific spelling for an allow-listed scalar call, or `None` to
    /// take the shared `<name>(<args>)` form. The override exists because the
    /// portable INTENT of a few scalars is not the vendor's native spelling.
    fn render_scalar_fn_override(&self, f: ScalarFn, args: &[String]) -> Option<String>;

    /// The vendor's `IS TRUE` predicate for an already-rendered operand.
    fn render_is_true(&self, operand: &str) -> String;

    /// The vendor's `IS FALSE` predicate for an already-rendered operand.
    fn render_is_false(&self, operand: &str) -> String;

    fn render_concat_ws(&self, rendered: &[String]) -> String;
    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError>;
    fn synth_now(&self) -> String;
    fn uuid_v4(&self) -> String;
    fn uuid_v7(&self) -> Result<String, DmlError>;
    fn view_create_prefix(&self, materialized: bool, replace: bool)
        -> Result<String, IrLowerError>;
    fn view_replace_prelude(&self, qname: &str, replace: bool) -> Vec<String>;
    fn view_object_name(&self, name: &str, eff_schema: &str) -> Result<String, IrLowerError>;
    fn render_table_ref(&self, table: &TableRef, eff_schema: &str) -> Result<String, IrLowerError>;
    fn render_trigger_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::render::vendor::VendorStatement>, IrLowerError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_capability_matrix_is_explicit() {
        // This matrix pins the feature surface for every shipping dialect, so a
        // descriptor that flips an answer fails HERE rather than in whichever
        // render path happened to read it.
        //
        // The exhaustiveness check at the bottom used to compare this table
        // against a hand-written `ALL_CAPABILITIES` list, which had drifted:
        // `MaterializedEnumType`, `MaterializedDomainType` and
        // `SchemaWideIndexNames` were in the enum and in the dispatch matrix but
        // in neither the pinned table nor the "all" list, so three predicates
        // times three dialects were unpinned and the completeness assertion
        // could not notice. It now compares against `Capability::ALL`, the one
        // vocabulary list, which cannot drift from the enum without the set's
        // own tests failing.
        let expected = [
            (
                SqlDialect::Postgres,
                [
                    (Capability::NonPkIdentity, true),
                    (Capability::VirtualGeneratedColumn, false),
                    (Capability::CrossSchemaDdl, true),
                    (Capability::TableLevelForeignKey, true),
                    (Capability::TableLevelUnique, true),
                    (Capability::NonBtreeIndexMethod, true),
                    (Capability::PartialIndexPredicate, true),
                    (Capability::NativeAlterColumn, true),
                    (Capability::AlterTableAddConstraint, true),
                    (Capability::AlterTableDropConstraint, true),
                    (Capability::AlterTableValidateConstraint, true),
                    (Capability::InsertOnConflictClause, true),
                    (Capability::PostgresVendorPrimitives, true),
                    (Capability::MaterializedView, true),
                    (Capability::CreateOrReplaceView, true),
                    (Capability::TriggerTruncateEvent, true),
                    (Capability::TriggerStatementForEach, true),
                    (Capability::TriggerExecuteFunction, true),
                    (Capability::TriggerBody, false),
                    (Capability::MaterializedEnumType, true),
                    (Capability::MaterializedDomainType, true),
                    (Capability::Sequence, true),
                    (Capability::ExclusionConstraint, true),
                    (Capability::CommentOn, true),
                    (Capability::SchemaWideIndexNames, true),
                ],
            ),
            (
                SqlDialect::Sqlite,
                [
                    (Capability::NonPkIdentity, false),
                    (Capability::VirtualGeneratedColumn, true),
                    (Capability::CrossSchemaDdl, false),
                    (Capability::TableLevelForeignKey, true),
                    (Capability::TableLevelUnique, false),
                    (Capability::NonBtreeIndexMethod, false),
                    (Capability::PartialIndexPredicate, true),
                    (Capability::NativeAlterColumn, false),
                    (Capability::AlterTableAddConstraint, false),
                    (Capability::AlterTableDropConstraint, false),
                    (Capability::AlterTableValidateConstraint, false),
                    (Capability::InsertOnConflictClause, true),
                    (Capability::PostgresVendorPrimitives, false),
                    (Capability::MaterializedView, false),
                    (Capability::CreateOrReplaceView, false),
                    (Capability::TriggerTruncateEvent, false),
                    (Capability::TriggerStatementForEach, false),
                    (Capability::TriggerExecuteFunction, false),
                    (Capability::TriggerBody, true),
                    (Capability::MaterializedEnumType, false),
                    (Capability::MaterializedDomainType, false),
                    (Capability::Sequence, false),
                    (Capability::ExclusionConstraint, false),
                    (Capability::CommentOn, false),
                    (Capability::SchemaWideIndexNames, true),
                ],
            ),
            (
                SqlDialect::Mysql,
                [
                    (Capability::NonPkIdentity, false),
                    (Capability::VirtualGeneratedColumn, true),
                    (Capability::CrossSchemaDdl, true),
                    (Capability::TableLevelForeignKey, true),
                    (Capability::TableLevelUnique, true),
                    (Capability::NonBtreeIndexMethod, false),
                    (Capability::PartialIndexPredicate, false),
                    (Capability::NativeAlterColumn, true),
                    (Capability::AlterTableAddConstraint, true),
                    (Capability::AlterTableDropConstraint, true),
                    (Capability::AlterTableValidateConstraint, false),
                    (Capability::InsertOnConflictClause, true),
                    (Capability::PostgresVendorPrimitives, false),
                    (Capability::MaterializedView, false),
                    (Capability::CreateOrReplaceView, true),
                    (Capability::TriggerTruncateEvent, false),
                    (Capability::TriggerStatementForEach, false),
                    (Capability::TriggerExecuteFunction, false),
                    (Capability::TriggerBody, true),
                    (Capability::MaterializedEnumType, false),
                    (Capability::MaterializedDomainType, false),
                    (Capability::Sequence, false),
                    (Capability::ExclusionConstraint, false),
                    (Capability::CommentOn, false),
                    (Capability::SchemaWideIndexNames, false),
                ],
            ),
        ];

        for (dialect, capabilities) in expected {
            for (cap, supported) in capabilities {
                assert_eq!(
                    dialect.supports(cap),
                    supported,
                    "{dialect:?} support for {cap:?}"
                );
                assert_eq!(
                    dialect.descriptor().capabilities.contains(cap),
                    supported,
                    "{dialect:?} descriptor answer for {cap:?}"
                );
            }
        }

        // Exhaustiveness against the ONE vocabulary list, not a second
        // hand-written copy of it.
        for (dialect, capabilities) in expected {
            let pinned: Vec<Capability> = capabilities.iter().map(|(cap, _)| *cap).collect();
            for cap in Capability::ALL {
                assert!(
                    pinned.contains(cap),
                    "{dialect:?}: {cap:?} is in the vocabulary but unpinned by this matrix"
                );
            }
            assert_eq!(
                pinned.len(),
                Capability::ALL.len(),
                "{dialect:?}: the pinned matrix must be exactly the vocabulary"
            );
        }
    }

    /// Every shipping descriptor must answer the whole vocabulary, and the
    /// answers must be a real per-dialect matrix rather than one shared row.
    #[test]
    fn every_shipping_descriptor_answers_the_whole_vocabulary() {
        let dialects = [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql];
        for cap in Capability::ALL {
            let answers: Vec<bool> = dialects.iter().map(|d| d.supports(*cap)).collect();
            assert_eq!(answers.len(), 3, "{cap:?} must be answered by all three");
        }
        // The three shipping capability sets are pairwise distinct; a bug that
        // pointed every descriptor at one set would otherwise pass silently.
        assert_ne!(
            SqlDialect::Postgres.descriptor().capabilities,
            SqlDialect::Sqlite.descriptor().capabilities
        );
        assert_ne!(
            SqlDialect::Postgres.descriptor().capabilities,
            SqlDialect::Mysql.descriptor().capabilities
        );
        assert_ne!(
            SqlDialect::Sqlite.descriptor().capabilities,
            SqlDialect::Mysql.descriptor().capabilities
        );
    }
}
