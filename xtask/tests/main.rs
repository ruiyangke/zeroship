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
//!
//! `common` is the shared owned-PostgreSQL fixture for the database-orchestration
//! areas, declared once here so both reach one copy.

mod architecture;
mod common;
mod live_db;
mod playwright;
mod repository;
mod suite_db;
mod workflow;
