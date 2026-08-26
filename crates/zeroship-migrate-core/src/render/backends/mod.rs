//! The REGISTRY of shipping backends - the engine's one list of which vendors exist.
//!
//! This module used to hold the vendors themselves, as three sibling modules with a
//! hard-coded match over a closed dialect enum beneath them. The split
//! `docs/proposals/pluggable-backends.md` describes has happened: they are
//! `zeroship-migrate-postgres`, `zeroship-migrate-sqlite`
//! and `zeroship-migrate-mysql`, the contract they implement is `zeroship-migrate-backend`,
//! and what is left here is the composition.
//!
//! | before                        | after                        |
//! |-------------------------------|------------------------------|
//! | `render::renderer`            | `zeroship_migrate_backend::renderer` |
//! | `render::backends::postgres`  | `zeroship-migrate-postgres`      |
//! | `render::backends::sqlite`    | `zeroship-migrate-sqlite`        |
//! | `render::backends::mysql`     | `zeroship-migrate-mysql`         |
//! | `render::backends` (this)     | the registry composition     |
//!
//! # What belongs in a backend crate
//!
//! **SPELLING only** - the bytes this vendor wants to read. `"now()"` vs
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
//! modules with `include_str!` and asserts both halves - own dialect exactly once, as
//! the `const DIALECT` line, and no other dialect at all. It was repointed at the new
//! crate paths in the commit that moved them; a re-export shim at the old path would
//! NOT have satisfied it, because a shim has zero carriers.
//!
//! # The rule does NOT catch implicit coupling, and there was some
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper that
//! hard-codes a dialect, and the grep above cannot see it because the literal lives
//! in `zeroship-migrate-backend`. That was not hypothetical here: `dml::quote_ident`,
//! `dml::quote_bare_ident` and `dml::quote_ident_checked_for_dialect` all pinned
//! the PostgreSQL enum leg, so all four identifier emissions in the SQLite backend
//! used to be quoted by the POSTGRESQL renderer. It was correct only because both
//! vendors spell an identifier `"x"`.
//!
//! MEASURED, not reasoned, BEFORE the fix: corrupting `PostgresDmlRenderer::quote_ident`
//! alone reddened tests in the SQLite-ONLY `sqlite_engine` binary, all against a real
//! SQLite database. RESOLVED: every identifier emission in the SQLite and PostgreSQL
//! backends goes through the `*_for_dialect(.., DIALECT)` seam, and re-running the
//! identical neuter afterwards leaves `sqlite_engine` fully green over the same tests,
//! so the SQLite backend stopped reading the PostgreSQL renderer without any emitted
//! byte changing.
//!
//! # The OTHER half of the class: emission that reaches no renderer at all
//!
//! The larger instance was the engine reaching NO renderer: `render::dml` held a
//! `pub(crate)` raw `format!` that any module could call to spell `"x"` without
//! naming a dialect. Correct bytes, absent routing, invisible to every behaviour test
//! for the same reason as above.
//!
//! RESOLVED by VISIBILITY, and the crate split WEAKENED that fix. The primitive is
//! now `zeroship_migrate_backend::spelling::ansi_double_quote_ident`, and across a crate
//! boundary `pub(in ...)` cannot say "these three crates and no other" - the vendor
//! crates must reach it, so it is `pub`, so the engine can name it too. The compiler
//! no longer enforces the rule. It is replaced by a textual census,
//! `tests/dialect_matrix/core_does_not_spell_a_vendors_bytes.rs`, which walks every
//! crate `src` root and asserts the engine names neither spelling primitive. That is
//! strictly weaker than a privacy error and it is recorded as a downgrade, not as an
//! equal substitute.
//!
//! And a DELIBERATE non-defect that looks identical to a neuter: the
//! `pg_get_constraintdef` normal form (`zeroship_migrate_backend::constraint_definition`
//! - `quote_ident_if_needed` / `constraintdef_cols`, re-exported at their historical
//! `render::declarative::...` paths) is PostgreSQL-spelled ON PURPOSE and is read by
//! the SQLite and MySQL drift comparators. It has a renderer-independent snapshot
//! codec, precisely because a red count cannot tell it apart from an unrouted
//! emission. Re-dialecting it would be a regression.
//!
//! It sits BELOW the vendors rather than here because MySQL's drift path has to
//! build that form itself, and it is now `pub` across a crate boundary rather than
//! `pub(crate)`. That widening is what
//! `tests/dialect_matrix/constraint_definition_is_comparison_text.rs` stands in for:
//! a vendor may READ the codec to normalize what it introspected, but its `ddl.rs` /
//! `dml.rs` may not spell an EMITTED identifier with it. On PostgreSQL and SQLite
//! the wrong call emits correct bytes, so only a census can see it.

