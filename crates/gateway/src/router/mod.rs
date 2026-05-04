//! Gateway router — manifest dispatch, auth, CORS, idempotency,
//! conditional/range/variant negotiation, and static-asset serving.
//!
//! This module was split out of a single 4,612-line `router.rs` in
//! May 2026; the historical commit log for the pre-split file is
//! available via `git log --follow crates/gateway/src/router/mod.rs`.
//! Files cut OUT into their own files (`dispatch.rs`, `static_serve.rs`,
//! …) appear as new files in `git log`; for their pre-split history
//! follow `mod.rs`.
//!
//! Submodules:
//!
//! * [`dispatch`]      — request entry, resource-tree dispatch, idempotency,
//!                       worker forwarding, subscription dispatch.
//! * [`auth`]          — auth gate + JWT subject / session cookie parsers.
//! * [`static_serve`]  — three-tier blob fetch + buffered/streaming
//!                       response building.
//! * [`streaming`]     — chunk readers + the `STREAM_*` consts.
//! * [`cors`]          — preflight + actual-response header injection.
//! * [`conditional`]   — `If-None-Match` matching and `Range:` parsing.
//! * [`variants`]      — `Accept-Encoding` negotiation.
//! * [`helpers`]       — small framework-thin utilities.
//!
//! Each submodule owns its own `#[cfg(test)] mod tests` for the
//! private items it exposes; integration-shaped tests that exercise
//! `serve_static_hit` end-to-end live alongside the production code
//! in `static_serve.rs`.

pub mod auth;
pub mod conditional;
pub mod cors;
pub mod dispatch;
pub mod helpers;
pub mod static_serve;
pub mod streaming;
pub mod variants;

// Re-exports for external callers (`main.rs` registers `handle` and
// `handle_subdomain` as the gateway's two ntex routes).
pub use dispatch::{handle, handle_subdomain};

// Re-exports for intra-crate callers — kept for API stability while
// the gateway's outer middleware still spells these names against the
// `crate::router::*` path.
#[allow(unused_imports)]
pub(crate) use dispatch::{
    compute_bucket_id, extract_app_name, is_websocket_upgrade, subscription_affinity_key,
};
