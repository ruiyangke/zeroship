#![recursion_limit = "256"]

//! The `zeroship-control` test suites in one binary.
//!
//! `Cargo.toml` sets `autotests = false`, so a new suite is compiled by nothing
//! until its tier's `mod.rs` declares it. Shared fixtures live under
//! `tests/support/`; public-API suites under `tests/integration/`; the suites
//! that drive real processes under `tests/e2e/`. `tests/live_case_rings.rs`,
//! `tests/workflow_e2e.rs` and `tests/database_decoupling_e2e.rs` keep their
//! own targets; `Cargo.toml` states why each cannot share this binary.
//!
//! Select a tier or a suite with a module-path filter:
//!
//! `cargo test -p zeroship-control --test main integration::spend::`
//! `cargo test -p zeroship-control --test main e2e::control_boot_test::`

mod support;
mod integration;
mod e2e;
