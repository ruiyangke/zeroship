//! The migration server's end-to-end suites, linked into one test executable.
//!
//! These launch the freshly built `zeroship-migrate-server` binary with a private
//! environment, so they live in their own `e2e` target rather than the
//! `integration` binary.

mod check_config;
