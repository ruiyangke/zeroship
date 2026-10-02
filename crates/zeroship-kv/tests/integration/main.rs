//! KV integration suites linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until it is declared here. `support` is the shared Docker fixture
//! module; suites reach it through `crate::support`.

mod architecture;
mod config;
mod delete_contract;
mod redis_backend;
mod state_dir_lock_marker;
mod store;
mod topologies;

#[cfg(feature = "redis")]
mod support;
