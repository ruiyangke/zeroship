//! # `schema` — the engine's schema-authority core (formerly the `zero-migrate-schema` crate)
//!
//! ONE schema implementation, consumed by the migration engine for
//! write / diff / generate. Dissolved into the engine:
//! the data-plane query language that used to ride along here (`build_find`,
//! `build_aggregate`, the MongoDB-style filter→WHERE translator, the query
//! limits) had **zero engine callers** and lived here only for a consumer
//! (plugin-db) that is not in this repo, so it was deleted. The write / diff /
//! describe layer the engine actually uses became this module tree.
//!
//! ## What lives here (the *describe/shape* layer)
//!
//! - [`query`] — the DSL→SQL **DDL builders** (CREATE TABLE / index / FK /
//!   constraints; vector / geoPoint / encrypted-column / mask-sibling;
//!   policy-injected columns; [`query::SqlDialect`]), the type map
//!   ([`query::def_to_column_type_for_dialect`], [`query::sqlite_canonical_type`],
//!   [`query::mysql_canonical_type`]), the encryption + mask sentinel
//!   builders, and the identifier/field validators. Dual-dialect (PG + SQLite,
//!   with a MySQL render leg).
//! - [`diff`] — the **diff classifier** ([`diff::compute_diff`],
//!   [`diff::ChangeKind`], [`diff::ChangeClass`]) and the schema **metadata
//!   types** ([`diff::MaskMeta`], [`diff::EncryptionMeta`], [`diff::MaskKind`],
//!   [`diff::Classification`], [`diff::WrappedType`], …).
//! - [`mask_codec`] — the **sentinel CODEC** ([`mask_codec::build_mask_sentinel`]
//!   / [`mask_codec::parse_mask_sentinel`]).
//! - [`descriptors`] — the schema-shape **enums** ([`descriptors::VectorMetric`],
//!   [`descriptors::EncryptionMode`]) + [`descriptors::GeoPoint`].
//! - [`fts_sqlite`] — the SQLite full-text-search vtable helpers.
//! - [`error`] — leaf error types ([`error::SchemaError`],
//!   [`error::MaskSentinelError`]).

// **Inherited lint posture.** `query.rs` and `diff.rs` were relocated verbatim
// out of the original data-plane crate; the moved code trips a handful of style
// lints this workspace enforces. Scoped to the schema module tree.
#![allow(
    clippy::collapsible_if,
    clippy::doc_lazy_continuation,
    clippy::doc_overindented_list_items,
    clippy::manual_map,
    clippy::needless_range_loop,
    clippy::too_many_arguments
)]

pub mod descriptors;
pub mod diff;
pub mod error;
pub mod fts_sqlite;
pub mod mask_codec;
pub mod query;
