//! `zero-migrate-ir` — the zero-migrate WIRE CONTRACT.
//!
//! The pure-data core the whole engine (and any external validator/checksummer)
//! agrees on: the migration document, the closed `op.*` IR, the closed expression
//! AST, the constrained numeric scalar, the precondition vocabulary, the
//! typed-id machinery, the canonical checksum, the fail-closed IR envelope load
//! gate (the policy-free half), and the STRUCTURAL (allow-list) validator.
//!
//! # A true leaf
//!
//! This crate is deliberately I/O-free and dependency-thin: no `compio`, no
//! `pg_query`, no database driver, and — crucially — **no dependency on the engine
//! (`zero-migrate`) or the schema layer.** It is the bottom of the crate graph.
//! The engine re-exports every type here (`pub use zero_migrate_ir as ir;` plus
//! the flattened re-exports at the engine root), so downstream code keeps naming
//! `zero_migrate::MigrationIr`, `zero_migrate::Op`, etc. unchanged.
//!
//! # What is NOT here (and why)
//!
//! - **Dialect-support / vendor-capability computation** for an [`ir::Op`] reads
//!   the engine-owned portability tables and lives in
//!   `zero_migrate::model::op_support` (free functions over `&Op`).
//! - **The POLICY validator** (`validate_ir`/`validate_op`, the vendor-capability
//!   gate, the raw-view-body `pg_query` scan) threads an explicit composed policy
//!   and schema scope and stays engine-side in `zero_migrate::model::validate`.
//!   Only the STRUCTURAL allow-list walk lives here in [`validate`].
//! - **`load_ir_document`** (the full fail-closed load gate) calls the POLICY
//!   validator, so it lives engine-side; the policy-free ownership/checksum
//!   helpers of the load gate stay here in [`load`].

pub mod capability;
pub mod dialect;
pub mod expr;
pub mod id;
pub mod ir;
pub mod load;
pub mod migration;
pub mod policy;
pub mod policy_approval;
pub mod policy_capability;
pub mod policy_registry;
pub mod precondition;
pub mod probe;
pub mod validate;
