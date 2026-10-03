//! The gateway's integration suites, linked into a single test executable.
//!
//! These exercise the crate's public API in-process. Suites that spawn the
//! shipped `zeroship-gate` binary live in `tests/e2e/`.

mod op_breaker_test;
mod op_pool_idle_test;
