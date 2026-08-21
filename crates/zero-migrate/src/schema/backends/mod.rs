//! The schema/DDL half of the backend registry.
//!
//! The sibling of [`crate::render::backends`], for the OTHER renderer registry. Read
//! that module's header first: it states the backend-boundary rule, the
//! one-dialect-literal rule, and the measurements behind both, and every word of it
//! applies here unchanged. This file records only what is different.
//!
//! | before                        | after                        |
//! |-------------------------------|------------------------------|
//! | `schema::query` (the trait)   | `zero_migrate_backend::schema` |
//! | `schema::backends::postgres`  | `zero-migrate-postgres`      |
//! | `schema::backends::sqlite`    | `zero-migrate-sqlite`        |
//! | `schema::backends::mysql`     | `zero-migrate-mysql`         |
//! | `schema::backends` (this)     | the registry composition     |
//!
//! # There is ONE vendor list, not two
//!
//! Both registries resolve through the same `render::backends::VENDORS`, because a
//! `BackendVendor` carries both renderers. That is a real change and it is the one
//! worth reading: before the split there were two independent exhaustive `match`
//! statements, so a vendor could in principle be present in one and absent from the
//! other. Now a backend crate ships both spellings or it does not ship, and
//! `render::backends`'s `every_vendor_agrees_with_its_own_descriptor` checks that the
//! `SchemaRenderer` in a vendor's entry answers with that vendor's own dialect.
//!
//! # The cross-stack edge, which the `renderer(` grep cannot see
//!
//! Each vendor's schema renderer explicitly delegates identifier spelling to its
//! own DML renderer sibling. There is no core dialect switch between them.
//!
//! That is deliberate and it must STAY one forwarder. The alternative — each vendor
//! spelling its own identifiers — would put a second physical home of the quoting
//! bytes back in the tree, which is exactly the defect `render::backends`'s header
//! measured and removed (125 and 39 disjoint red tests, two sets that could not see
//! each other). The crate split moved that seam; it did not fix it by copying the
//! escape, and it must not be.
//!
//! # A near-collision that must not be tidied
//!
//! `SchemaRenderer::current_timestamp_expr` and `DmlRenderer::synth_now` answer the
//! same-sounding question and are NOT the same function. SQLite spells both
//! `CURRENT_TIMESTAMP` and MySQL spells both `CURRENT_TIMESTAMP(6)`, so two of the
//! three vendors agree — and PostgreSQL does not: `NOW()` here, `now()` there.
//! Agreeing on two of three is precisely the shape that makes a fold look safe and
//! makes the divergence invisible to a reader skimming for duplication. Emitted bytes
//! are the contract; leave both. Now that the two spellings live in the SAME vendor
//! crate, one file apart, this warning matters more than it did.

use crate::schema::query::{SchemaRenderer, SqlDialect};

/// The schema renderer for a dialect.
///
/// Re-exported as `crate::schema::query::renderer`, the path every caller uses.
///
/// # Who is allowed to call this
///
/// It had eight callers and every one of them was a POINT-OF-USE lookup: an engine
/// emitter deep in a call chain, holding a `dialect: SqlDialect` parameter, asking
/// the registry for a vendor at the moment it needed one spelling. One `CREATE TABLE`
/// emit went through the registry five separate times for the same dialect.
///
/// That is now zero. `schema::query`'s private emitters take
/// `backend: &'static dyn SchemaRenderer` instead of `dialect: SqlDialect`, and the
/// resolution happens ONCE per entry point. What remains is only BOUNDARIES (the
/// `pub` surfaces whose callers hand in a dialect) and CALLER-FIXED TARGETS (the
/// functions that name PostgreSQL because they ARE PostgreSQL).
///
/// The distinction is what step 4 turned on, and it is now past tense: a boundary
/// resolution survived the move to per-vendor crates by becoming this registry
/// composition, while a point-of-use lookup could not — it is the engine reaching for
/// a vendor list it no longer has. Adding one back inside an emitter re-creates the
/// blocker.
pub fn renderer(dialect: SqlDialect) -> &'static dyn SchemaRenderer {
    crate::render::backends::schema_renderer(dialect)
}
