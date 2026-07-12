//! # zero-migrate-schema — the shared schema-authority core
//!
//! ONE schema implementation, reused by two consumers:
//!
//! - the **migration engine** (`zero-migrate`) — write / diff / generate
//!   (adopted in P2; this crate is engine-free in P1);
//! - **a data plane** — read / introspect, which needs the same
//!   sentinel codec + metadata types to learn column behaviour at runtime.
//!
//! That shared need is *why* this is a leaf crate rather than two files
//! moved into the engine: the data plane and the engine both depend on the
//! schema-description layer, so it must sit below both. See
//! `docs/proposals/2026-06-18-schema-authority-drizzle-model-design.md` §5.
//!
//! ## What lives here (the *describe/shape* layer)
//!
//! - [`query`] — the DSL→SQL **DDL builders** (CREATE TABLE / index / FK /
//!   constraints; vector / geoPoint / encrypted-column / mask-sibling; the
//!   system-field columns; [`query::SqlDialect`]). Dual-dialect (PG + SQLite).
//! - [`diff`] — the **diff classifier** ([`diff::compute_diff`],
//!   [`diff::ChangeKind`], [`diff::ChangeClass`]), the live **introspection**
//!   ([`diff::read_live_schema`], [`diff::estimate_row_count`]), and the
//!   schema **metadata types** ([`diff::MaskMeta`], [`diff::EncryptionMeta`],
//!   [`diff::MaskKind`], [`diff::Classification`], [`diff::WrappedType`],
//!   [`diff::LiveSchema`], [`diff::ColumnInfo`], …).
//! - [`mask_codec`] — the **sentinel CODEC** ([`mask_codec::build_mask_sentinel`]
//!   / [`mask_codec::parse_mask_sentinel`]). The contract between the schema
//!   layer (writes the sentinel into DDL) and the data plane (reads it back).
//! - [`descriptors`] — the schema-shape **enums** ([`descriptors::VectorMetric`],
//!   [`descriptors::EncryptionMode`]) + [`descriptors::GeoPoint`].
//! - [`error`] — leaf-crate error types ([`error::SchemaError`],
//!   [`error::MaskSentinelError`]) that this crate returns instead of
//!   plugin-db's runtime-coupled `DbError`.
//!
//! ## What does NOT live here (the *transform* layer — stays in plugin-db)
//!
//! AEAD encrypt/decrypt, the mask read-pass, the backfill *runner*
//! (`run_mask_backfill` / `run_mask_rewrite`), CRUD / transactions /
//! `SET LOCAL ROLE` / metering, and `register_model`. Those are the data
//! plane; they call *into* this crate for any DDL / diff / introspection /
//! codec they need.
//!
//! ## Leaf purity
//!
//! Deps: `serde_json` + `sha2` (always-on) + — behind the `introspect`
//! feature only — `compio-postgres` + the `tracing` observability facade.
//! The DEFAULT (write/diff/describe) profile pulls NO PG driver and no
//! tokio chain: the migration engine
//! consumes exactly that profile (it does its own introspection over the
//! `PgSession` seam and never calls `read_live_schema`). A data-plane consumer
//! enables `introspect` because it reads live column behaviour at runtime.
//! **No v8, no runtime coupling, no crypto (aes/hkdf/hmac), no
//! metering.** The trust domain is preserved: nothing in this crate
//! can touch a key, an isolate, or a usage counter.

// **Schema-authority P1 — inherited lint posture.** `query.rs` and `diff.rs`
// were relocated *verbatim* out of the original data-plane crate (a pure refactor: the
// logic is byte-for-byte unchanged). plugin-db was authored under a regime
// where these specific `clippy::all` style lints were not enforced, so the
// moved code trips them here. Suppressing them at the crate level keeps this
// phase a faithful relocation — tightening the moved code's style is a
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
pub mod fts_sqlite;
pub mod mask_codec;
pub mod query;
