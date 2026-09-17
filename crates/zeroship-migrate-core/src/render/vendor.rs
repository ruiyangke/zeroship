//! The engine's view of the VENDOR render seam.
//!
//! [`VendorStatement`] and [`VendorError`] are CONTRACT vocabulary and live in
//! `zeroship-migrate-backend`: the first is the return type of
//! `DmlRenderer::render_trigger_op` and `DmlRenderer::render_vendor_op`, which all
//! three vendors implement and which SQLite and MySQL each construct, and the second
//! is a `#[from]` variant of `IrLowerError`. Neither could move into
//! `zeroship-migrate-postgres` without making the other two vendors depend on PostgreSQL
//! to name their own return type.
//!
//! This module is two names the engine re-exports as its public vocabulary for the
//! seam. It carries no vendor.
//!
//! # What enforces it, and why it is stronger than a census
//!
//! `mod vendor` is PRIVATE in `zeroship-migrate-postgres` and the `pub use` at that
//! crate's root is gone, so `render_vendor_op` is unreachable from this crate: naming
//! it again is an E0603 privacy error at the use site. That is a compiler-enforced
//! boundary, not a convention.
//!
//! Privacy works here because it can express the rule. `render_vendor_op` only ever
//! needs to be reachable by PostgreSQL itself - it is one crate's own item, and one
//! crate's own privacy still works. A spelling primitive such as
//! `ansi_double_quote_ident` has to be reachable by the vendor crates, and
//! `pub(in ...)` cannot say "these three crates and no other", so it has to be `pub`.
//! A rule that can be expressed through privacy should use privacy.
//!
//! # The behaviour test the census cannot be
//!
//! `crates/zeroship-migrate/tests/dialect_matrix/vendor_ops_dispatch_per_vendor.rs` asserts that exactly ONE
//! shipping vendor renders a vendor op and the other two refuse. A census proves core
//! does not NAME `zeroship_migrate_postgres`; it cannot prove the dispatch is real, and a
//! refactor that routed all three vendors to PostgreSQL's renderer would satisfy a
//! census completely. Here the vendors do not agree: two of them have no answer at
//! all, and that disagreement is what makes it testable.

pub use zeroship_migrate_backend::vendor::{VendorError, VendorStatement};
