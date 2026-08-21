//! # `zero-migrate-backend` — the backend CONTRACT
//!
//! The crate `zero_migrate::render::renderer` and `zero_migrate::schema::backends`
//! have both called "the future `zero-migrate-backend`" in their headers since the
//! in-crate backend modules were written. This is it.
//!
//! It holds the three per-vendor TRAITS, the vocabulary their signatures name, and
//! the registry shape a vendor crate hands back. It deliberately holds no vendor:
//! nothing here spells a keyword, quotes an identifier or names a dialect except
//! `SqlDialect`, which is a wire-level target descriptor from `zero-migrate-ir`.
//!
//! | trait | question it answers |
//! |---|---|
//! | [`renderer::DmlRenderer`] | how does this vendor spell DML, views and triggers |
//! | [`schema::SchemaRenderer`] | how does this vendor spell columns and DDL |
//! | [`guard::MigrationGuard`] | what does this vendor REFUSE to run |
//!
//! The third arrived last, for the same reason as the first two: a guard declared in
//! the engine would force every vendor to depend on the engine, which already depends
//! on every vendor. See [`guard`] for what moved with it and what measurably did not.
//!
//! ```text
//!   zero-migrate-policy ─┐
//!                        ├─> zero-migrate-ir ──> zero-migrate-backend ──┬─> zero-migrate-postgres ─┐
//!                        │                                              ├─> zero-migrate-sqlite   ─┼─> zero-migrate
//!                        └──────────────────────────────────────────────┴─> zero-migrate-mysql    ─┘
//! ```
//!
//! # Why the traits are HERE and not in `zero-migrate-ir`
//!
//! `zero-migrate-ir` is the WIRE CONTRACT: `MigrationIr`, the closed `Op` enum, the
//! closed `Expr` AST, the canonical checksum, the structural validator. Its own
//! manifest calls it "pure data, zero I/O". This crate is 6,000-odd lines of SQL
//! RENDERING — an expression-to-SQL lowerer, a PostgreSQL vendor-DDL renderer, an
//! identifier-quoting seam. Putting that in `-ir` would make every consumer that
//! wants to checksum an envelope compile a SQL renderer, and it would erase the one
//! distinction the two crates exist to keep: what a migration SAYS versus how a
//! vendor WRITES it.
//!
//! The split is also what the `-ir` half already assumes. `DialectId`, `Capability`,
//! `BackendDescriptor` and `BackendRegistry` were promoted into `-ir` because they
//! are IDENTITY and CAPABILITY — facts about a backend that a checksummer or a
//! policy engine legitimately reads. `DmlRenderer` and `SchemaRenderer` are neither;
//! they are the spelling.
//!
//! # What had to come with the traits, and the measurement that bounded it
//!
//! A trait cannot move without the types in its signatures. The transitive closure of
//! the three DML vendor modules over the engine, measured by walking `crate::`
//! references at module granularity with `#[cfg(test)]` stripped, was **54 modules /
//! 113,216 lines** — the whole engine, in effect, because
//! [`error::IrLowerError`] sat in the 16,868-line `render::lower`, which reaches
//! `engine`, `apply::*`, `model::validate` and `render::fold`.
//!
//! Moving four things collapses it to **7 modules / 7,733 lines**:
//! [`error::IrLowerError`], [`error::DeclarativeError`], [`step::BindValue`] and
//! `SPLIT_PART_MAX_N` (which was already a re-export of
//! `zero_migrate_ir::validate::SPLIT_PART_MAX_N` and needed only to be named at its
//! real home). Every other apparent edge dissolved on inspection: `crate::model::ir`,
//! `crate::model::expr` and `crate::schema::query::SqlDialect` are `-ir` re-exports,
//! and `BackfillSpec` / `PlanStep` appeared in doc links only.
//!
//! # What is NOT here
//!
//! The engine's `render::lower`, `render::declarative`, `render::fold`,
//! `render::value_format` and the bulk of `schema::query`. All of them read the
//! dialect, and none of them is a spelling: they COMPARE, NORMALIZE and DECIDE, which
//! stays in the engine, dialect-parameterized. The test is the direction of the arrow
//! — spelling is the engine ASKING a vendor how to write something; semantics is the
//! engine DECIDING something about a vendor.

pub mod advisory;
pub mod descriptors;
pub mod dml;
pub mod error;
pub mod guard;
pub mod mask_codec;
pub mod mask_meta;
pub mod registry;
pub mod renderer;
pub mod schema;
pub mod schema_error;
pub mod snapshot;
pub mod spelling;
pub mod step;
pub mod vendor;
