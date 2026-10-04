//! The integration contracts for `zeroship-workflow-client`.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/integration/<name>.rs`
//! is compiled by nothing until it is declared below; add `mod <name>;` in the
//! same change as the file, or its tests never run. A suite is addressed by its
//! module path now, so select a subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-client --test main -- `integration::jobs_client::`

mod coordination_transport;
mod jobs_client;
mod queue_holds;
mod round_trip_cost;
mod schedules_client;
mod transport_fence;
