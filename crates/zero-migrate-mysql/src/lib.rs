//! # `zero-migrate-mysql` — the MySQL backend
//!
//! One vendor, no engine. This crate holds MySQL's DML, schema, DDL, and
//! value-format renderers plus its guard, and it
//! depends on `zero-migrate-backend` and `zero-migrate-ir` — never on the engine.
//! That is the whole point of the split: the engine names this crate for its
//! registry, so this crate must not name the engine back.
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

pub mod collation;
mod ddl;
mod descriptor;
mod dml;
mod existence_probe;
pub mod guard;
mod schema;
mod value_format;

pub use guard::MysqlGuard;

use zero_migrate_backend::registry::BackendVendor;

/// Everything the engine needs from this crate: the capability descriptor, the four
/// renderers, and the line-1 guard.
///
/// The renderer structs themselves are deliberately private. A caller reaches this
/// vendor's spelling through a registry or not at all, which is the property the
/// in-crate `match` used to give for free and which `pub` statics would have thrown
/// away at exactly the moment the vendor became separately linkable.
///
/// `value_format`, `ddl`, and `guard` are REQUIRED. Delete any line and this literal stops
/// compiling, here, with this crate named — which is the point: a backend cannot
/// inherit another backend's DDL or acquire a trusting guard by omission. See
/// `zero_migrate_backend::registry::BackendVendor`.
pub static VENDOR: BackendVendor = BackendVendor {
    descriptor: &descriptor::MYSQL_DESCRIPTOR,
    dml: &dml::RENDERER,
    schema: &schema::RENDERER,
    value_format: &value_format::RENDERER,
    existence_probe: &existence_probe::POLICY,
    ddl: ddl::emitter,
    guard: guard::guard,
};
