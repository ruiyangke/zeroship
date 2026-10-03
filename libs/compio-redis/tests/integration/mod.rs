//! The compio-redis integration suites.
//!
//! `Cargo.toml` sets `autotests = false` and registers `tests/main.rs` as the
//! `main` target, so a suite is compiled only once it is declared here. Add
//! `mod <name>;` with the file or its tests never run. The suites reach the
//! shared fixture helpers at the target root as `crate::support::...`.

mod cluster;
mod redis_ops;
