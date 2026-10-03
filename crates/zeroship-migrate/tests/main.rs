//! The migration engine's test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers this file as the `main`
//! target, so a new `tests/<name>.rs` is compiled by nothing until a module
//! path is declared here. The shared helpers live under `tests/support/`; the
//! public-API suites under `tests/integration/`; the suites that spawn `cargo`
//! as a child process under `tests/e2e/`.
//!
//! Select a tier or a suite with a module-path filter:
//!
//!   cargo test -p zeroship-migrate --test main integration::
//!   cargo test -p zeroship-migrate --test main e2e::

mod support;
mod integration;
mod e2e;
