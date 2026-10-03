//! kv-v8 integration suites linked into one test executable.
//!
//! The Docker fixture module lives at the crate root, shared with the KV
//! suites through `zeroship-testkit`; the suites reach it through
//! `crate::support`. `autotests` is off, so a new `tests/integration/<name>.rs`
//! is compiled by nothing until it is declared here.

mod e2e_runtime;
mod isolation;
