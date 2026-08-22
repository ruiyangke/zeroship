//! The REGISTRY of shipping backends — the engine's one list of which vendors exist.
//!
//! This module used to hold the vendors themselves, as three sibling modules with a
//! hard-coded match over a closed dialect enum beneath them. `docs/proposals/pluggable-backends.md`
//! step 4 has now happened: they are `zero-migrate-postgres`, `zero-migrate-sqlite`
//! and `zero-migrate-mysql`, the contract they implement is `zero-migrate-backend`,
//! and what is left here is the composition.
//!
//! | before                        | after                        |
//! |-------------------------------|------------------------------|
//! | `render::renderer`            | `zero_migrate_backend::renderer` |
//! | `render::backends::postgres`  | `zero-migrate-postgres`      |
//! | `render::backends::sqlite`    | `zero-migrate-sqlite`        |
//! | `render::backends::mysql`     | `zero-migrate-mysql`         |
//! | `render::backends` (this)     | the registry composition     |
//!
//! # What belongs in a backend crate
//!
//! **SPELLING only** — the bytes this vendor wants to read. `"now()"` vs
//! `"CURRENT_TIMESTAMP"`, `"bytea"` vs `"blob"`, `` `x` `` vs `"x"`,
//! `OR REPLACE VIEW` vs a `DROP VIEW IF EXISTS` prelude.
//!
//! **COMPARISON stays in the engine; vendor FACTS do not.** Core still decides whether
//! two defaults or checks are equivalent, but `ValueFormatRenderer` supplies each
//! backend's catalog decorations, aliases, storage spellings, and format DDL. The
//! comparison is one algorithm without a vendor match; the facts live with their
//! vendors.
//!
//! The test is the DIRECTION of the arrow. Spelling is the engine ASKING a vendor how
//! to write something. Semantics is the engine DECIDING something about a vendor.
//! That rule is what kept the extraction from dragging `render::lower`,
//! `render::declarative`, `render::fold` and `schema::query` out with it.
//!
//! # The one-dialect-literal rule, now across a crate boundary
//!
//! A backend module names its own dialect exactly ONCE, as its `DIALECT` const, and
//! names no other dialect at all. Everything else reads `DIALECT`. That rule is what
//! made this step mechanical, and it is still ENFORCED:
//! `tests/dialect_matrix/backend_modules_name_one_dialect.rs` reads all nine vendor
//! modules with `include_str!` and asserts both halves — own dialect exactly once, as
//! the `const DIALECT` line, and no other dialect at all. It was repointed at the new
//! crate paths in the commit that moved them; a re-export shim at the old path would
//! NOT have satisfied it, because a shim has zero carriers.
//!
//! # The rule does NOT catch implicit coupling, and there was some
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper that
//! hard-codes a dialect, and the grep above cannot see it because the literal lives
//! in `zero-migrate-backend`. That was not hypothetical here: `dml::quote_ident`,
//! `dml::quote_bare_ident` and `dml::quote_ident_checked_for_dialect` all pinned
//! the PostgreSQL enum leg, so all four identifier emissions in the SQLite backend
//! used to be quoted by the POSTGRESQL renderer. It was correct only because both
//! vendors spell an identifier `"x"`.
//!
//! MEASURED, not reasoned, BEFORE the fix: corrupting `PostgresDmlRenderer::quote_ident`
//! alone failed 7 of the 155 tests in the SQLite-ONLY `sqlite_engine` binary, all
//! against a real SQLite database. RESOLVED: every identifier emission in the SQLite
//! and PostgreSQL backends goes through the `*_for_dialect(.., DIALECT)` seam, and
//! re-running the identical neuter afterwards leaves `sqlite_engine` at 155 / 0 — the
//! same 155 tests, so the SQLite backend stopped reading the PostgreSQL renderer
//! without any emitted byte changing.
//!
//! # The OTHER half of the class: emission that reaches no renderer at all
//!
//! The larger instance was the engine reaching NO renderer: `dml::escape_quote_ident`
//! was a `pub(crate)` raw `format!` that any module could call to spell `"x"` without
//! naming a dialect. Correct bytes, absent routing, invisible to every behaviour test
//! for the same reason as above.
//!
//! RESOLVED by VISIBILITY, and the crate split WEAKENED that fix. The primitive is
//! now `zero_migrate_backend::spelling::ansi_double_quote_ident`, and across a crate
//! boundary `pub(in …)` cannot say "these three crates and no other" — the vendor
//! crates must reach it, so it is `pub`, so the engine can name it too. The compiler
//! no longer enforces the rule. It is replaced by a textual census,
//! `tests/dialect_matrix/core_does_not_spell_a_vendors_bytes.rs`, which walks every
//! crate `src` root and asserts the engine names neither spelling primitive. That is
//! strictly weaker than a privacy error and it is recorded as a downgrade, not as an
//! equal substitute.
//!
//! And a DELIBERATE non-defect that looks identical to a neuter: the
//! `pg_get_constraintdef` normal form (`declarative::quote_ident_if_needed` /
//! `constraintdef_cols`) is PostgreSQL-spelled ON PURPOSE and is read by the SQLite
//! and MySQL drift comparators. It has a renderer-independent snapshot codec,
//! precisely because a red count cannot tell it apart from an unrouted emission.
//! Re-dialecting it would be a regression.

