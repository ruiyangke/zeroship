//! Auth end-to-end suites: the browser and `--check-config` cases that drive
//! the shipped `zeroship-auth` binary and the Playwright specs under
//! `tests/web/`. Run them with
//! `cargo test -p zeroship-auth --test main e2e::`.

mod auth_ui;
mod check_config_smtp_test;
mod config_env_tier;
