//! The backend CONTRACT — the future `zero-migrate-backend`.
//!
//! This module holds the vocabulary and the traits, and it deliberately holds no
//! SQL. Nothing here names a vendor, spells a keyword, or quotes an identifier;
//! the three shipping implementations live in `crate::render::backends`, one
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

use crate::dml::DmlError;
use crate::error::IrLowerError;
use crate::step::BindValue;
use zero_migrate_ir::dialect::{DialectId, SqlDialect};
use zero_migrate_ir::expr::{CastTarget, ExtractField, ScalarFn};
use zero_migrate_ir::ir::{IrScalar, Op, TableRef};

/// The dialect feature predicates the migration lowerer asks.
///
/// PROMOTED to public vocabulary in `zero_migrate_ir::backend` — unchanged in
/// spirit and unchanged in membership (the same 25 predicates, the same
/// spellings). It is re-exported here so the ~250 in-crate `Capability::…` uses
/// keep naming it through `render::renderer`.
pub use zero_migrate_ir::backend::Capability;

/// Ask a dialect a capability QUESTION.
///
/// The answer no longer lives in an exhaustive `match` on the vendor: it is read
/// off the vendor's [`BackendDescriptor`](zero_migrate_ir::backend::BackendDescriptor),
/// which is the whole point of promoting the matrix. A fourth backend answers by
/// declaring a descriptor in its own crate, not by editing an arm here.
pub trait DialectSupports {
    fn supports(self, cap: Capability) -> bool;
}

impl DialectSupports for SqlDialect {
    fn supports(self, cap: Capability) -> bool {
        self.descriptor().capabilities.contains(cap)
    }
}

/// Dialect-specific DML/view/trigger rendering.
///
/// No SPELLING method has a default body: adding a dialect requires an explicit
/// impl for every render decision. The single exhaustive dispatch match lives in
/// `crate::render::backends`, so a third `SqlDialect` variant breaks there at
/// compile time until its renderer is implemented and wired.
///
/// The two exceptions are [`dialect`](Self::dialect) and
/// [`supports`](Self::supports), and they are exceptions because neither is a
/// render decision: both are DERIVED from the one thing a backend does declare
/// about itself, its [`descriptor`](Self::descriptor). Giving them bodies here is
/// what stops a vendor from answering the identity question and the capability
/// question inconsistently — there is one source of truth per backend and the
/// trait reads it.
///
/// `Debug` is a SUPERTRAIT because the carriers that now hold a resolved
/// `&'static dyn DmlRenderer` ([`crate::dml::BindCtx`],
/// `crate::render::lower::IrAuthor`) are `#[derive(Debug)]` types, and a
/// carrier losing its `Debug` to gain a backend would be a worse trade than
/// asking each unit-struct renderer for the one derive it costs.
pub trait DmlRenderer: std::fmt::Debug + Sync {
    /// Which vendor this is.
    ///
    /// ADDED BY THE CRATE SPLIT, and it is the hinge the whole extraction turns on.
    /// The spelling helpers in [`crate::dml`] used to take a `dialect: SqlDialect`
    /// and resolve a renderer from it through a registry in the engine — which is
    /// exactly the edge that could not survive the split, because the registry has
    /// to be ABOVE the vendors and `dml` has to be BELOW them. They take a
    /// `&dyn DmlRenderer` now, and this method gives back the one thing the
    /// dialect parameter was still carrying: the capability and leg-selection
    /// questions the helpers ask of the dialect itself.
    ///
    /// It is NOT a second dialect literal in a vendor module. Each impl returns its
    /// module's existing `DIALECT` const, so the one-dialect-literal rule (and the
    /// test that enforces it) is unaffected.
    ///
    /// # Why the OPEN id and not the closed enum
    ///
    /// It returned [`SqlDialect`] until now, and that single return type was what
    /// stopped a fourth backend from lowering a migration. A vendor crate cannot
    /// produce a value of a closed enum it does not own, so the only body that
    /// type-checked outside this workspace's three vendors was `todo!()`: the crate
    /// compiled and panicked the first time anything asked it who it was. The
    /// registry was never the blocker — a stub backend registers and is reached
    /// through the real registry — this signature was.
    ///
    /// [`DialectId`] is `const`-constructible from a `&'static str`, so an outsider
    /// writes `DialectId::new("duckdb")` at item scope and answers honestly. It is
    /// deliberately NOT exhaustively matchable, which is why the leg selection and
    /// vendor gates below the trait compare against the canonical id CONSTANTS and
    /// carry an explicit fail-closed arm for an id they do not recognise, instead of
    /// a `match` the compiler would have completed for them.
    ///
    /// The direction stays one-way: an id does not convert back to a variant. See
    /// [`SqlDialect::id`].
    fn dialect(&self) -> DialectId {
        self.descriptor().id
    }

