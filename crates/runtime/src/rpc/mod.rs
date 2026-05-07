//! RPC v2 native foundation. See `docs/proposals/rpc-v2.md`.
//!
//! Current layout:
//!
//!   - `zeroship-core::superjson`: wire envelope types, byte
//!     serializer, npm-fixture-compatible round-trip.
//!   - `error.rs`: `RpcError` `#[v8_class]` + the `ZsErrorCode` enum.
//!     Exposed on `globalThis`, inherits `Error`.
//!   - `superjson.rs`: V8 ↔ envelope encoder/decoder. Encodes
//!     `v8::Value` into the wire envelope and revives it on the
//!     receiving side.
//!   - `ctx_holder.rs`: per-request `RpcCtx` `#[v8_class]` with lazy
//!     accessors for `requestId` / `traceId` / `method` / `url` /
//!     `headers` / `signal` / `user` / `idempotencyKey`.
//!   - `dispatch.rs`: ALS slot install/restore + `__zeroshipGetRpcCtx()`
//!     that lets the holder survive `await` boundaries.
//!   - `abort.rs`: per-isolate `AbortRegistry` +
//!     `entered_for_eviction(app_id)` — fires every in-flight
//!     procedure's `ctx.signal` when the worker's LRU cache evicts the
//!     isolate.

pub mod abort;
pub mod ctx_holder;
pub mod dispatch;
pub mod error;
pub mod superjson;

pub use abort::{entered_for_eviction, register_in_flight, AbortGuard};
pub use ctx_holder::{mint_rpc_ctx, RpcCtx};
pub use dispatch::{
    install_globals as install_dispatch_globals, rpc_ctx_als_key, with_rpc_context_lazy,
};
pub use error::{
    build, install_global, throw, RpcError, RpcErrorBuildOptions, RpcErrorInit, ZsErrorCode,
};
pub use superjson::{decode_from_bytes, decode_to_v8, encode_to_bytes, encode_to_envelope};
