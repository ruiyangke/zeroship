//! # `zero-migrate-postgres` — the PostgreSQL backend
//!
//! One vendor, two spellings, no engine. This crate holds PostgreSQL's
//! `DmlRenderer` impl (`dml`) and its `SchemaRenderer` impl (`schema`), and it
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

mod dml;
mod schema;
mod vendor;

/// The privileged PostgreSQL vendor ops (roles, grants, policies, functions,
/// triggers, RLS, `pgRaw`) lowered to structured DDL.
///
/// PUBLIC because the engine calls it directly at three sites covering sixteen op
/// kinds that never reach `DmlRenderer::render_trigger_op` — so unlike the two
/// renderers, this one is not reachable through the registry and has to be named.
/// That asymmetry is real and is worth stating: the vendor-op surface is the one
/// part of a backend the engine still knows by name rather than by contract.
pub use vendor::render_vendor_op;

use zero_migrate_backend::registry::BackendVendor;

/// Everything the engine needs from this crate: the capability descriptor and the
/// two renderers.
///
/// The renderer structs themselves are deliberately private. A caller reaches this
/// vendor's spelling through a registry or not at all, which is the property the
/// in-crate `match` used to give for free and which `pub` statics would have thrown
/// away at exactly the moment the vendor became separately linkable.
pub static VENDOR: BackendVendor = BackendVendor {
    descriptor: &zero_migrate_ir::backend::POSTGRES_DESCRIPTOR,
    dml: &dml::RENDERER,
    schema: &schema::RENDERER,
};
