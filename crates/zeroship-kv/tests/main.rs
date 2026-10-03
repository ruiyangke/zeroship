//! The KV suites, linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until `tests/integration/mod.rs` declares it. `support` is the
//! shared Docker fixture module; suites reach it through `crate::support`.

#[cfg(feature = "redis")]
mod support;

mod integration;