use zero_migrate_backend::advisory::{
    AdvisoryVerdict, AnalyzerAbsent, IndexCoverage, OperationalAdvisor,
};
use zero_migrate_backend::ddl::DdlEmitter;
use zero_migrate_backend::guard::{GuardConfig, MigrationGuard};
use zero_migrate_backend::registry::{BackendVendor, VendorSet};
use zero_migrate_backend::renderer::DmlRenderer;
use zero_migrate_ir::dialect::DialectId;

// This vendor-owned policy remains part of the engine's neutral plan carrier. Re-export
// it from the one composition module so core never reaches into a vendor crate directly.
pub(crate) use zero_migrate_sqlite::{SqliteSequencePolicy, VENDOR};

#[cfg(test)]
use zero_migrate_ir::dialect::{MYSQL, POSTGRES, SQLITE};

/// The shipping backends, named ONCE for the whole engine.
///
/// This is the registry composition that replaced the hard-coded three-arm `match`.
/// The difference is not cosmetic: the `match` named three statics in the engine's
/// own crate, which is precisely why the vendors could not leave it. A fourth backend
/// is now a `[dependencies]` line plus an entry here, with no edit to the contract
/// crate and no edit to any other vendor.
///
/// The set is still a compile-time constant rather than a global the host fills at
/// startup, and that is deliberate — see `zero_migrate_backend::registry` for why a
/// growable registry would trade a compile error for a runtime one.
const POSTGRES_VENDOR: &BackendVendor = &zero_migrate_postgres::VENDOR;
const SQLITE_VENDOR: &BackendVendor = &VENDOR;
const MYSQL_VENDOR: &BackendVendor = &zero_migrate_mysql::VENDOR;

static SHIPPING: [&BackendVendor; 3] = [POSTGRES_VENDOR, SQLITE_VENDOR, MYSQL_VENDOR];

pub(crate) const VENDORS: VendorSet = VendorSet::new(&SHIPPING);

/// The engine's stable generated-identifier byte budget, read from the registered
/// backend that imposes the limiting byte-counted cap. Generated names are
/// precomputed once and then handed to every emitter, so this is one composition
/// fact rather than a per-call vendor dispatch.
///
/// This is the ONE definition. It used to be three - `plan::author`, `apply::role`
/// and `render::expand_contract` each restated `63`, and `plan::author`'s doc
/// promised the author's number and the backend's DECLARED number "cannot drift
/// apart", which was true of one site and false of the other two. Reading the
/// declared limit here makes that promise true of all of them.
pub(crate) const GENERATED_IDENT_MAX_BYTES: usize =
    match POSTGRES_VENDOR.descriptor.limits.identifier {
        zero_migrate_ir::backend::IdentifierLimit::Bytes(n) => n,
        zero_migrate_ir::backend::IdentifierLimit::Unbounded
        | zero_migrate_ir::backend::IdentifierLimit::Characters(_) => {
            panic!("PostgreSQL declares a BYTE identifier cap")
        }
    };

/// The vendor for a dialect.
///
/// Resolved by the open [`DialectId`](zero_migrate_ir::dialect::DialectId) filed in
/// each [`BackendVendor`], never by an enum match in core. The shipping list above is
/// the one composition point that names backend crates.
pub(crate) fn vendor(dialect: &DialectId) -> &'static BackendVendor {
    VENDORS
        .get(dialect)
        .unwrap_or_else(|| panic!("no registered backend vendor for {dialect}"))
}

