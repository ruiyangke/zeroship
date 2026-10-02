//! The integration contracts for `zeroship-workflow-manager`, in one binary.
//!
//! The suites share one database fixture, so they compile into one test binary
//! instead of one per `tests/*.rs`. `Cargo.toml` sets `autotests = false`, so a
//! new `tests/integration/<name>.rs` is compiled by nothing until it is
//! declared below; add `mod <name>;` in the same change as the file, or its
//! tests never run. A suite is addressed by its module path now, so select a
//! subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-manager --test integration -- `queue::`

#![recursion_limit = "256"]

mod support;

#[path = "../../src/models/schema_definition.rs"]
mod native_schema;

mod capacity;
mod closing;
mod coordinator;
mod dispatch_fairness;
mod driver;
mod hold_release;
mod management;
mod placement;
mod policy;
mod queue;
mod recovery;
mod retention;
mod retirement;
mod schedule_lifecycle;
mod scheduling;
mod storage_classification;
