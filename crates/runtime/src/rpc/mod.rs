//! RPC v2 native foundation. See `docs/proposals/rpc-v2.md`.
//!
//! Phase 1 (Waves A–C):
//!
//!   - **Wave A** (`zeroship-core::superjson`): wire envelope types,
//!     bytes serializer, npm-fixture-compatible round-trip.
//!   - **Wave B** (`error.rs`): `RpcError` `#[v8_class]` + the
//!     `ZsErrorCode` enum. Exposed on `globalThis`, inherits `Error`.
//!   - **Wave C** (`superjson.rs`): V8 ↔ Envelope encoder/decoder.
//!     Encodes `v8::Value` into the wire envelope and revives it on
//!     the receiving side.

pub mod error;
pub mod superjson;

pub use error::{
    build, install_global, throw, RpcError, RpcErrorBuildOptions, RpcErrorInit, ZsErrorCode,
};
pub use superjson::{decode_from_bytes, decode_to_v8, encode_to_bytes, encode_to_envelope};
