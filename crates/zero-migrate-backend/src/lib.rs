//! # `zero-migrate-backend` — the backend CONTRACT
//!
//! The crate `zero_migrate::render::renderer` and `zero_migrate::schema::backends`
//! have both called "the future `zero-migrate-backend`" in their headers since the
//! in-crate backend modules were written. This is it.
//!
//! It holds the per-vendor TRAITS, the vocabulary their signatures name, and
//! the registry shape a vendor crate hands back. It deliberately holds no vendor:
//! nothing here spells a keyword, quotes an identifier or names a dialect. Its
//! wire-level target identity is the open `DialectId` from `zero-migrate-ir`.
//!
//! | trait | question it answers |
//! |---|---|
//! | [`renderer::DmlRenderer`] | how does this vendor spell DML, views and triggers |
//! | [`schema::SchemaRenderer`] | how does this vendor spell column types and collations |
//! | [`ddl::DdlEmitter`] | how does this vendor spell schema-changing statements |
//! | [`fold::CatalogFoldPolicy`] | how does this vendor shape shared catalog replay |
//! | [`existence_probe::ExistenceProbePolicy`] | how do this vendor's catalog identities behave under guarded probes |
//! | [`guard::MigrationGuard`] | what does this vendor REFUSE to run |
//! | [`stored_ddl::StoredDdl`] | how does this vendor parse catalog-stored table DDL |
//! | [`value_format::ValueFormatRenderer`] | how does this vendor render and normalize ID formats |
//!
//! The same dependency rule governs all eight: a trait declared in the engine would
//! force every vendor to depend on the engine, which already depends on every vendor.
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
//! policy engine legitimately reads. The renderer/parser traits are neither; they
//! are backend-owned spelling and normalization.
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
//! Moving three things collapses it to **7 modules / 7,733 lines**:
//! [`error::IrLowerError`], [`error::DeclarativeError`] and [`step::BindValue`].
//! Every other apparent edge dissolved on inspection: `crate::model::ir`,
//! `crate::model::expr` and the former `crate::schema::query` dialect identity were
//! `-ir` re-exports, and `BackfillSpec` / `PlanStep` appeared in doc links only.
//!
//! # What is NOT here
//!
//! The engine's `render::lower`, `render::declarative`, `render::fold`, and the bulk
//! of `schema::query`. The engine still composes comparisons and decisions; the
//! backend contract supplies every vendor-specific spelling and catalog-normalization
//! fact those algorithms consume.

pub mod advisory;
pub mod ddl;
pub mod descriptors;
pub mod dml;
// The dialect-neutral network driver seam (`SqlSession`) and its conformance
// suite. A CONTRACT with no vendor in it: `std` is its only dependency, it
// spells no keyword and names no dialect, and the network backends are generic
// over it. The engine re-exports it at `zero_migrate::driver`.
pub mod driver;
pub mod error;
pub mod existence_probe;
pub mod fold;
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
pub mod stored_ddl;
pub mod table_rebuild;
pub mod validation;
pub mod value_format;
pub mod vendor;
