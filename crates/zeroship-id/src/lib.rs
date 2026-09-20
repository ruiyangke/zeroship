//! The platform's entity-id vocabulary.
//!
//! Every entity this platform names carries a typed id: a three-or-four letter
//! prefix, an underscore, and base36 of a UUIDv7 - `app_…`, `usr_…`, `org_…`,
//! `prj_…`, `ivt_…`. [`typed_id`] is the codec, [`entity_id::declare_entity_id`]
//! is the one way a type is declared, and each id module is that macro plus the
//! prose saying what the entity is.
//!
//! # Why this is its own crate
//!
//! An id is named by things that sit at very different heights: the control
//! plane, the gateway, the worker, the deploy artifact, the CDC wire format. A
//! crate that owns wire types AND an HTTP client AND AEAD keys cannot be
//! depended on by an artifact-format crate, so an id living there forces every
//! crate below it to invent its own - and two spellings of one identity is how a
//! blob prefix, a schema name or a replication slot silently disagrees about
//! which tenant it belongs to.
//!
//! So the vocabulary is a leaf. It carries `uuid` and `serde` and nothing else:
//! no database driver, no v8, no runtime, no crypto, no HTTP. Anything that
//! needs an id can depend on this, which is the property that lets there be
//! exactly one `AppId` in the tree.
//!
//! # What an id deliberately cannot do
//!
//! The macro exposes minting, validated borrowed and owned text conversions,
//! serde, `Ord` and `Hash`, and
//! NOTHING else: no `Display`, no `AsRef<str>`, no `From<&str>`, no inherent
//! `as_bytes`. Read [`entity_id`] for what each absence prevents - each one is
//! asserted by a test the macro generates for every id that uses it.
//!
//! # The collation contract
//!
//! Base36 as spelled here is ascending in byte value, so a bytewise comparison
//! puts ids in creation order. Every database column holding one therefore owes
//! `COLLATE "C"`, including the foreign-key copies nothing orders: a join
//! against the collated id cannot use a copy's index when the two collations
//! differ, and that degrades silently rather than erroring.

pub mod app_id;
pub mod binding_id;
pub mod database_id;
pub mod datastore_id;
pub mod deploy_command;
pub mod entity_id;
pub mod invite_id;
pub mod organization_id;
pub mod project_id;
pub mod typed_id;
pub mod user_id;
pub mod workflow;

pub use app_id::AppId;
pub use binding_id::BindingId;
pub use database_id::DatabaseId;
pub use datastore_id::DatastoreId;
pub use deploy_command::DeployCommandId;
pub use invite_id::InviteId;
pub use organization_id::OrganizationId;
pub use project_id::ProjectId;
pub use user_id::UserId;