use zeroship_migrate_backend::advisory::{
    AdvisoryVerdict, AnalyzerAbsent, IndexCoverage, OperationalAdvisor,
};
use zeroship_migrate_backend::ddl::DdlEmitter;
use zeroship_migrate_backend::guard::{GuardConfig, MigrationGuard};
use zeroship_migrate_backend::registry::{BackendVendor, VendorSet};
use zeroship_migrate_backend::renderer::DmlRenderer;
use zeroship_migrate_ir::dialect::DialectId;

// ---------------------------------------------------------------------------
// THE COMPOSITION USED TO LIVE HERE, AND IT IS THE REASON THIS CRATE EXISTS
// ---------------------------------------------------------------------------
//
// Three `const`s naming `zeroship_migrate_postgres::VENDOR`, `zeroship_migrate_sqlite::VENDOR`
// and `zeroship_migrate_mysql::VENDOR`, a `static SHIPPING` array over them, and a
// `pub(crate) const VENDORS: VendorSet` folded from that array. Those three lines were
// the whole of why this crate had to depend on all three backends.
//
// They are `crates/zeroship-migrate/src/lib.rs` now - the COMPOSITION ROOT, which depends
// on this crate rather than the other way round. `zeroship-migrate-core` declares no
// vendor `[dependencies]` at all, so writing one of those idents in this crate's
// production source is an unresolved-crate error. The rule *the core should be
// neutral* stopped being something a census asserts about source text and became
// something the build cannot express.
//
// The resolvers below did NOT move, and that is the other half of the design: the
// engine RESOLVES, and it resolves against the set its caller hands it. Every one of
// them takes `vendors: VendorSet` as its first argument, and every caller of one takes
// it from its own caller - as a parameter, or as a field on a struct that already
// carries the dialect it is about to resolve. `IrAuthor`, `DeclarativeAuthor`,
// `ExpandContractAuthor`, `CatalogFold`, `MigrationEngine` and the raw/deterministic
// authors all carry it, so their methods pay no signature for it at all.
//
// `zeroship_migrate::shipping_vendors()` is how a host asks for the composed value, and
// `tests/dialect_matrix/the_registry_travels_as_a_value.rs` is what keeps the list of
// places that may name it from growing back - a rule Cargo now enforces for this crate
// but not for the `#[cfg(test)]` modules that reach the vendors through dev edges.
//
// (Earlier notes here recorded a `pub(crate) use zeroship_migrate_sqlite::VENDOR;` that
// made one registry entry asymmetric with the other two, and a `SqliteSequencePolicy`
// re-export riding the same line. Both were closed before the split; nothing of either
// is left to move.)

#[cfg(test)]
use crate::test_fixtures::{MYSQL, POSTGRES, SQLITE};