    /// What this backend IS: its id, its human-facing name, its capability set and
    /// its limits, all in one value the backend declares in its own crate.
    ///
    /// The renderer used to hand back an identity ([`dialect`](Self::dialect)) and
    /// core turned that identity into capabilities through
    /// `SqlDialect::descriptor` — an exhaustive match in `zero-migrate-ir`, i.e. a
    /// table core owns about vendors core does not. That is the same closed-set
    /// problem the identity had, one level up: an outsider's id has no arm in that
    /// match, so the honest answer for it was "no capabilities at all", and a
    /// backend that answers NO to everything cannot render anything.
    ///
    /// Asking the VENDOR instead removes the table. A backend crate declares one
    /// `BackendDescriptor` const and returns it here; core reads capabilities off
    /// the value rather than deriving them from a name it recognises.
    fn descriptor(&self) -> &'static zero_migrate_ir::backend::BackendDescriptor;

    /// Ask THIS backend a capability question.
    ///
    /// The [`DialectSupports`] shape one line at a time: `supports` never branched
    /// on the vendor, it read the vendor's descriptor, so it belongs on the vendor.
    fn supports(&self, cap: Capability) -> bool {
        self.descriptor().capabilities.contains(cap)
    }

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

    /// BIND a binary value, and return the SQL fragment that reconstitutes it at
    /// the placeholder site. `push` appends ONE bind and hands back its
    /// placeholder.
    ///
    /// The bound counterpart of [`inline_bytes_literal`](Self::inline_bytes_literal),
    /// and the vendor's to answer for the same reason: the vendors disagree about
    /// the CARRIER, not about the value. SQLite's driver takes the byte vector
    /// straight through, so the fragment is the bare placeholder; PostgreSQL's
    /// schema-blind DML seam and mysql2 both take canonical base64 as TEXT and
    /// decode it inside the statement, in each vendor's own spelling.
    ///
    /// This was a three-way `match` on `SqlDialect` inside `BindCtx::push_scalar`
    /// — a spelling decision made in core, which is the shape this module exists
    /// to hold instead.
    fn bind_bytes(&self, bytes: &[u8], push: &mut dyn FnMut(BindValue) -> String) -> String;

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
    ) -> Result<Vec<crate::vendor::VendorStatement>, IrLowerError>;

    /// This vendor's rendering of the PRIVILEGED vendor ops — schemas, extensions,
    /// roles, grants, RLS, policies, functions and the raw escape.
    ///
    /// # Why this is on the trait, and what it replaced
    ///
    /// The engine used to reach PostgreSQL's renderer BY NAME:
    /// `zero_migrate::render::vendor` re-exported `zero_migrate_postgres::render_vendor_op`
    /// and `render::lower` called it at three sites covering sixteen op kinds. Those
    /// op kinds never touch [`DmlRenderer::render_trigger_op`], which is exactly why
    /// they were left behind when the two renderers went behind the contract, and
    /// `render/vendor.rs` recorded the gap honestly rather than hiding it. This
    /// method closes it: the vendor-op surface is now reached the same way every
    /// other spelling decision is, through the registry.
    ///
    /// # Two vendors REFUSE, and they refuse in writing
    ///
    /// Measured rather than assumed: `zero-migrate-sqlite` and `zero-migrate-mysql`
    /// contain no vendor-op renderer and never did, every one of the sixteen op kinds
    /// is `dialect_scope = PgOnly`, and the engine's lower seam refuses a target
    /// without `Capability::PostgresVendorPrimitives` before it ever gets here.
    ///
    /// So the obvious shape was an `Option` or a default body returning a refusal —
    /// and it is the wrong one, for the reason
    /// [`crate::registry::BackendVendor::guard`] spells out at length. A default body
    /// is an answer a future backend acquires by OMITTING something. This method has
    /// none, like every other method on this trait, so a fourth vendor has to answer
    /// the question in its own crate and in its own diff. The two refusals cost five
    /// lines each and each one names its own dialect through its `DIALECT` const.
    ///
    /// # Errors
    /// [`crate::vendor::VendorError`] on an invalid identifier, an unrenderable
    /// predicate or an empty required list; and
    /// [`crate::vendor::VendorError::VendorOpsUnsupported`] from a vendor that renders
    /// no vendor ops at all.
    fn render_vendor_op(
        &self,
        op: &Op,
        eff_schema: &str,
    ) -> Result<Vec<crate::vendor::VendorStatement>, crate::vendor::VendorError>;
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
                    (Capability::TransactionalDdl, true),
                    (Capability::DeferrableConstraint, true),
                    (Capability::UniqueConstraintDistinctFromIndex, true),
                    (Capability::IntegerPrimaryKeyRowidAlias, false),
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
                    (Capability::TransactionalDdl, true),
                    (Capability::DeferrableConstraint, true),
                    (Capability::UniqueConstraintDistinctFromIndex, true),
                    (Capability::IntegerPrimaryKeyRowidAlias, true),
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
                    (Capability::TransactionalDdl, false),
                    (Capability::DeferrableConstraint, false),
                    (Capability::UniqueConstraintDistinctFromIndex, false),
                    (Capability::IntegerPrimaryKeyRowidAlias, false),
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
