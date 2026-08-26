//! Web API surface — the spec-facing classes and helpers we install on
//! every isolate (Headers, URL, Blob, fetch, streams, crypto, WebSocket,
//! TextEncoder, etc.).
//!
//! Distinct from `core/` (V8 + dispatch + module loader) and `transport/`
//! (Rust-side HTTP plumbing — cyper client, SSRF guard, kernel bridge).

pub mod base64;
pub mod blob;
pub mod codec;
pub mod crypto;
pub mod dom;
pub mod encoding;
pub mod eventsource;
pub mod fetch;
pub mod headers;
pub mod streams;
pub mod structured_clone;
pub mod url;
pub mod websocket;

// Back-compat shim: `node:crypto` is a Node-API surface, not a Web
// API, so it was moved to `crate::node::crypto`. Re-export here so
// existing `crate::web::crypto_node::*` paths keep resolving until
// callers naturally migrate.
pub use crate::node::crypto as crypto_node;
