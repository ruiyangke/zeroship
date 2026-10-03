//! The `zeroship-data-orm` test suites in one binary.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/integration/<name>.rs`
//! is compiled by nothing until `tests/integration/mod.rs` declares it. Select
//! a suite with a module-path filter:
//!
//! `cargo test -p zeroship-data-orm --test main -- integration::sql::`

mod integration;
