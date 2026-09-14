//! # `zeroship-migrate-postgres` - the PostgreSQL backend
//!
//! One vendor, no engine. This crate holds PostgreSQL's DML, schema, DDL, and
//! value-format renderers plus its guard, and it
//! depends on `zeroship-migrate-backend` and `zeroship-migrate-ir` - never on the engine.
//! That is the whole point of the split: the engine names this crate for its
//! registry, so this crate must not name the engine back.
//!
//! # The one-dialect-literal rule
//!
//! This CRATE names its dialect exactly ONCE - [`DIALECT`] in this file - and no
//! module names another vendor at all. Everything else reads `crate::DIALECT`.
//!
//! The rule used to be per-MODULE: each renderer held its own
//! `const DIALECT: DialectId = POSTGRES;` and imported that name from
//! `zeroship-migrate-ir`, the neutral vocabulary crate, which declared the ids for all
//! the shipping vendors. The ids moved into the vendor crates; this crate exports
//! its `DIALECT` identity and its modules read that value.
//!
//! # What the rule does NOT catch
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper
//! that hard-codes a dialect, and no grep of this crate can see it because the
//! literal lives in `zeroship-migrate-backend`. That is measured, not hypothetical -
//! `zeroship_migrate_backend::dml`'s header carries the numbers. The identifier seam
//! (`*_for_dialect(.., DIALECT)`) is how this crate stays clear of it.

mod advisory;
pub mod analysis;
pub mod attribute;
/// The `MigrationBackend` implementation: PostgreSQL's session/lock/transaction
/// bracket, its journal, drift, backfill, identity, primary-key, baseline,
/// precondition and status SQL.
///
/// This is the EXECUTION half. It arrived from `zero-migrate`'s
/// `apply/backend/postgres/` - the LAST vendor backend inside the engine - and it
/// reaches nothing but `zeroship-migrate-backend`, `zeroship-migrate-ir` and this crate's
/// own renderers, descriptor and guard.
pub mod backend;
// The confinement settings only this backend reads, and the host seam that sets
// them. `pub` because a host has to be able to name `PostgresConfinement` and reach
// `PostgresConfinementExt::with_migrator_role`; the neutral crate cannot offer either
// without naming this one.
pub mod confinement;
mod ddl;
mod descriptor;
mod dml;
mod dual_write;
mod existence_probe;
mod fold;
pub mod guard;
pub mod role;
mod schema;
mod validation;
mod value_format;
mod vendor;

// `render_vendor_op` IS NOT RE-EXPORTED, and its absence is the enforcement.
//
// It used to be `pub use vendor::render_vendor_op`, because the engine called it
// directly at three sites covering sixteen op kinds that never reach
// `DmlRenderer::render_trigger_op`. That made the vendor-op surface the one part of
// a backend the engine knew by NAME rather than by contract, and both this file and
// `zeroship_migrate::render::vendor` said so in as many words.
//
// It is behind `DmlRenderer::render_vendor_op` now. `mod vendor` above is private,
// so with this re-export gone the function is UNREACHABLE from outside this crate:
// core naming it again is an E0603 privacy error at the use site, not a review
// comment. Privacy enforces this boundary within the vendor crate.
//
// The function itself did not move and did not change. `crate::vendor` is the same
// module it was; what changed is who may ask for it.

/// This vendor's line-1 guard, re-exported because the engine's public API has
/// surfaced it since before the vendor crates existed.
pub use guard::PgGuard;

/// This vendor's `MigrationBackend`, re-exported at the crate root the way
/// `zeroship_migrate_sqlite::SqliteBackend` and `zeroship_migrate_mysql::MysqlBackend` are.
pub use backend::PostgresBackend;

/// TEST-ONLY charter fixtures, shared by this crate's unit tests.
///
/// The engine's `zeroship_migrate::test_fixtures::no_inject` is `pub(crate)`, and no
/// visibility widening can make a `pub(crate)` reachable across a crate boundary -
/// so the execution half's tests needed a sibling when they moved here. This is it,
/// and it is the same shape `zeroship-migrate-sqlite`'s and `zeroship-migrate-mysql`'s have.
#[cfg(test)]
mod test_fixtures;

use zeroship_migrate_backend::registry::BackendVendor;
use zeroship_migrate_ir::dialect::DialectId;

/// This backend's id STRING, spelled once for the whole crate.
///
/// Separate from [`DIALECT`] only so the compile-time check below can read the
/// same bytes the id is built from. Asserting on the `DialectId` itself is not
/// available: evaluating one inside a `const` item copies it, and const
/// evaluation refuses to run the `Cow`'s destructor (E0493). One string, two
/// readers, and no way for the declaration and the check to drift apart.
const NAME: &str = "postgres";

/// This backend's identity, declared HERE for every shipping path.
///
/// `zeroship-migrate-ir` is the neutral vocabulary crate and its own module doc says a
/// backend "declares its own - `DialectId::new(\"duckdb\")` - without editing this
/// crate". It used to declare three anyway, and core re-exported them, so every
/// consumer that wanted to name PostgreSQL reached a neutral crate to get it. This
/// is the declaration that ended that: the `NAME` const above is the workspace's only
/// non-test spelling of it, and [`VENDOR`]'s descriptor, this crate's own modules,
/// the composition's `tests/` binaries and the Node host all read it from here.
///
/// The neutral crates' own `#[cfg(test)]` modules do rebuild the string, because a
/// crate that must not depend on a vendor cannot import the id it needs to write a
/// test. `DialectId` compares by content, so those rebuilds ARE this id rather than
/// a second one. Measure the non-test set with
/// `git grep -n 'DialectId::new("postgres")' -- crates`.
///
/// A fourth backend adds its own `DIALECT` in its own crate and edits neither the
/// contract crate nor any other vendor.
pub const DIALECT: DialectId = DialectId::new(NAME);

/// The id rule, asserted at COMPILE time on this crate's own declaration.
///
/// [`DialectId::new`] is `const` and so cannot return a `Result`; the registry
/// re-checks at build time because a backend is not trusted about its own
/// declaration. This is the same check one hop earlier, in the crate that owns the
/// string, where a violation is a compile error naming this line rather than a
/// registry panic naming a value.
const _: () = assert!(DialectId::is_well_formed_name(NAME));

/// Everything the engine needs from this crate: the capability descriptor, the four
/// renderers, and the line-1 guard.
///
/// The renderer structs themselves are deliberately private. A caller reaches this
/// vendor's spelling through a registry or not at all, which is the property the
/// in-crate `match` used to give for free and which `pub` statics would have thrown
/// away at exactly the moment the vendor became separately linkable.
///
/// `value_format`, `validation`, `ddl`, `guard` and `advisor` are REQUIRED. Delete any line and this literal stops
/// compiling, here, with this crate named - which is the point: a backend cannot
/// inherit another backend's DDL, acquire a trusting guard, or acquire a silently
/// empty advisory report by omission. See
/// `zeroship_migrate_backend::registry::BackendVendor`.
pub static VENDOR: BackendVendor = BackendVendor {
    attributes: attribute::VOCABULARY,
    descriptor: &descriptor::POSTGRES_DESCRIPTOR,
    dml: &dml::RENDERER,
    schema: &schema::RENDERER,
    value_format: &value_format::RENDERER,
    existence_probe: &existence_probe::POLICY,
    catalog_fold: &fold::POLICY,
    validation: &validation::POLICY,
    ddl: ddl::emitter,
    guard: guard::guard,
    advisor: &advisory::ADVISOR,
};
