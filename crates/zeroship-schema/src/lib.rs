//! # zeroship-schema — the shared schema-authority core
//!
//! ONE schema implementation, reused by two consumers:
//!
//! - the **migration engine** (`zeroship-migrate`) — write / diff / generate
//!   (engine adoption is not yet wired up; this crate is currently
//!   engine-free);
//! - **plugin-db's data plane** — consumes the same sentinel codec + metadata
//!   types while its vendor tiers populate them from their own catalogs.
//!
//! That shared need is *why* this is a leaf crate rather than two files
//! moved into the engine: the data plane and the engine both depend on the
//! schema-description layer, so it must sit below both. See
//! `docs/archive/proposals/2026-06-18-schema-authority-drizzle-model-design.md` §5.
//!
//! ## What lives here (the *describe/shape* layer)
//!
//! - [`query`] — the DSL→SQL **DDL builders** (CREATE TABLE / index / FK /
//!   constraints; vector / geoPoint / encrypted-column / mask-sibling; the
//!   system-field columns; [`query::SqlDialect`]). Dual-dialect (PG + SQLite).
//! - [`diff`] — the **diff classifier** ([`diff::compute_diff`],
//!   [`diff::ChangeKind`], [`diff::ChangeClass`]) and the vendor-neutral schema
//!   **metadata types** ([`diff::MaskMeta`], [`diff::EncryptionMeta`],
//!   [`diff::MaskKind`], [`diff::Classification`], [`diff::WrappedType`],
//!   [`diff::LiveSchema`], [`diff::ColumnInfo`], …).
//! - [`mask_codec`] — the **sentinel CODEC** ([`mask_codec::build_mask_sentinel`]
//!   / [`mask_codec::parse_mask_sentinel`]). The contract between the schema
//!   layer (writes the sentinel into DDL) and the data plane (reads it back).
//! - [`descriptors`] — the schema-shape **enums** ([`descriptors::VectorMetric`],
//!   [`descriptors::EncryptionMode`]) + [`descriptors::GeoPoint`].
//! - [`error`] — the leaf-crate sentinel error
//!   ([`error::MaskSentinelError`]).
//! - [`schema_name`] — [`SchemaName`], the validated physical schema identity.
//!   Every builder and DDL emitter above takes it rather than a `&str`, so a
//!   tenant id cannot reach a parameter that wants a schema.
//!
//! ## What does NOT live here (the *transform* layer — stays in plugin-db)
//!
//! Vendor catalog introspection, AEAD encrypt/decrypt, the mask read-pass,
//! CRUD / transactions / `SET LOCAL ROLE`, and metering. Those are data-plane or
//! vendor-tier concerns; they call *into* this crate for neutral schema values,
//! DDL, diffing, and codecs.
//!
//! ## Leaf purity
//!
//! **No database driver, v8, `zeroship-runtime`, crypto (aes/hkdf/hmac), or
//! `zeroship-metering`.** Nothing in this crate can touch a database connection,
//! a key, an isolate, or a usage counter.

// **Inherited lint posture.** `query.rs` and `diff.rs`
// were relocated *verbatim* out of `zeroship-plugin-db` (a pure refactor: the
// logic is byte-for-byte unchanged). plugin-db was authored under a regime
// where these specific `clippy::all` style lints were not enforced, so the
// moved code trips them here. Suppressing them at the crate level keeps this
// relocation faithful — tightening the moved code's style is a
// separate cleanup pass, NOT part of extracting the crate (editing the moved
// logic to satisfy a style lint would dilute the "behaviour-identical"
// guarantee this refactor is judged on). The list is exactly the deny-level
// lints the verbatim move trips; nothing broader is silenced.
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
pub mod ident;
pub mod mask_codec;
pub mod query;
pub mod schema_name;

pub use schema_name::SchemaName;
