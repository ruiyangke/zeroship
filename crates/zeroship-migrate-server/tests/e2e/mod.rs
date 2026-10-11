//! The migration server's end-to-end suites.
//!
//! These launch the freshly built `zeroship-migrate-server` binary with a
//! private environment. They are declared as `e2e` modules of the single `main`
//! test target.

mod check_config;
