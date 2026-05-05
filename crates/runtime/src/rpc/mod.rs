//! RPC v2 native foundation. See `docs/proposals/rpc-v2.md`.
//!
//! Phase 1 (Waves A–D):
//!
//!   - **Wave A** (`zeroship-core::superjson`): wire envelope types,
//!     bytes serializer, npm-fixture-compatible round-trip.
//!   - **Wave B** (`error.rs`): `RpcError` `#[v8_class]` + the
//!     `ZsErrorCode` enum. Exposed on `globalThis`, inherits `Error`.
//!   - **Wave C** (`superjson.rs`): V8 ↔ Envelope encoder/decoder.
//!     Encodes `v8::Value` into the wire envelope and revives it on
//!     the receiving side.
//!   - **Wave D** (`dispatch.rs`): per-request `RpcContext`, frozen
//!     native Headers/URL wrapping, and the ALS slot install/restore
//!     that lets `__zeroshipGetRpcCtx()` survive `await` boundaries.

pub mod dispatch;
pub mod error;
pub mod superjson;

pub use dispatch::{
    install_globals as install_dispatch_globals, rpc_ctx_als_key, with_rpc_context_in_als,
    RpcContext, RpcContextHandle,
};
pub use error::{
    build, install_global, throw, RpcError, RpcErrorBuildOptions, RpcErrorInit, ZsErrorCode,
};
pub use superjson::{decode_from_bytes, decode_to_v8, encode_to_bytes, encode_to_envelope};
