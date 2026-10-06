//! The xtask test executable.
//!
//! `Cargo.toml` sets `autotests = false` and registers this file as the `main`
//! target, so a `tests/<name>.rs` is compiled by nothing until a module path is
//! declared here.
//!
//! Select an area with a module-path filter:
//!
//!   cargo test --manifest-path xtask/Cargo.toml --test main repository::
//!   cargo test --manifest-path xtask/Cargo.toml --test main architecture::

mod architecture;
mod playwright;
mod repository;
