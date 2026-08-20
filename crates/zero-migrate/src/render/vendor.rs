//! The engine's view of the VENDOR render seam, which the crate split cut in two.
//!
//! [`VendorStatement`] and [`VendorError`] are CONTRACT vocabulary and live in
//! `zero-migrate-backend`: the first is the return type of
//! `DmlRenderer::render_trigger_op`, which all three vendors implement and which
//! SQLite and MySQL each construct, and the second is a `#[from]` variant of
//! `IrLowerError`. Neither could move into `zero-migrate-postgres` without making
//! the other two vendors depend on PostgreSQL to name their own return type.
//!
//! [`render_vendor_op`] is the other ~720 lines of that file, and every one of them
//! is PostgreSQL SPELLING — `CREATE ROLE`, `GRANT`, `CREATE POLICY`, dollar-quoted
//! function bodies, `ALTER TABLE … ENABLE ROW LEVEL SECURITY`. It moved to
//! `zero-migrate-postgres`, where the boundary rule puts a vendor's spelling.
//!
//! # The asymmetry worth naming
//!
//! The engine reaches `render_vendor_op` BY NAME rather than through a registry, at
//! three sites covering sixteen op kinds that never touch `DmlRenderer`. So while
//! the two renderers are fully behind the contract, the vendor-op surface is not:
//! `zero_migrate::render::lower` still knows there is a crate called
//! `zero-migrate-postgres`. That is honest rather than hidden — every vendor op is
//! `dialect_scope = PgOnly` and the lower seam refuses a non-PostgreSQL target before
//! reaching here, so there is nothing for a registry to dispatch on. It is recorded
//! because a fourth backend WOULD have to answer it, and this is where the question
//! lives.

pub use zero_migrate_backend::vendor::{VendorError, VendorStatement};
pub use zero_migrate_postgres::render_vendor_op;
