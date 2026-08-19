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

use crate::model::expr::CastTarget;
use crate::model::ir::{Op, TableRef};
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
    fn render_concat_ws(&self, rendered: &[String]) -> String;
    fn render_split_part(&self, col_sql: &str, delim: &str, n: i64) -> Result<String, DmlError>;
    fn synth_now(&self) -> String;
    fn uuid_v4(&self) -> String;
    fn uuid_v7(&self) -> Result<String, DmlError>;
    fn validate_view_materialized(&self, materialized: bool) -> Result<(), IrLowerError>;
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
