//! The policy crate's test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers this file as the `main`
//! target. The public-API suites live under `tests/integration/`.
//!
//!   cargo test -p zeroship-migrate-policy --test main integration::

mod integration;
