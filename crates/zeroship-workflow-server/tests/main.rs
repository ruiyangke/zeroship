//! The integration contracts for `zeroship-workflow-server`, in one binary.
//!
//! Cargo links one executable per `tests/*.rs`, and each statically links the
//! service and its database fixture. Registering the suites as modules here
//! leaves one integration executable. `Cargo.toml` sets `autotests = false`, so
//! a new `tests/<name>.rs` is compiled by nothing until it is declared below;
//! add `mod <name>;` in the same change as the file, or its tests never run.
//!
//! A suite is addressed by its module path now, so select a subset with a
//! filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-server --test main -- `http_jobs::`

mod support;

mod config;
mod control_policy;
mod coordination_wire;
mod coordinator;
mod driver;
mod http;
mod http_jobs;
mod http_policy;
mod http_runs;
mod http_schedules;
mod lifecycle;
mod maintenance_lane;
mod placement_eligibility;
mod platform_schema;
mod run_wire_pair;
