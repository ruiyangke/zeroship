//! Node-specific API surfaces.
//!
//! Establishes the symmetry with `web/`: WHATWG/W3C Web APIs live
//! under `web/`, Node.js APIs live here. Both call into shared
//! algorithm backends (e.g. `crate::crypto_ops`) at the boundary.
//!
//! Currently hosts only `crypto` (the `node:crypto` surface), but
//! is the natural home for future `node:*` modules implemented
//! natively (e.g. `node:zlib`, `node:os`).

pub mod crypto;
