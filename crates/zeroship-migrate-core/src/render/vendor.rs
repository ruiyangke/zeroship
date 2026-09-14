//! The engine's view of the VENDOR render seam, which the crate split cut in two.
//!
//! [`VendorStatement`] and [`VendorError`] are CONTRACT vocabulary and live in
//! `zeroship-migrate-backend`: the first is the return type of
//! `DmlRenderer::render_trigger_op` and `DmlRenderer::render_vendor_op`, which all
//! three vendors implement and which SQLite and MySQL each construct, and the second
//! is a `#[from]` variant of `IrLowerError`. Neither could move into
//! `zeroship-migrate-postgres` without making the other two vendors depend on PostgreSQL
//! to name their own return type.
//!
//! That is now ALL this module is: two names the engine re-exports as its public
//! vocabulary for the seam. It carries no vendor.
//!
//! # The asymmetry that used to be recorded here, and how it closed
//!
//! This file used to say: *"The engine reaches `render_vendor_op` BY NAME rather than
//! through a registry, at three sites covering sixteen op kinds that never touch
//! `DmlRenderer`. So while the two renderers are fully behind the contract, the
//! vendor-op surface is not."* It re-exported
//! `zeroship_migrate_postgres::render_vendor_op` to do it, and
//! `zeroship-migrate-sqlite/src/dml.rs` recorded the mirror image - PostgreSQL being
//! "still in the position SQLite just left, via `render::vendor`".
//!
//! Both notes are now discharged. The surface is
//! [`DmlRenderer::render_vendor_op`](zeroship_migrate_backend::renderer::DmlRenderer::render_vendor_op),
//! answered by each vendor crate, and `render::lower` asks the backend it already
//! resolved - `self.backend` at the lowering seam, and a threaded `backend` parameter
//! in `vendor_inverse_from_history`, which is what its sibling
//! `trigger_inverse_from_history` had always done.
//!
//! # Nothing moved to close it
//!
//! Worth stating because the size of the thing suggests otherwise: the PostgreSQL
//! spelling - `CREATE ROLE`, `GRANT`, `CREATE POLICY`, dollar-quoted function bodies,
//! `ALTER TABLE ... ENABLE ROW LEVEL SECURITY` - was ALREADY in
//! `zeroship-migrate-postgres`, and it was not touched. This was a routing change: a
//! handful of call sites and some small `impl` blocks. Byte-identical output, which is the
//! bar a move has to clear.
//!
//! # What enforces it, and why it is stronger than the census next door
//!
//! `mod vendor` is PRIVATE in `zeroship-migrate-postgres` and the `pub use` at that
//! crate's root is gone, so `render_vendor_op` is unreachable from this crate: naming
//! it again is an E0603 privacy error at the use site. That is a compiler-enforced
//! boundary, not a convention.
//!
//! It is worth being precise about why that was available here when it was not
//! available for the spelling primitives. The difference is direction.
//! `ansi_double_quote_ident` has to be reachable by the vendor crates, and
//! `pub(in ...)` cannot say "these three crates and no other", so it had to become
//! `pub`. `render_vendor_op` only ever needs to be reachable by PostgreSQL ITSELF -
//! it is one crate's own item, and one crate's own privacy still works. A rule that
//! can be a privacy should be one; the textual census in
//! `crates/zeroship-migrate/tests/dialect_matrix/core_names_no_vendor_crate.rs` is the backstop for the rest.
//!
//! # The behaviour test the census cannot be
//!
//! `crates/zeroship-migrate/tests/dialect_matrix/vendor_ops_dispatch_per_vendor.rs` asserts that exactly ONE
//! shipping vendor renders a vendor op and the other two refuse. A census proves core
//! does not NAME `zeroship_migrate_postgres`; it cannot prove the dispatch is real, and a
//! refactor that routed all three vendors to PostgreSQL's renderer would satisfy a
//! census completely. That is not hypothetical in this tree - it is the shape of the
//! SQLite-identifiers-quoted-by-PostgreSQL defect, which compiled clean and passed
//! every emitted-SQL assertion because the two vendors agreed on the bytes. Here they
//! do not agree: two of them have no answer at all, and that disagreement is what
//! makes it testable.

pub use zeroship_migrate_backend::vendor::{VendorError, VendorStatement};
