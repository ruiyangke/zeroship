//! Authz integration suites linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until it is declared here.

mod engine_test;
mod injection_test;
mod platform_policies_test;
mod scaffold_test;
