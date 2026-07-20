//! `zero-migrate-node` — the Node/Bun N-API addon for `zero-migrate`.
//!
//! It exposes the migrate engine's core surface over N-API with a **zero-tokio,
//! zero-io_uring** transport, so the `.node` is cross-platform. The engine is
//! unchanged; only the *driver* is host-supplied:
//!
//! - **Sync, DB-free entrypoints** (`plan`, `generate`/load-verify) run inline on
//!   the napi call thread — no bridge.
//! - **Async, host-driven entrypoints** (`apply`, `status`, `history`) run the
//!   engine on its own worker thread via a reactor-less `futures::executor::block_on`
//!   ([`runtime`]) and bottom out in the host driver over a `ThreadsafeFunction`
//!   ([`bridge`], `napi` feature), returning a JS `Promise` resolved cross-thread
//!   (fire-and-resolve). The bridge parks each verb on a `oneshot`
//!   fired by a Rust-supplied `done` callback — NO `#[napi] async fn`, NO
//!   `Promise::await`, NO `tokio_rt`.
//!
//! `dry_run` is **not** surfaced in v1: on the `host-pg` build `backend.shadow()`
//! is `None`, so a shadow dry-run would return `DryRunError::ShadowUnsupported`. The
//! host-side shadow harness is a deferred follow-up.

// The workspace pins `unsafe_code = "deny"` (correct for the pure-Rust engine
// crates). This addon is the ONE workspace member that MUST use `unsafe`: the napi
// bridge FFIs into the Node ABI (`ToNapiValue`/`FromNapiValue` raw conversions in
// `bridge.rs`) are `unsafe fn`s by contract. Scope the allowance to this crate so
// the engine's blanket deny stays intact everywhere else.
#![allow(unsafe_code)]

/// The single source of truth for every N-API boundary DTO: the
/// driver cell transport + the typed verb request/response envelopes. napi-rs emits
/// ONE `index.d.ts` from these `#[napi(object)]` structs; the TS host imports it
/// (no hand-copied interfaces).
pub mod wire;

pub mod marshal;
pub mod runtime;
pub mod session;

#[cfg(test)]
pub(crate) mod test_fixtures;

/// Sync, DB-free API surface (plan / load-verify) — pure functions returning
/// JSON strings, callable directly or through the napi entrypoints.
pub mod api;

/// Host-authoring lower: turn a pure-JS IR envelope into the
/// `Vec<Migration>` the engine consumes, folding the authoritative
/// `Checksum::of_ir` in Rust. Pure functions (no napi); the `applyIr`/`statusIr`
/// bridge entrypoints wrap them.
pub mod lower;

/// The N-API transport: `#[napi]` entrypoints, the `ThreadsafeFunction`-backed
/// [`session::VerbDispatch`], and the `JsDeferred` fire-and-resolve wrapper. Only
/// compiled with the `napi` feature (needs the Node ABI); the pure-Rust core above
/// builds and its integration suite runs WITHOUT a Node host.
#[cfg(feature = "napi")]
pub mod bridge;
