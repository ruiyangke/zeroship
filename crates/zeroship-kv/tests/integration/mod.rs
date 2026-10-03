//! KV integration suites linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until it is declared here. The shared Docker fixture module lives at
//! the crate root; suites reach it through `crate::support`.

mod architecture;
mod config;
mod delete_contract;
mod redis_backend;
mod state_dir_lock_marker;
mod store;
mod topologies;
