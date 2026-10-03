//! The `zeroship-storage-v8` single test target.
//!
//! `Cargo.toml` sets `autotests = false`, so a new `tests/<name>.rs` is compiled
//! by nothing until its tier module declares it. Public-API suites live under
//! `tests/integration/`.

mod integration;
