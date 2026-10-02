//! The integration contracts for `zeroship-workflow-v8`, in one binary.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/integration/<name>.rs`
//! is compiled by nothing until it is declared below; add `mod <name>;` in the
//! same change as the file, or its tests never run. A suite is addressed by its
//! module path now, so select a subset with a filter rather than a target:
//!
//!   cargo test -p zeroship-workflow-v8 --test integration -- `dispatch::`

mod support;

mod binding;
mod dispatch;
mod runner;
mod service_binding;
