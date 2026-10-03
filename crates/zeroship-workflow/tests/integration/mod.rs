//! The integration contracts for `zeroship-workflow`.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/integration/<name>.rs`
//! is compiled by nothing until it is declared below; add `mod <name>;` in the
//! same change as the file, or its tests never run. A suite is addressed by its
//! module path now, so select a subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow --test main -- `integration::execution::`

mod bundle_executable;
mod bundle_schedule;
mod deployment_hold_client;
mod execution;
mod operations;
