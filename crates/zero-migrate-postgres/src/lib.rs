//! # `zero-migrate-postgres` — the PostgreSQL backend
//!
//! One vendor, no engine. This crate holds PostgreSQL's DML, schema, DDL, and
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

mod ddl;
mod dml;
pub mod guard;
mod schema;
mod value_format;
mod vendor;

// `render_vendor_op` IS NOT RE-EXPORTED, and its absence is the enforcement.
//
// It used to be `pub use vendor::render_vendor_op`, because the engine called it
// directly at three sites covering sixteen op kinds that never reach
// `DmlRenderer::render_trigger_op`. That made the vendor-op surface the one part of
// a backend the engine knew by NAME rather than by contract, and both this file and
// `zero_migrate::render::vendor` said so in as many words.
//
// It is behind `DmlRenderer::render_vendor_op` now. `mod vendor` above is private,
// so with this re-export gone the function is UNREACHABLE from outside this crate:
// core naming it again is an E0603 privacy error at the use site, not a review
// comment and not a census finding. That is strictly stronger than the textual
// census in `tests/dialect_matrix/core_names_no_vendor_crate.rs`, which stays as the
// backstop for the couplings a privacy rule cannot express across a crate boundary.
//
// The function itself did not move and did not change. `crate::vendor` is the same
// module it was; what changed is who may ask for it.

/// This vendor's line-1 guard, re-exported because the engine's public API has
/// surfaced it since before the vendor crates existed.
pub use guard::PgGuard;

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
    descriptor: &zero_migrate_ir::backend::POSTGRES_DESCRIPTOR,
    dml: &dml::RENDERER,
    schema: &schema::RENDERER,
    value_format: &value_format::RENDERER,
    ddl: ddl::emitter,
    guard: guard::guard,
};