/// The engine's generated-identifier byte budget: the TIGHTEST cap any backend in
/// `vendors` declares. Generated names are precomputed once and then handed to every
/// emitter, so this is one fact about the whole registry rather than a per-call vendor
/// dispatch - which means it has to fit the strictest target in the build, not a
/// chosen one.
///
/// This is the ONE definition. It used to be three - `plan::author`, `apply::role`
/// and `render::expand_contract` each restated `63`, and `plan::author`'s doc
/// promised the author's number and the backend's DECLARED number "cannot drift
/// apart", which was true of one site and false of the other two. Reading the
/// declared limit here makes that promise true of all of them.
///
/// # It used to read ONE vendor's cap while claiming to read the limiting one
///
/// The body was `match POSTGRES_VENDOR.descriptor.limits.identifier`, with a
/// `panic!("PostgreSQL declares a BYTE identifier cap")` on the other two arms. The
/// doc directly above it already said "the registered backend that imposes the
/// limiting byte-counted cap" - so the sentence was a description of the intended
/// fold and the code was a hard-coded lookup of one vendor. The two agree by
/// accident today: PostgreSQL declares `Bytes(63)`, MySQL `Characters(64)` and SQLite
/// `Unbounded`, so the tightest IS PostgreSQL's. A fourth backend declaring a cap
/// under 63 would have been silently ignored, and the engine would have precomputed
/// names its own registry says do not fit.
///
/// # IT WAS A `const`, AND THE COMPILE-TIME EVALUATION IS GONE
///
/// This was a `pub(crate) const`, folded by the compiler over the shipping set at its
/// definition, consumed by five files that never saw a registry. It was the ONE
/// reader of the composition that did not take it as an argument, and that is exactly
/// why it could not survive the composition leaving the crate: an engine that cannot
/// name the vendors cannot fold over them at compile time either.
///
/// So the budget is a RUNTIME QUERY on the registry the caller already carries, and
/// the loss is real and is not being described as a wash. What went:
///
/// * the fold ran ONCE per build; it now runs once per minted or checked name;
/// * a wrong answer was a build-time impossibility rather than a runtime value;
/// * `cap_ident_name` and its siblings grew a `vendors` parameter, so a caller that
///   mints a generated name has to be handed the set - which is the same threading
///   every other resolution in this module already pays for.
///
/// What was bought is the only thing that matters here: ONE source of truth, and it
/// stays beside the registry rather than becoming a number the composing crate
/// computes and passes in. A budget passed as a bare `usize` from outside would be a
/// second place the tightest cap is decided, and nothing would notice it disagreeing
/// with the set the same call resolves its renderer from.
///
/// # The fold
///
/// Each declared limit is converted to a BYTE budget, taking the safe direction in
/// both cases where the two units differ:
///
/// * `IdentifierLimit::Bytes(n)` is already a byte budget.
/// * `IdentifierLimit::Characters(n)` becomes `n` bytes, because a byte string of
///   length `b` holds at most `b` characters - so `b <= n` bytes always fits an
///   `n`-character cap, whatever encoding the name is in. Treating it as `4 * n`
///   would be the true maximum and the WRONG direction: it would let the engine mint
///   a name that fits only if the name happens to be ASCII.
/// * `IdentifierLimit::Unbounded` imposes nothing, so it contributes `usize::MAX`
///   and cannot be the minimum unless it is the only kind present.
///
/// An empty registry would yield `usize::MAX`, which cannot arise: the registry
/// refuses to be composed empty.
///
/// It stays a `const fn` so a caller that DOES hold a compile-time set can still get
/// the fold at compile time. Nothing in the engine can any more; the composing crate
/// can.
pub(crate) const fn generated_ident_max_bytes(vendors: VendorSet) -> usize {
    let vendors = vendors.as_slice();
    let mut budget = usize::MAX;
    let mut at = 0;
    while at < vendors.len() {
        let declared = match vendors[at].descriptor.limits.identifier {
            zeroship_migrate_ir::backend::IdentifierLimit::Bytes(n)
            | zeroship_migrate_ir::backend::IdentifierLimit::Characters(n) => n,
            zeroship_migrate_ir::backend::IdentifierLimit::Unbounded => usize::MAX,
        };
        if declared < budget {
            budget = declared;
        }
        at += 1;
    }
    budget
}

/// The vendor for a dialect.
///
/// Resolved by the open [`DialectId`] filed in
/// each [`BackendVendor`], never by an enum match in core. The shipping list above is
/// the one composition point that names backend crates.
pub(crate) fn vendor(vendors: VendorSet, dialect: &DialectId) -> &'static BackendVendor {
    vendors
        .get(dialect)
        .unwrap_or_else(|| panic!("no registered backend vendor for {dialect}"))
}