/// The DML renderer for a dialect.
pub(crate) fn renderer(dialect: &DialectId) -> &'static dyn DmlRenderer {
    vendor(dialect).dml
}

/// The schema renderer for a dialect. Re-exported as `schema::query::renderer`.
pub(crate) fn schema_renderer(
    dialect: &DialectId,
) -> &'static dyn zero_migrate_backend::schema::SchemaRenderer {
    vendor(dialect).schema
}

/// The value-format renderer and catalog normalizer registered by a dialect's vendor.
pub(crate) fn value_format_renderer(
    dialect: &DialectId,
) -> &'static dyn zero_migrate_backend::value_format::ValueFormatRenderer {
    vendor(dialect).value_format
}

/// Every shipping value-format renderer, used only when a legacy snapshot has
/// no backend provenance and the neutral comparator must compose the vendors'
/// explicitly declared normalization rules.
pub(crate) fn value_format_renderers(
) -> impl Iterator<Item = &'static dyn zero_migrate_backend::value_format::ValueFormatRenderer> {
    VENDORS.as_slice().iter().map(|vendor| vendor.value_format)
}

/// The vendor-owned parser for catalog-stored table DDL, when this backend
/// explicitly provides one.
pub(crate) fn stored_ddl(
    dialect: &DialectId,
) -> Option<&'static dyn zero_migrate_backend::stored_ddl::StoredDdl> {
    vendor(dialect).schema.stored_ddl()
}

/// The schema-bound DDL emitter registered by a dialect's vendor crate.
pub(crate) fn ddl_emitter(dialect: &DialectId, project_schema: &str) -> Box<dyn DdlEmitter> {
    (vendor(dialect).ddl)(project_schema)
}

/// The LINE-1 guard for a config's dialect — this vendor's, built by this vendor.
///
/// This replaced `zero_migrate_guard::guard::guard_for`, which was a second
/// closed identity match living in the guard crate and mapping BOTH
/// descriptor-only dialects onto one shared `SqliteDescriptorGuard`. Two consequences
/// of folding it into the vendor registry are worth stating:
///
/// - Every backend surface now resolves through the same [`VendorSet`] lookup, keyed
///   by the vendor's open id; there is no enum dispatch here for a fourth backend to
///   be omitted from.
/// - "This vendor ships no guard" became a compile error at the vendor's own
///   definition site rather than something a `_ =>` arm here could paper over. See
///   `zero_migrate_backend::registry::BackendVendor`.
///
/// SQLite and MySQL no longer share a guard TYPE either; each writes its own trusting
/// impl, so a change to one dialect's posture cannot silently become a change to the
/// other's.
pub(crate) fn guard_for(cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    (vendor(cfg.dialect()).guard)(cfg)
}

/// The OPERATIONAL analyzer for a dialect — this vendor's, run by this vendor.
///
/// The advisory counterpart of [`guard_for`], and it removed the same shape of
/// coupling. Before it, `render::declarative` called `crate::analysis::analyze::…`
/// directly: a re-export that resolved into the `libpg_query` analyzer crate, so the
/// engine ran the PostgreSQL parser over every backend's DDL and reported the
/// resulting parse failures as a clean, empty advisory list. That was the whole of
/// the engine's advisory routing, and it named a parser rather than a vendor.
pub(crate) fn advisor(dialect: &DialectId) -> &'static dyn OperationalAdvisor {
    vendor(dialect).advisor
}

/// The operational advisories the backend registered for `dialect` finds in `sql`.
///
/// The verdict, not a list: see
/// [`AdvisoryVerdict`](zero_migrate_backend::advisory::AdvisoryVerdict) for why a
/// backend with no analyzer must not answer with an empty vector.
#[must_use]
pub fn advisories_for_sql(dialect: &DialectId, sql: &str) -> AdvisoryVerdict {
    advisor(dialect).advise(sql)
}

/// Whether the backend registered for `dialect` ships an operational analyzer at
/// all, answered without SQL.
///
/// A caller reporting on a SET of statements asks this ONCE, up front, so it can
/// tell an operator the whole set is unchecked before it renders the first statement
/// — and still say so when the set renders to nothing.
#[must_use]
pub fn analyzer_absence(dialect: &DialectId) -> Option<AnalyzerAbsent> {
    advisor(dialect).analyzer_absence()
}

