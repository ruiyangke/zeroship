//! The integration contracts for `zeroship-workflow-manager`.
//!
//! The suites share one database fixture, so they compile into one test binary.
//! `Cargo.toml` sets `autotests = false`, so a new `tests/integration/<name>.rs`
//! is compiled by nothing until it is declared below; add `mod <name>;` in the
//! same change as the file, or its tests never run. A suite is addressed by its
//! module path now, so select a subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-manager --test main -- `integration::queue::`

mod capacity;
mod claim;
mod closing;
mod dispatch_fairness;
mod driver;
mod give_back;
mod hold_release;
mod management;
mod queue;
mod recovery;
mod retention;
mod retirement;
mod schedule_lifecycle;
mod scheduling;
mod storage_classification;
