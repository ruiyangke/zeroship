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
pub mod crypto_node;
pub mod dom;
pub mod encoding;
pub mod fetch;
pub mod headers;
pub mod streams;
pub mod structured_clone;
pub mod url;
pub mod websocket;
