// The async integration cases need this limit on their own crate root.
#![recursion_limit = "256"]

//! The auth single test target.
//!
//! Shared fixtures live under `tests/support/`; in-process public-API suites
//! live under `tests/integration/`; browser and `--check-config` suites live
//! under `tests/e2e/`. Run a tier with
//! `cargo test -p zeroship-auth --test main integration::` or
//! `cargo test -p zeroship-auth --test main e2e::`.

mod support;
mod integration;
mod e2e;
