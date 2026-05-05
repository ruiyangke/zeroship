//! RPC v2 native foundation. See `docs/proposals/rpc-v2.md`.
//!
//! Phase 1 ships the shared error envelope:
//!   - [`ZsErrorCode`] — closed enum of canonical error codes (gRPC-flavoured,
//!     stable on the wire).
//!   - [`RpcError`] — native `#[v8_class]` exposed on `globalThis` as
//!     `RpcError`. Inherits `Error.prototype` so `instanceof Error` is true.
//!   - [`RpcErrorInit`] — WebIDL dict for the constructor's options arg.
//!   - [`build`] / [`throw`] / [`install_global`] — Rust-side helpers for
//!     constructing RpcError instances and wiring the class onto a V8 isolate.

pub mod error;

pub use error::{
    build, install_global, throw, RpcError, RpcErrorBuildOptions, RpcErrorInit, ZsErrorCode,
};
