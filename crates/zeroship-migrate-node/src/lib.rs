//! `zeroship-migrate-node` — the Node/Bun N-API addon for `zeroship-migrate`
//! (design §C).
//!
//! It exposes the migrate engine's core surface over N-API with a **zero-tokio,
//! zero-io_uring** transport, so the `.node` is cross-platform (no Linux-locked
//! compio). The engine is unchanged; only the *driver* is host-supplied:
//!
//! - **Sync, DB-free entrypoints** (`plan`, `generate`/load-verify) run inline on
//!   the napi call thread — no bridge (§C.5).
//! - **Async, host-driven entrypoints** (`apply`, `status`, `history`) run the
//!   engine on its own worker thread via a reactor-less `futures::executor::block_on`
//!   ([`runtime`]) and bottom out in the host driver over a `ThreadsafeFunction`
//!   ([`bridge`], `napi` feature), returning a JS `Promise` resolved cross-thread
//!   (fire-and-resolve — §B.3/§C.4). The bridge parks each verb on a `oneshot`
//!   fired by a Rust-supplied `done` callback — NO `#[napi] async fn`, NO
//!   `Promise::await`, NO `tokio_rt`.
//!
//! `dry_run` is **not** surfaced in v1: on the `host-pg` build `backend.shadow()`
//! is `None`, so a shadow dry-run would return `DryRunError::ShadowUnsupported`. The
//! host-side shadow harness (§F) is a deferred follow-up.

pub mod marshal;
pub mod runtime;
pub mod session;

/// Sync, DB-free API surface (plan / load-verify) — pure functions returning
/// JSON strings, callable directly or through the napi entrypoints.
pub mod api;

/// The N-API transport: `#[napi]` entrypoints, the `ThreadsafeFunction`-backed
/// [`session::VerbDispatch`], and the `JsDeferred` fire-and-resolve wrapper. Only
/// compiled with the `napi` feature (needs the Node ABI); the pure-Rust core above
/// builds and its integration suite runs WITHOUT a Node host.
#[cfg(feature = "napi")]
pub mod bridge;
