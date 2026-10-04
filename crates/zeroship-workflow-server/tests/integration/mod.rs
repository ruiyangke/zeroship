//! The workflow-server in-process integration suites.
//!
//! These suites run the service's public API in-process against its database
//! fixture. Suites that spawn the shipped `zeroship-workflow-server` binary
//! live in `tests/e2e/`.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/<name>.rs` is
//! compiled by nothing until it is declared below; add `mod <name>;` in the
//! same change as the file, or its tests never run.
//!
//! Select a subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-server --test main integration:: -- `http_runs::`

mod control_policy;
mod coordination_wire;
mod coordinator;
mod http_policy;
mod http_runs;
mod lifecycle;
mod maintenance_lane;
mod placement_eligibility;
mod platform_schema;
mod publication_wake;
mod worker_zone;
mod zone_declaration;
