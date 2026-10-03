//! The compio-redis test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers this file as the `main`
//! target. The shared Testcontainers fixtures live under `tests/support/`; the
//! public-API suites under `tests/integration/`.
//!
//!   cargo test -p compio-redis --test main integration::

mod support;
mod integration;
