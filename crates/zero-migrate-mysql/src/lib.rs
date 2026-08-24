//! # `zero-migrate-mysql` — the MySQL backend
//!
//! One vendor, no engine. This crate holds BOTH halves of MySQL now:
//!
//! * the RENDER half — DML, schema, DDL and value-format renderers plus the line-1
//!   guard, all registered through [`VENDOR`]; and
//! * the EXECUTION half — [`backend`], the `MigrationBackend` implementation: the
//!   `GET_LOCK` project lock, the session pins, the two-phase non-transactional
//!   apply MySQL's auto-committing DDL forces, the journal DDL and net-state reads,
//!   the `information_schema` drift snapshot, and the resumable backfill.
//!
//! It depends on `zero-migrate-backend` and `zero-migrate-ir` — never on the engine.
//! That is the whole point of the split: the engine names this crate for its
//! registry, so this crate must not name the engine back.
//!
//! # What that cost, and what it bought
//!
//! The execution half was the last vendor backend inside the engine, and moving it
//! is what the governing rule — the core is neutral, and that is the hard limit —
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
//! `zero_migrate_backend::value_format` and takes its renderers as parameters.
//!
//! # The one-dialect-literal rule
//!
//! Each module names its own dialect exactly ONCE, as its `DIALECT` const, and names
//! no other dialect at all. Everything else reads `DIALECT`. The rule predates the
//! extraction and it is what made the extraction mechanical; it is ENFORCED, across
//! the crate boundary now, by
//! `zero-migrate/tests/dialect_matrix/backend_modules_name_one_dialect.rs`.
//!
//! # What the rule does NOT catch
//!
//! A backend can still reach another vendor's spelling THROUGH a contract helper
//! that hard-codes a dialect, and no grep of this crate can see it because the
//! literal lives in `zero-migrate-backend`. That is measured, not hypothetical —
//! `zero_migrate_backend::dml`'s header carries the numbers. The identifier seam
//! (`*_for_dialect(.., DIALECT)`) is how this crate stays clear of it.

mod advisory;
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
// leg — it cannot read one without naming this type, which is the point.
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
/// through the REAL `zero_migrate_ir::policy_registry` rather than restating the
/// algebra, so what a vendor's tests compose and what production composes cannot
/// drift.
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
