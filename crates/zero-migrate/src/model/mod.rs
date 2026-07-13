pub mod backfill;
pub mod dialect_table;
// The fail-closed `.ir.json` load gate — POLICY-bound half (`load_ir_document`);
// the policy-free half is re-exported from `zero_migrate_ir::load`.
pub mod load;
// Engine-side dialect-support + vendor-capability computation for the closed `Op`
// wire type (the logic that could not ride the `Op` type into the leaf crate).
pub mod op_support;
pub mod profile;
pub mod snapshot;
pub mod support;
pub mod table_shape;
// The POLICY validator (`validate_ir`/`validate_op`, the vendor-capability gate,
// the raw-view-body `pg_query` scan). It re-exports the STRUCTURAL validator from
// `zero_migrate_ir::validate`, so `crate::model::validate::{Dialect, AuthoringError,
// validate_expr, CODE_*, …}` still resolve.
pub mod validate;

// ── The wire contract lives in the `zero-migrate-ir` leaf crate. Re-export its
// modules under their historical `crate::model::*` paths so the
// engine's ~hundreds of `crate::model::{ir,expr,migration,precondition,probe}::…`
// references (and the flattened root re-exports) keep resolving unchanged.
pub use zero_migrate_ir::capability;
pub use zero_migrate_ir::expr;
pub use zero_migrate_ir::ir;
pub use zero_migrate_ir::migration;
pub use zero_migrate_ir::policy;
pub use zero_migrate_ir::precondition;
pub use zero_migrate_ir::probe;