/// The DML renderer for a dialect.
pub(crate) fn renderer(vendors: VendorSet, dialect: &DialectId) -> &'static dyn DmlRenderer {
    vendor(vendors, dialect).dml
}

/// The schema renderer for a dialect. Re-exported as `schema::query::renderer`.
pub(crate) fn schema_renderer(
    vendors: VendorSet,
    dialect: &DialectId,
) -> &'static dyn zeroship_migrate_backend::schema::SchemaRenderer {
    vendor(vendors, dialect).schema
}

/// The value-format renderer and catalog normalizer registered by a dialect's vendor.
pub(crate) fn value_format_renderer(
    vendors: VendorSet,
    dialect: &DialectId,
) -> &'static dyn zeroship_migrate_backend::value_format::ValueFormatRenderer {
    vendor(vendors, dialect).value_format
}

/// Every shipping value-format renderer, used only when a legacy snapshot has
/// no backend provenance and the neutral comparator must compose the vendors'
/// explicitly declared normalization rules.
pub(crate) fn value_format_renderers(
    vendors: VendorSet,
) -> impl Iterator<Item = &'static dyn zeroship_migrate_backend::value_format::ValueFormatRenderer> {
    vendors.as_slice().iter().map(|vendor| vendor.value_format)
}

/* `pub(crate) fn stored_ddl(dialect)` USED TO LIVE HERE. Its only caller was
 * SQLite's execution half, which asked this registry which parser handles SQLite
 * from inside the SQLite backend. That half is `zeroship-migrate-sqlite` now and names
 * `crate::stored_ddl::PARSER` directly, leaving this resolver with zero callers.
 *
 * Nothing was lost: the parser is still reached, by everything that has a resolved
 * renderer, through the `SchemaRenderer::stored_ddl()` method this body forwarded to
 * - see `render/declarative.rs`, which calls it on the renderer it already holds.
 */

/// The schema-bound DDL emitter registered by a dialect's vendor crate.
pub(crate) fn ddl_emitter(
    vendors: VendorSet,
    dialect: &DialectId,
    project_schema: &str,
) -> Box<dyn DdlEmitter> {
    (vendor(vendors, dialect).ddl)(project_schema)
}

/// The LINE-1 guard for a config's dialect - this vendor's, built by this vendor.
///
/// This replaced the old `guard_for` free function in the since-dissolved guard crate, which was a second
/// closed identity match living in the guard crate and mapping BOTH
/// descriptor-only dialects onto one shared guard type. Two consequences
/// of folding it into the vendor registry are worth stating:
///
/// - Every backend surface now resolves through the same [`VendorSet`] lookup, keyed
///   by the vendor's open id; there is no enum dispatch here for a fourth backend to
///   be omitted from.
/// - "This vendor ships no guard" became a compile error at the vendor's own
///   definition site rather than something a `_ =>` arm here could paper over. See
///   `zeroship_migrate_backend::registry::BackendVendor`.
///
/// SQLite and MySQL no longer share a guard TYPE either; each writes its own trusting
/// impl, so a change to one dialect's posture cannot silently become a change to the
/// other's.
pub(crate) fn guard_for(vendors: VendorSet, cfg: &GuardConfig) -> Box<dyn MigrationGuard> {
    (vendor(vendors, cfg.dialect()).guard)(cfg)
}

/// The OPERATIONAL analyzer for a dialect - this vendor's, run by this vendor.
///
/// The advisory counterpart of [`guard_for`], and it removed the same shape of
/// coupling. Before it, `render::declarative` called `crate::analysis::analyze::...`
/// directly: a re-export that resolved into the `libpg_query` analyzer crate, so the
/// engine ran the PostgreSQL parser over every backend's DDL and reported the
/// resulting parse failures as a clean, empty advisory list. That was the whole of
/// the engine's advisory routing, and it named a parser rather than a vendor.
pub(crate) fn advisor(vendors: VendorSet, dialect: &DialectId) -> &'static dyn OperationalAdvisor {
    vendor(vendors, dialect).advisor
}

