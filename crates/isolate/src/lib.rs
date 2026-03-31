//! # appbase-isolate
//!
//! V8 isolate management for the appbase platform.
//!
//! - `Isolate`: wraps a `deno_core::JsRuntime` with plugin support
//! - `IsolateActor`: runs an isolate on a dedicated thread, communicates via messages
//! - `IsolatePool`: manages a pool of actors, one per app, with idle eviction
//! - `cpu`: CPU time metering via `CLOCK_THREAD_CPUTIME_ID`

pub mod actor;
pub mod cpu;
pub mod isolate;
mod permissions;
pub mod pool;
pub mod watchdog;
