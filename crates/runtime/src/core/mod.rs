//! Runtime infrastructure — V8 pump, dispatch, modules, server plumbing.
//!
//! Distinct from `web/` (Web API surface) and `transport/` (Rust HTTP
//! plumbing). Everything in this module is internal scaffolding that
//! wires V8 to the compio event loop.

pub mod channel;
pub mod dispatch;
pub mod init;
pub mod modules;
pub mod native_modules;
pub mod node_error;
pub(crate) mod panic_util;
pub mod plugin;
pub mod runtime;
pub mod serve;
pub mod state;

#[cfg(target_os = "linux")]
pub mod cpu_timer;