/// The operational advisories the backend registered for `dialect` finds in `sql`.
///
/// The verdict, not a list: see
/// [`AdvisoryVerdict`] for why a
/// backend with no analyzer must not answer with an empty vector.
#[must_use]
pub fn advisories_for_sql(vendors: VendorSet, dialect: &DialectId, sql: &str) -> AdvisoryVerdict {
    advisor(vendors, dialect).advise(sql)
}

/// Whether the backend registered for `dialect` ships an operational analyzer at
/// all, answered without SQL.
///
/// A caller reporting on a SET of statements asks this ONCE, up front, so it can
/// tell an operator the whole set is unchecked before it renders the first statement
/// - and still say so when the set renders to nothing.
#[must_use]
pub fn analyzer_absence(vendors: VendorSet, dialect: &DialectId) -> Option<AnalyzerAbsent> {
    advisor(vendors, dialect).analyzer_absence()
}

/// Every identifier prefix any REGISTERED backend reserves for its own catalog,
/// paired with the backend that reserves it.
///
/// The union rather than the selected target's, for the same reason
/// [`generated_ident_max_bytes`] is the tightest cap rather than the selected one: a
/// declared name that is legal here and reserved on another registered backend is a
/// re-targeting hazard, and refusing it at declaration is cheaper than discovering it
/// at deploy. Both halves are returned because the refusal should say WHOSE catalog
/// claims the prefix, which is the fact the operator needs and the one core cannot
/// state without asking.
///
/// This replaced two literals in `schema::query`: a hand-rolled `pg_` byte comparison
/// in `validate_collection`, and a `ReservedName::Prefix("sqlite_")` row in the
/// platform reserved-name table. Between them they made two backends' catalog
/// conventions part of the neutral name validator, and left a fourth backend's
/// reservation with nowhere to be declared.
pub(crate) fn reserved_catalog_prefixes(
    vendors: VendorSet,
) -> impl Iterator<Item = (&'static str, &'static str)> {
    vendors.as_slice().iter().flat_map(|vendor| {
        vendor
            .descriptor
            .limits
            .reserved_identifier_prefixes
            .iter()
            .map(move |prefix| (*prefix, vendor.descriptor.id.as_str()))
    })
}

/// The registered targets that DO declare `capability`, spelled for an operator who
/// was just refused for want of it.
///
/// This exists because a capability refusal has two halves and only one of them was
/// ever neutral. "This target cannot do X" already named the target from its own
/// [`DialectId`]. The advice beside it - "target Postgres" - was a compiled-in vendor
/// string, in core, restating a fact the registry already holds and would keep holding
/// after it stopped being true. Every one of those strings was written when three
/// backends shipped and PostgreSQL was the only one with the capability in question; a
/// fourth backend that declared it would have been told to go somewhere else.
///
/// Returns `None` when NO registered backend declares it, which is a different
/// sentence and must not be rendered as an empty list of alternatives - a fix that
/// says "target one of: " has told the operator nothing. Callers spell that case
/// themselves.
pub(crate) fn targets_declaring(
    vendors: VendorSet,
    capability: zeroship_migrate_ir::backend::Capability,
) -> Option<String> {
    let able: Vec<&str> = vendors
        .as_slice()
        .iter()
        .filter(|vendor| vendor.descriptor.capabilities.contains(capability))
        .map(|vendor| vendor.descriptor.id.as_str())
        .collect();
    match able.as_slice() {
        [] => None,
        [only] => Some((*only).to_string()),
        [rest @ .., last] => Some(format!("{} or {last}", rest.join(", "))),
    }
}

