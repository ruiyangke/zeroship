//! In-Rust V8 host impl for the `zero-migrate` engine seams.
//!
//! The engine captures its entire coupling to a JS runtime behind three
//! engine-owned traits — [`AuthoringHost`], [`RecorderPlatform`], and
//! [`JsDriverHost`] — that name no V8 type. This crate provides the concrete impls
//! backed by the **public [`v8`] crate** (rusty_v8), with no dependency on any
//! internal runtime crate. It is injected as a dev-dependency into the engine's
//! V8-host tests and linked by the authoring/recorder CLIs.
//!
//! ## Coverage (this pass)
//!
//! - [`AuthoringHost`] — **implemented.** Builds a fresh isolate + context, links
//!   the in-memory ESM module graph, installs a minimal globals profile
//!   (`TextEncoder`/`TextDecoder`/`console`/`globalThis`), sets the requested input
//!   string globals, evaluates the entry module, drains microtasks, and reads back
//!   the requested output string globals. Error fidelity maps to
//!   [`HostEvalError::{Install, MissingOutput}`].
//! - [`RecorderPlatform`] — **implemented.** Commits the single-threaded V8
//!   platform once and tracks the committed flavor.
//! - [`JsDriverHost`] — **partial.** The long-lived mysql2-over-`node:net` driver
//!   isolate is a mini-runtime (socket polyfill + event-loop pump) beyond this
//!   pass; the impl is present and type-correct but `open_js_driver` returns an
//!   `Install` error. See the crate README section "V8-host driver: partial".

pub mod host;

pub use host::ZeroMigrateHost;
