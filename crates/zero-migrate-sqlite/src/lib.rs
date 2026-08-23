//! # `zero-migrate-sqlite` — the SQLite backend
//!
//! One vendor, no engine. This crate holds BOTH halves of SQLite now:
//!
//! * the RENDER half — DML, schema, DDL and value-format renderers plus the line-1
//!   guard, all registered through [`VENDOR`]; and
//! * the EXECUTION half — [`backend`], the `MigrationBackend` implementation: the
//!   dedicated hardened CDC-free `rusqlite` actor, the two-mode prepare-time
//!   authorizer that is SQLite's second line of confinement, the `_mig` attached
//!   journal, the `sqlite_master` + `PRAGMA` drift snapshot, the twelve-step table
//!   rebuild, the batched backfill, and the OS-backed project lock.
//!
//! It depends on `zero-migrate-backend` and `zero-migrate-ir` — never on the engine.
//! That is the whole point of the split: the engine names this crate for its
//! registry, so this crate must not name the engine back.
//!
//! # What that cost, and what it bought
//!
//! Moving the execution half is what the governing rule — the core is neutral, and
//! that is the hard limit — asks for. Not one of its couplings needed a change on
//! the core side: every one was the ENGINE'S REGISTRY being asked which backend
//! handles SQLite, from inside the SQLite backend, and each is answered by this
//! crate naming ITSELF:
//!
//! * `render::backends::stored_ddl(&DIALECT)` — and the local `fn stored_ddl()`
//!   wrapper that unwrapped its `Option` — became `crate::stored_ddl::PARSER`, the
//!   spelling `fold.rs` here already used;
//! * `render::backends::schema_renderer(&DIALECT)` became `&crate::schema::RENDERER`,
//!   which is the value that lookup returned;
//! * the four identifier-quoting seams (`escape_quote_ident_for_dialect`,
//!   `quote_ident_checked_for_dialect`) pass `&crate::dml::RENDERER` to the neutral
//!   `*_for_backend` entry points `zero_migrate_backend::dml`'s header describes;
//! * the catalog value-format comparison (`catalog_id_default`,
//!   `catalog_uuid_id_default`, `recover_format_check`, `column_metadata`) calls
//!   `zero_migrate_backend::value_format` with this vendor's own two renderers —
//!   the engine's doors are a `pub(crate)` module whose bodies are exactly that;
//! * `schema::query::normalize_fk_action_for_dialect` and the engine's FK-snapshot
//!   wrapper became `normalize_fk_action_for_vendor` / `fk_constraint_snapshot`
//!   with `&VENDOR`, as the wrapper's own doc comment says a backend should; and
//! * the existence-probe decider takes a `&BackendVendor`, so `backend/mod.rs` also
//!   lost the one stray `&zero_migrate_ir::dialect::SQLITE` it named outside its
//!   `SQLITE_DIALECT` const.
//!
//! Nothing in `zero-migrate` was widened to `pub` to make this compile, and core
//! re-exports nothing from here: `apply::backend::sqlite` is gone rather than
//! repointed, because a `pub use zero_migrate_sqlite::SqliteBackend` in core would
//! be core naming a vendor CRATE outside the registry — trading one coupling for
//! another. Consumers name this crate directly.
//!
//! # The one-dialect-literal rule
//!
//! Each module names its own dialect exactly ONCE, as its `DIALECT` const, and names
//! no other dialect at all. Everything else reads `DIALECT`. The rule predates the
//! extraction and it is what made the extraction mechanical.
//!
//! `zero-migrate/tests/dialect_matrix/backend_modules_name_one_dialect.rs` ENFORCES
//! it across the crate boundary, but be precise about what it reads: a NAMED set of
//! nine files, three per vendor, all in the RENDER half. [`backend`] is not in that
//! set, and neither is `zero-migrate-mysql`'s. Both execution halves hold the rule
//! by construction — `backend/mod.rs`'s `SQLITE_DIALECT` is the subtree's one
//! literal and every other module reads it — but they hold it unwatched.
//!
//! # What the rule does NOT catch
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper
//! that hard-codes a dialect, and no grep of this crate can see it because the
//! literal lives in `zero-migrate-backend`. That is measured, not hypothetical —
//! `zero_migrate_backend::dml`'s header carries the numbers. The identifier seam
//! (`*_for_dialect(.., DIALECT)`) is how this crate stays clear of it.

mod advisory;
/// The `MigrationBackend` implementation: SQLite's hardened single-writer actor,
/// its prepare-time authorizer, and the journal / drift / rebuild / backfill /
/// rollback SQL that runs on it.
///
/// This is the EXECUTION half. It arrived from `zero-migrate`'s
/// `apply/backend/sqlite/` and it reaches nothing but `zero-migrate-backend`,
/// `zero-migrate-ir` and this crate's own renderers.
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
/// The engine's `zero_migrate::test_fixtures::no_inject` is `pub(crate)`, and no
/// visibility widening can make a `pub(crate)` reachable across a crate boundary —
/// so the four project-lock tests that came with the execution half needed a
/// sibling. This is it, and it is the same shape `zero-migrate-mysql`'s already has.
#[cfg(test)]
mod test_fixtures;

use zero_migrate_backend::registry::BackendVendor;

/// Everything the engine needs from this crate: the capability descriptor, the four
/// renderers, and the line-1 guard.
///
/// The renderer structs themselves are deliberately private. A caller reaches this
/// vendor's spelling through a registry or not at all, which is the property the
/// in-crate `match` used to give for free and which `pub` statics would have thrown
/// away at exactly the moment the vendor became separately linkable.
///
/// `value_format`, `validation`, `ddl`, `guard` and `advisor` are REQUIRED. Delete any line and this literal stops
/// compiling, here, with this crate named — which is the point: a backend cannot
/// inherit another backend's DDL, acquire a trusting guard, or acquire a silently
/// empty advisory report by omission. See
/// `zero_migrate_backend::registry::BackendVendor`.
pub static VENDOR: BackendVendor = BackendVendor {
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
