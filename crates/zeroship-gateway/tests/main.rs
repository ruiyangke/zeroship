//! The gateway's single test target.
//!
//! In-process public-API suites live under `tests/integration/`; suites that
//! spawn the shipped `zeroship-gate` binary live under `tests/e2e/`. Run a tier
//! with `cargo test -p zeroship-gateway --test main integration::` or
//! `... --test main e2e::`.

mod integration;
mod e2e;
