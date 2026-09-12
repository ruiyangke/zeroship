//! Schema DDL, descriptor recovery and change classification for migrations.
//!
//! `query` lowers field definitions through registered vendor renderers. `diff`
//! classifies changes without executing them. Shared sentinel codecs and descriptor
//! vocabulary are provided by the backend contract crate.
//!
//! Runtime query planning belongs to `zeroship-data-sql`.

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
