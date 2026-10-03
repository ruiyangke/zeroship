//! The workflow-server single test target.
//!
//! Shared fixtures live under `tests/support/`; in-process public-API suites
//! live under `tests/integration/`; suites that spawn the shipped
//! `zeroship-workflow-server` binary live under `tests/e2e/`. Run a tier with
//! `cargo test -p zeroship-workflow-server --test main integration::` or
//! `cargo test -p zeroship-workflow-server --test main e2e::`.

mod support;
mod integration;
mod e2e;
