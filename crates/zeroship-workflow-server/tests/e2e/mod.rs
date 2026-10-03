//! The workflow-server end-to-end suites.
//!
//! Every suite here spawns the shipped `zeroship-workflow-server` binary and
//! drives it over its HTTP API. `Cargo.toml` sets `autotests = false`, so a new
//! `tests/<name>.rs` is compiled by nothing until it is declared below.
//!
//! Select a subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-server --test main e2e:: -- `http_jobs::`

mod config;
mod driver;
mod http;
mod http_jobs;
mod http_schedules;
mod run_wire_pair;
