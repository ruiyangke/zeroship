//! Runtime infrastructure — V8 pump, dispatch, modules, server plumbing.
//!
//! Distinct from `web/` (Web API surface) and `transport/` (Rust HTTP
//! plumbing). Everything in this module is internal scaffolding that
//! wires V8 to the compio event loop.

pub(crate) mod application_entry;
pub mod brand;
pub mod callback;
pub mod channel;
pub mod databases;
pub mod dev_auth;
pub(crate) mod dev_entry;
pub mod dispatch;
pub mod dynamic_import;
pub(crate) mod heap_cap;
pub mod init;
pub(crate) mod invocation;
pub mod modules;
pub mod native_modules;
pub mod node_error;
pub(crate) mod panic_util;
pub mod plugin;
pub(crate) mod plugin_modules;
pub mod runtime;
pub mod serve;
pub(crate) mod startup;
pub mod state;
pub mod strings;
pub(crate) mod tasks;
mod zeroship_module;

#[cfg(target_os = "linux")]
pub mod cpu_timer;

/// Whether a CPU limit stops a handler while it runs.
///
/// The stop is the POSIX CPU-time timer `cpu_timer` arms on the isolate's
/// thread, which only Linux builds; elsewhere a limit could only be checked
/// after the handler returned, which no caller may present as enforcement.
pub const CPU_LIMIT_STOPS_A_RUNNING_HANDLER: bool = cfg!(target_os = "linux");
