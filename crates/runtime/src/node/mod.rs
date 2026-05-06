//! Node-specific API surfaces.
//!
//! Establishes the symmetry with `web/`: WHATWG/W3C Web APIs live
//! under `web/`, Node.js APIs live here. Both call into shared
//! algorithm backends (e.g. `crate::crypto_ops`) at the boundary.
//!
//! Hosts:
//! - `crypto` — the `node:crypto` surface.
//! - `async_hooks` — `AsyncLocalStorage`, native because the
//!   closure-based polyfill tore down stores before continuations
//!   from native async work (`fetch`) resumed (ISS-01).

pub mod async_hooks;
pub mod crypto;
pub mod os;
pub mod zlib;
