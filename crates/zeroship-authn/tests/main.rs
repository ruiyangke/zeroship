//! The `zeroship-authn` single test target.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/<name>.rs` is compiled
//! by nothing until its tier module declares it. Shared fixtures live under
//! `tests/support/`; public-API suites live under `tests/integration/`.

#![recursion_limit = "256"]

mod support;
mod integration;
