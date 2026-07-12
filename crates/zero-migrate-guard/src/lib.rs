//! `zero-migrate-guard` — the `pg_query`(libpg_query)-backed SQL security layer.
//!
//! This crate is the security heart of the migration engine, extracted into its
//! own crate so it owns the ONLY C dependency on the non-SQLite path
//! (`pg_query` → libpg_query). It parses every migration statement with the real
//! Postgres parser and enforces the deny-list, the cross-schema confinement, and
//! the operational advisories — regardless of the submitted SQL.
//!
//! # Layering
//!
//! - Depends on [`zero_migrate_ir`] (the wire contract: `Op`, `MigrationIr`, the
//!   policy vocabulary `TrustProfile`/`SchemaScope`/`VendorCapabilities`/
//!   `OperatorCapability`/`DestructiveOps`) and [`zero_migrate_schema`] (the
//!   `SqlDialect` deploy-target enum).
//! - Does **not** depend on the engine (`zero-migrate`). The engine depends on
//!   THIS crate and re-exports the surface it needs (`SqlGuard`, `GuardConfig`,
//!   `analyze`, `classify`, `Advisory`, `Severity`, …).
//!
//! # Two lines of defense
//!
//! This is line 1 (the parse-time deny-list). The least-privilege `migrator`
//! role provisioned by the engine is line 2 (the DB rejects the same ops even if
//! SQL slips past parse). This crate owns line 1 in full.

pub mod analysis;
pub mod guard;

pub use analysis::{analyze, classify};
