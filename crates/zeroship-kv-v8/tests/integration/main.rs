//! kv-v8 integration suites linked into one test executable.
//!
//! The Docker fixture module is shared with `zeroship-kv`'s suites through its
//! path; the suites reach it through `crate::support`. `autotests` is off, so a
//! new `tests/integration/<name>.rs` is compiled by nothing until it is
//! declared here.

#[path = "../../../zeroship-kv/tests/integration/support/mod.rs"]
mod support;

mod e2e_runtime;
mod isolation;
