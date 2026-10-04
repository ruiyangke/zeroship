//! The compio-postgres test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers this file as the `main`
//! target. The public-API integration suites are modules of `tests/integration/`
//! and reach the shared helpers at this root as `support::...`.
//!
//! `Cargo.toml` declares further `[[test]]` entries, each of which owns its
//! process on purpose:
//!
//!   serialized_loop, socket_release, query_debug_logging,
//!   serialized_teardown_logging
//!                    each installs a process-global `log::set_logger`, and only
//!                    the first install in a process takes effect.
//!
//! A crate-level inner attribute belongs here rather than in a module:
//! `#![recursion_limit]` inside a module is ignored.

#![recursion_limit = "256"]

mod support;
mod integration;
