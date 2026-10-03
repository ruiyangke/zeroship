//! The postgres backend's test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers this file as the `main`
//! target. The shared helpers live under `tests/support/`; the public-API
//! suites under `tests/integration/`.
//!
//!   cargo test -p zeroship-migrate-postgres --test main integration::

mod support;
mod integration;
