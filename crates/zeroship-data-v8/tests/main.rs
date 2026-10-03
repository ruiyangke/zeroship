#![recursion_limit = "256"]

//! The `zeroship-data-v8` test suites in one binary.
//!
//! `Cargo.toml` sets `autotests = false`, so a new suite is compiled by nothing
//! until its tier's `mod.rs` declares it. Ordinary package tests run this
//! target, the shared fixtures in `src/tests/`, and the process-isolated
//! distributed CDC target. Database fixtures are private source modules;
//! PostgreSQL is required.
//!
//! Select a tier or a suite with a module-path filter:
//!
//! `cargo test -p zeroship-data-v8 --test main integration::capability::`
//! `cargo test -p zeroship-data-v8 --test main e2e::distributed_live::`

mod integration;
mod e2e;
