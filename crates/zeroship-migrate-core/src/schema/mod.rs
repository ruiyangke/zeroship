//! # `schema` - the engine's schema-authority core (formerly the `zero-migrate-schema` crate)
//!
//! ONE schema implementation, consumed by the migration engine for
//! write / diff / generate. Dissolved into the engine:
//! the data-plane query language that used to ride along here - the find and
//! aggregate builders, the MongoDB-style filter->WHERE translator, the query
//! limits - had **zero engine callers** and lived here only for a consumer
//! (plugin-db) that is not in this repo, so it was deleted. The write / diff /
//! describe layer the engine actually uses became this module tree.
//!
//! ## What lives here (the *describe/shape* layer)
//!
//! - [`query`] - the DSL->SQL **DDL builders** (CREATE TABLE / index / FK /
//!   constraints; vector / geoPoint / encrypted-column / mask-sibling;
//!   policy-injected columns), the neutral field-definition
//!   lowering ([`query::def_to_column_type_for_dialect`]), the encryption + mask
//!   sentinel builders, and the identifier/field validators. Vendor type
//!   canonicalization is reached through
//!   [`query::SchemaRenderer::canonical_type`].
//! - `backends` - the schema-renderer view of the one shipping
//!   [`VendorSet`](zeroship_migrate_backend::registry::VendorSet)
//!   registry. Implementations live in the PostgreSQL, SQLite, and MySQL backend
//!   crates; core performs an open
//!   [`DialectId`](zeroship_migrate_ir::dialect::DialectId) lookup and no enum match
//!   over them.
//! - [`diff`] - the **diff classifier** ([`diff::compute_diff`],
//!   [`diff::ChangeKind`], [`diff::ChangeClass`]) and the schema **metadata
//!   types** ([`diff::MaskMeta`], [`diff::EncryptionMeta`], [`diff::MaskKind`],
//!   [`diff::Classification`], [`diff::WrappedType`], ...).
//! - [`mask_codec`] - the **sentinel CODEC** ([`mask_codec::build_mask_sentinel`]
//!   / [`mask_codec::parse_mask_sentinel`]).
//! - [`descriptors`] - the schema-shape **enums** ([`descriptors::VectorMetric`],
//!   [`descriptors::GeoPoint`]).
//! - [`error`] - leaf error types ([`error::MaskSentinelError`]).

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

pub(crate) mod backends;
pub mod diff;
pub mod query;

// -- Three leaf modules moved into `zeroship-migrate-backend` and re-exported under
// their historical `crate::schema::*` paths.
//
// `mask_codec` is the sentinel CODEC PostgreSQL's `SchemaRenderer::
// column_comment_statements` spells through, so a backend crate has to be able to
// name it; `descriptors` and `error` are the vocabulary `mask_codec` itself names.
// All three are pure data with no engine dependency, which is why they could go.
pub use zeroship_migrate_backend::descriptors;
pub use zeroship_migrate_backend::mask_codec;
pub use zeroship_migrate_backend::schema_error as error;