/// The index-coverage facts `dialect`'s backend reads out of `sql`, for the
/// plan-wide `FK_WITHOUT_INDEX` suppression.
pub(crate) fn index_coverage(dialect: &DialectId, sql: &str) -> IndexCoverage {
    advisor(dialect).index_coverage(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_dml_renderer() {
        assert_eq!(renderer(&POSTGRES).synth_now(), "now()");
        assert_eq!(renderer(&SQLITE).synth_now(), "CURRENT_TIMESTAMP");
        assert_eq!(renderer(&MYSQL).synth_now(), "CURRENT_TIMESTAMP(6)");
    }

    /// The registry composes, and it composes through the LEAF crate's builder
    /// rather than through a rule restated here.
    ///
    /// `BackendRegistry::build` is what refuses a malformed or duplicate id, naming
    /// both registrants on a collision. Asserting that the shipping set clears it is
    /// what makes "three vendors, three distinct ids" a checked fact rather than an
    /// arrangement that happens to hold.
    #[test]
    fn the_shipping_vendor_set_composes_into_a_registry() {
        let registry = VENDORS
            .descriptors()
            .expect("the shipping vendors must satisfy the dialect-id rule");
        assert_eq!(registry.len(), VENDORS.len());
        assert_eq!(registry.len(), 3);
        for dialect in [POSTGRES, SQLITE, MYSQL] {
            assert!(
                registry.get(&dialect).is_some(),
                "{dialect} must be registered"
            );
        }
    }

    /// Each vendor's identity-bearing renderers agree with the descriptor they are filed under.
    ///
    /// A `BackendVendor` is a hand-written struct literal in each vendor crate, so
    /// nothing but this stops a crate from pairing PostgreSQL's descriptor with
    /// SQLite's renderer. The engine would then spell one vendor's SQL under
    /// another's capability answers, which no emitted-SQL assertion could see for the
    /// two vendors that agree on identifier quoting.
    #[test]
    fn every_vendor_agrees_with_its_own_descriptor() {
        for v in VENDORS.as_slice() {
            assert_eq!(
                v.schema.dialect(),
                v.descriptor.id,
                "{} registered a SchemaRenderer for a different dialect",
                v.descriptor.display_name
            );
            assert_eq!(
                v.value_format.dialect(),
                v.descriptor.id,
                "{} registered a ValueFormatRenderer for a different dialect",
                v.descriptor.display_name
            );
            // The DmlRenderer answers through its descriptor now rather than a
            // literal of its own, so this half checks the vendor filed the SAME
            // descriptor in both places: one that returned SQLite's descriptor
            // while registering PostgreSQL's would spell one vendor's SQL under
            // the other's capability answers.
            assert_eq!(
                v.dml.descriptor(),
                v.descriptor,
                "{} registered a DmlRenderer carrying a different descriptor",
                v.descriptor.display_name
            );
            let ddl = (v.ddl)("registry_identity_probe");
            assert_eq!(
                ddl.dialect(),
                v.descriptor.id,
                "{} registered a DdlEmitter for a different dialect",
                v.descriptor.display_name
            );
        }
    }

    /// SQLite cannot reach a deferred `ALTER TABLE … ADD CONSTRAINT` through the
    /// current capability gate, but the old core router still selected the
    /// PostgreSQL FK-clause body for that arm. Moving the body must preserve that
    /// dormant answer too: unreachable is not permission to reimplement it.
    #[test]
    fn sqlite_deferred_fk_clause_keeps_the_former_postgres_bytes() {
        let fk = zero_migrate_backend::snapshot::ConstraintSnapshot {
            name: "fk\"child".to_string(),
            kind: "FOREIGN KEY".to_string(),
            definition: "FOREIGN KEY (\"child\"\"col\") REFERENCES old.parents(id, \"parent\"\"col\") ON DELETE CASCADE".to_string(),
            comment: None,
            cascade_columns: None,
        };
        let expected = "CONSTRAINT \"fk\"\"child\" FOREIGN KEY (\"child\"\"col\") REFERENCES \"project\"\"schema\".\"parents\" (id, \"parent\"\"col\") ON DELETE CASCADE";

        let sqlite = ddl_emitter(&SQLITE, "project\"schema").fk_clause(&fk);
        let postgres = ddl_emitter(&POSTGRES, "project\"schema").fk_clause(&fk);
        assert_eq!(sqlite, expected);
        assert_eq!(sqlite, postgres);
    }
}
