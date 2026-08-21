//! The REGISTRY of shipping backends — the engine's one list of which vendors exist.
//!
//! This module used to hold the vendors themselves, as three sibling modules with a
//! hard-coded `match` over `SqlDialect` beneath them. `docs/proposals/pluggable-backends.md`
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
//! **SEMANTICS stays in the engine, dialect-PARAMETERIZED.** Comparison and
//! normalization ("does this catalog default MEAN the same as that one", "is this
//! authored width equivalent to that reported width") are the engine's job even when
//! the answer depends on the dialect. `render::value_format` is the worked example:
//! its dialect branches are drift-comparison rules, and moving them into a vendor
//! crate would scatter one comparison across three vendors.
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
//! `tests/dialect_matrix/backend_modules_name_one_dialect.rs` reads all six vendor
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
//! `dml::quote_bare_ident` and `dml::quote_ident_checked` all pinned
//! `SqlDialect::Postgres`, so all four identifier emissions in the SQLite backend
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
//! and MySQL drift comparators. It has its own door, `dml::pg_canonical_ident`,
//! precisely because a red count cannot tell it apart from an unrouted emission.
//! Re-dialecting it would be a regression.

use zero_migrate_backend::guard::{GuardConfig, MigrationGuard};
use zero_migrate_backend::registry::{BackendVendor, VendorSet};
use zero_migrate_backend::renderer::DmlRenderer;

use crate::schema::query::SqlDialect;

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
static SHIPPING: [&BackendVendor; 3] = [
    &zero_migrate_postgres::VENDOR,
    &zero_migrate_sqlite::VENDOR,
    &zero_migrate_mysql::VENDOR,
];

pub(crate) const VENDORS: VendorSet = VendorSet::new(&SHIPPING);

/// The vendor for a dialect.
///
/// Still EXHAUSTIVE over the closed `SqlDialect`, so a fourth variant breaks here at
/// compile time until its crate is wired — the property the old `match` had and the
/// one worth keeping. What changed is that the arms now name a CRATE's registered
/// vendor rather than a module-private static, so the vendor is deletable.
fn vendor(dialect: SqlDialect) -> &'static BackendVendor {
    match dialect {
        SqlDialect::Postgres => &zero_migrate_postgres::VENDOR,
        SqlDialect::Sqlite => &zero_migrate_sqlite::VENDOR,
        SqlDialect::Mysql => &zero_migrate_mysql::VENDOR,
    }
}

/// The DML renderer for a dialect.
pub(crate) fn renderer(dialect: SqlDialect) -> &'static dyn DmlRenderer {
    vendor(dialect).dml
}

/// The schema renderer for a dialect. Re-exported as `schema::query::renderer`.
pub(crate) fn schema_renderer(
    dialect: SqlDialect,
) -> &'static dyn zero_migrate_backend::schema::SchemaRenderer {
    vendor(dialect).schema
}

/// The LINE-1 guard for a config's dialect — this vendor's, built by this vendor.
///
/// This replaced `zero_migrate_guard::guard::guard_for`, which was a second
/// `match` over `SqlDialect` living in the guard crate and mapping BOTH
/// descriptor-only dialects onto one shared `SqliteDescriptorGuard`. Two consequences
/// of folding it into the vendor registry are worth stating:
///
/// - There is now exactly ONE exhaustive `SqlDialect` match for backend selection in
///   the engine — [`vendor`] — instead of two that could disagree. A fourth dialect
///   breaks it in one place.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_dml_renderer() {
        assert_eq!(renderer(SqlDialect::Postgres).synth_now(), "now()");
        assert_eq!(
            renderer(SqlDialect::Sqlite).synth_now(),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(
            renderer(SqlDialect::Mysql).synth_now(),
            "CURRENT_TIMESTAMP(6)"
        );
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
        for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite, SqlDialect::Mysql] {
            assert!(
                registry.get(&dialect.id()).is_some(),
                "{dialect:?} must be registered"
            );
        }
    }

    /// Each vendor's two renderers agree with the descriptor they are filed under.
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
        }
    }
}
