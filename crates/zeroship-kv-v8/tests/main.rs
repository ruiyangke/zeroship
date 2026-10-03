//! The kv-v8 suites, linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until `tests/integration/mod.rs` declares it. `support` is the
//! shared Docker fixture module, consumed from `zeroship-testkit`; suites reach
//! it through `crate::support`.

mod support;

mod integration;
