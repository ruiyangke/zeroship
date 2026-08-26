//! # `zeroship-migrate-sqlite` - the SQLite backend
//!
//! One vendor, no engine. This crate holds BOTH halves of SQLite now:
//!
//! * the RENDER half - DML, schema, DDL and value-format renderers plus the line-1
//!   guard, all registered through [`VENDOR`]; and
//! * the EXECUTION half - [`backend`], the `MigrationBackend` implementation: the
//!   dedicated hardened CDC-free `rusqlite` actor, the two-mode prepare-time
//!   authorizer that is SQLite's second line of confinement, the `_mig` attached
//!   journal, the `sqlite_master` + `PRAGMA` drift snapshot, the twelve-step table
//!   rebuild, the batched backfill, and the OS-backed project lock.
//!
//! It depends on `zeroship-migrate-backend` and `zeroship-migrate-ir` - never on the engine.
//! That is the whole point of the split: the engine names this crate for its
//! registry, so this crate must not name the engine back.
//!
//! # What that cost, and what it bought
//!
//! Moving the execution half is what the governing rule - the core is neutral, and
//! that is the hard limit - asks for. Not one of its couplings needed a change on
//! the core side: every one was the ENGINE'S REGISTRY being asked which backend
//! handles SQLite, from inside the SQLite backend, and each is answered by this
//! crate naming ITSELF:
//!
//! * `render::backends::stored_ddl(&DIALECT)` - and the local `fn stored_ddl()`
//!   wrapper that unwrapped its `Option` - became `crate::stored_ddl::PARSER`, the
//!   spelling `fold.rs` here already used;
//! * `render::backends::schema_renderer(&DIALECT)` became `&crate::schema::RENDERER`,
//!   which is the value that lookup returned;
//! * the four identifier-quoting seams (`escape_quote_ident_for_dialect`,
//!   `quote_ident_checked_for_dialect`) pass `&crate::dml::RENDERER` to the neutral
//!   `*_for_backend` entry points `zeroship_migrate_backend::dml`'s header describes;
//! * the catalog value-format comparison (`catalog_id_default`,
//!   `catalog_uuid_id_default`, `recover_format_check`, `column_metadata`) calls
//!   `zeroship_migrate_backend::value_format` with this vendor's own two renderers -
//!   the engine's doors are a `pub(crate)` module whose bodies are exactly that;
//! * `schema::query::normalize_fk_action_for_dialect` and the engine's FK-snapshot
//!   wrapper became `normalize_fk_action_for_vendor` / `fk_constraint_snapshot`
//!   with `&VENDOR`, as the wrapper's own doc comment says a backend should; and
//! * the existence-probe decider takes a `&BackendVendor`, so `backend/mod.rs` also
//!   lost the one stray dialect constant it named outside its `SQLITE_DIALECT`
//!   alias.
//!
//! Nothing in `zero-migrate` was widened to `pub` to make this compile, and core
//! re-exports nothing from here: `apply::backend::sqlite` is gone rather than
//! repointed, because a `pub use zeroship_migrate_sqlite::SqliteBackend` in core would
//! be core naming a vendor CRATE outside the registry - trading one coupling for
//! another. Consumers name this crate directly.
//!
//! # The one-dialect-literal rule
//!
//! This CRATE names its dialect exactly ONCE - [`DIALECT`] in this file - and no
//! module names another vendor at all. Everything else reads `crate::DIALECT`, the
//! execution half through its `SQLITE_DIALECT` alias.
//!
//! The rule used to be per-MODULE: each renderer held its own
//! `const DIALECT: DialectId = SQLITE;` and imported that name from
//! `zeroship-migrate-ir`, the neutral vocabulary crate, which declared the ids for all
//! three shipping vendors. The ids moved into the vendors, so the rule tightened to
//! per-crate: `"sqlite"` is now spelled in exactly one place in this crate and in
//! exactly one place in the workspace.
//!
//! `zero-migrate/tests/dialect_matrix/backend_modules_name_one_dialect.rs` ENFORCES
//! it across the crate boundary - both halves, since the needle is now the
//! DECLARATION rather than a per-module const, and a crate has exactly one.
//!
//! # What the rule does NOT catch
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper
//! that hard-codes a dialect, and no grep of this crate can see it because the
//! literal lives in `zeroship-migrate-backend`. That is measured, not hypothetical -
//! `zeroship_migrate_backend::dml`'s header carries the numbers. The identifier seam
//! (`*_for_dialect(.., DIALECT)`) is how this crate stays clear of it.

mod advisory;
pub mod attribute;
/// The `MigrationBackend` implementation: SQLite's hardened single-writer actor,
/// its prepare-time authorizer, and the journal / drift / rebuild / backfill /
/// rollback SQL that runs on it.
///
/// This is the EXECUTION half. It arrived from `zero-migrate`'s
/// `apply/backend/sqlite/` and it reaches nothing but `zeroship-migrate-backend`,
/// `zeroship-migrate-ir` and this crate's own renderers.
pub mod backend;
mod ddl;
mod descriptor;
mod dml;
mod existence_probe;
mod fold;
pub mod guard;
mod plan;
mod schema;
mod stored_ddl;
mod table_rebuild;
mod validation;
mod value_format;

pub use backend::{RebuildError, SqliteActorError, SqliteBackend};
pub use guard::SqliteGuard;
pub use plan::SqliteSequencePolicy;

/// TEST-ONLY charter fixtures, shared by this crate's unit tests.
///
/// The engine's `zeroship_migrate::test_fixtures::no_inject` is `pub(crate)`, and no
/// visibility widening can make a `pub(crate)` reachable across a crate boundary -
/// so the four project-lock tests that came with the execution half needed a
/// sibling. This is it, and it is the same shape `zeroship-migrate-mysql`'s already has.
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
const NAME: &str = "sqlite";

/// This backend's identity, declared HERE for every shipping path.
///
/// `zeroship-migrate-ir` is the neutral vocabulary crate and its own module doc says a
/// backend "declares its own - `DialectId::new(\"duckdb\")` - without editing this
/// crate". It used to declare three anyway, and core re-exported them, so every
/// consumer that wanted to name `SQLite` reached a neutral crate to get it. This is
/// the declaration that ended that: the `NAME` const above is the workspace's only
/// non-test spelling of it, and [`VENDOR`]'s descriptor, this crate's own modules,
/// the composition's `tests/` binaries and the Node host all read it from here.
///
/// The neutral crates' own `#[cfg(test)]` modules do rebuild the string, because a
/// crate that must not depend on a vendor cannot import the id it needs to write a
/// test. `DialectId` compares by content, so those rebuilds ARE this id rather than
/// a second one. Measure the non-test set with
/// `git grep -n 'DialectId::new(\"sqlite\")' -- crates`.
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
    descriptor: &descriptor::SQLITE_DESCRIPTOR,
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
