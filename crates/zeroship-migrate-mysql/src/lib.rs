//! # `zero-migrate-mysql` - the MySQL backend
//!
//! One vendor, no engine. This crate holds BOTH halves of MySQL now:
//!
//! * the RENDER half - DML, schema, DDL and value-format renderers plus the line-1
//!   guard, all registered through [`VENDOR`]; and
//! * the EXECUTION half - [`backend`], the `MigrationBackend` implementation: the
//!   `GET_LOCK` project lock, the session pins, the two-phase non-transactional
//!   apply MySQL's auto-committing DDL forces, the journal DDL and net-state reads,
//!   the `information_schema` drift snapshot, and the resumable backfill.
//!
//! It depends on `zero-migrate-backend` and `zero-migrate-ir` - never on the engine.
//! That is the whole point of the split: the engine names this crate for its
//! registry, so this crate must not name the engine back.
//!
//! # What that cost, and what it bought
//!
//! The execution half was the last vendor backend inside the engine, and moving it
//! is what the governing rule - the core is neutral, and that is the hard limit -
//! asks for. Three of its couplings could not simply follow it down, and each was
//! answered by this crate naming ITSELF instead of asking a registry:
//!
//! * the drift snapshot resolved MySQL's `SchemaRenderer` out of the engine's
//!   registry by dialect; it reads `crate::schema::RENDERER`;
//! * the journal and backfill identifier quoting resolved MySQL's `DmlRenderer` the
//!   same way; both pass `&crate::dml::RENDERER` to the neutral spelling seam; and
//! * the catalog foreign-key body went through an engine wrapper whose only extra
//!   job was DERIVING a constraint name, which a key read out of a catalog never
//!   needs; it calls `fk_constraint_snapshot` with `&VENDOR` directly.
//!
//! What could NOT be answered that way was the catalog value-format comparison,
//! which is single-sourced on purpose: it moved DOWN into
//! `zeroship_migrate_backend::value_format` and takes its renderers as parameters.
//!
//! # The one-dialect-literal rule
//!
//! This CRATE names its dialect exactly ONCE - [`DIALECT`] in this file - and no
//! module names another vendor at all. Everything else reads `crate::DIALECT`.
//!
//! The rule used to be per-MODULE: each renderer held its own
//! `const DIALECT: DialectId = MYSQL;` and imported that name from
//! `zero-migrate-ir`, the neutral vocabulary crate, which declared the ids for all
//! three shipping vendors. The ids moved into the vendors, so the rule tightened to
//! per-crate: `"mysql"` is now spelled in exactly one place in this crate and in
//! exactly one place in the workspace. It is ENFORCED, across the crate boundary, by
//! `zero-migrate/tests/dialect_matrix/backend_modules_name_one_dialect.rs`.
//!
//! # What the rule does NOT catch
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper
//! that hard-codes a dialect, and no grep of this crate can see it because the
//! literal lives in `zero-migrate-backend`. That is measured, not hypothetical -
//! `zeroship_migrate_backend::dml`'s header carries the numbers. The identifier seam
//! (`*_for_dialect(.., DIALECT)`) is how this crate stays clear of it.

mod advisory;
pub mod attribute;
/// The `MigrationBackend` implementation: MySQL's lock, session, journal, drift,
/// backfill and DDL-step execution over the dialect-neutral `SqlSession` seam.
///
/// This is the EXECUTION half. It arrived from `zero-migrate`'s
/// `apply/backend/mysql/`, where it was the last vendor backend still inside the
/// engine, and it reaches nothing but `zero-migrate-backend`, `zero-migrate-ir` and
/// this crate's own renderers.
pub mod backend;
pub mod collation;
mod ddl;
mod descriptor;
mod dml;
mod existence_probe;
mod fold;
pub mod guard;
// This backend's own parsed type identity, and the carrier leg it rides to the
// neutral column snapshot in. `pub` because the engine's drift comparator holds the
// leg - it cannot read one without naming this type, which is the point.
pub mod physical_type;
mod schema;
mod validation;
mod value_format;

pub use backend::{
    MysqlBackend, MysqlInflightDdlMarker, MysqlInflightRecoveryError, MysqlInflightRecoveryOutcome,
    MysqlInflightResolution, BINARY_IDENTITY_COLUMNS,
};
pub use guard::MysqlGuard;

/// TEST-ONLY charter fixtures, shared by this crate's unit tests.
///
/// The engine's `test_fixtures::no_inject` is `pub(crate)` and cannot cross a crate
/// boundary, so this is the MySQL sibling of `zero-migrate-node`'s. It composes
/// through the REAL `zeroship_migrate_ir::policy_registry` rather than restating the
/// algebra, so what a vendor's tests compose and what production composes cannot
/// drift.
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
const NAME: &str = "mysql";

/// This backend's identity, declared HERE for every shipping path.
///
/// `zero-migrate-ir` is the neutral vocabulary crate and its own module doc says a
/// backend "declares its own - `DialectId::new(\"duckdb\")` - without editing this
/// crate". It used to declare three anyway, and core re-exported them, so every
/// consumer that wanted to name `MySQL` reached a neutral crate to get it. This is
/// the declaration that ended that: the `NAME` const above is the workspace's only
/// non-test spelling of it, and [`VENDOR`]'s descriptor, this crate's own modules,
/// the composition's `tests/` binaries and the Node host all read it from here.
///
/// The neutral crates' own `#[cfg(test)]` modules do rebuild the string, because a
/// crate that must not depend on a vendor cannot import the id it needs to write a
/// test. `DialectId` compares by content, so those rebuilds ARE this id rather than
/// a second one. Measure the non-test set with
/// `git grep -n 'DialectId::new(\"mysql\")' -- crates`.
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
    descriptor: &descriptor::MYSQL_DESCRIPTOR,
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