/// The index-coverage facts `dialect`'s backend reads out of `sql`, for the
/// plan-wide `FK_WITHOUT_INDEX` suppression.
pub(crate) fn index_coverage(vendors: VendorSet, dialect: &DialectId, sql: &str) -> IndexCoverage {
    advisor(vendors, dialect).index_coverage(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_returns_expected_dml_renderer() {
        assert_eq!(
            renderer(crate::test_fixtures::VENDORS, &POSTGRES).synth_now(),
            "now()"
        );
        assert_eq!(
            renderer(crate::test_fixtures::VENDORS, &SQLITE).synth_now(),
            "CURRENT_TIMESTAMP"
        );
        assert_eq!(
            renderer(crate::test_fixtures::VENDORS, &MYSQL).synth_now(),
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
        let registry = crate::test_fixtures::VENDORS
            .descriptors()
            .expect("the shipping vendors must satisfy the dialect-id rule");
        assert_eq!(registry.len(), crate::test_fixtures::VENDORS.len());
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
        for v in crate::test_fixtures::VENDORS.as_slice() {
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

    /// The engine's generated-name budget is the TIGHTEST cap in the registry, and
    /// that is checked against every registrant rather than against one vendor.
    ///
    /// This is the assertion the previous body could not make. It read
    /// `POSTGRES_VENDOR.descriptor.limits.identifier` directly, so "the limiting cap"
    /// was a claim in its doc and "PostgreSQL's cap" was the code, and the two agree
    /// today only because PostgreSQL happens to be the strictest of the three. A
    /// register-a-tighter-backend regression would have produced generated names the
    /// engine's own registry says do not fit, and no test in the tree would have
    /// moved.
    ///
    /// Deliberately NOT `assert_eq!(generated_ident_max_bytes(crate::test_fixtures::VENDORS), 63)`. That
    /// restates the answer instead of checking it, and it is exactly as true of the
    /// broken one-vendor lookup as of the fold.
    #[test]
    fn the_generated_ident_budget_fits_every_registered_backend() {
        let budget = generated_ident_max_bytes(crate::test_fixtures::VENDORS);
        let mut tightest = usize::MAX;
        for v in crate::test_fixtures::VENDORS.as_slice() {
            let declared = match v.descriptor.limits.identifier {
                zeroship_migrate_ir::backend::IdentifierLimit::Bytes(n)
                | zeroship_migrate_ir::backend::IdentifierLimit::Characters(n) => n,
                zeroship_migrate_ir::backend::IdentifierLimit::Unbounded => usize::MAX,
            };
            assert!(
                budget <= declared,
                "{} declares an identifier cap of {declared}, but the engine mints \
                 generated names up to {budget} bytes and hands them to every emitter",
                v.descriptor.display_name
            );
            tightest = tightest.min(declared);
        }
        assert_eq!(
            budget, tightest,
            "the budget is below the tightest declared cap, so it is costing every \
             backend name length no registrant asked for"
        );
        assert!(
            crate::test_fixtures::VENDORS.len() >= 3,
            "the sweep ran over {} vendors; a budget checked against an empty or \
             truncated registry is vacuous",
            crate::test_fixtures::VENDORS.len()
        );
    }

    /// SQLite cannot reach a deferred `ALTER TABLE ... ADD CONSTRAINT` through the
    /// current capability gate, but the old core router still selected the
    /// PostgreSQL FK-clause body for that arm. Moving the body must preserve that
    /// dormant answer too: unreachable is not permission to reimplement it.
    #[test]
    fn sqlite_deferred_fk_clause_keeps_the_former_postgres_bytes() {
        let fk = zeroship_migrate_backend::snapshot::ConstraintSnapshot {
            name: "fk\"child".to_string(),
            kind: "FOREIGN KEY".to_string(),
            definition: "FOREIGN KEY (\"child\"\"col\") REFERENCES old.parents(id, \"parent\"\"col\") ON DELETE CASCADE".to_string(),
            comment: None,
            cascade_columns: None,
        };
        let expected = "CONSTRAINT \"fk\"\"child\" FOREIGN KEY (\"child\"\"col\") REFERENCES \"project\"\"schema\".\"parents\" (id, \"parent\"\"col\") ON DELETE CASCADE";

        let sqlite =
            ddl_emitter(crate::test_fixtures::VENDORS, &SQLITE, "project\"schema").fk_clause(&fk);
        let postgres =
            ddl_emitter(crate::test_fixtures::VENDORS, &POSTGRES, "project\"schema").fk_clause(&fk);
        assert_eq!(sqlite, expected);
        assert_eq!(sqlite, postgres);
    }
}
