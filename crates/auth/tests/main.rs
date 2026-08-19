//! The one integration-test target for `zeroship-auth`.
//!
//! WHY THIS FILE EXISTS
//! --------------------
//! Cargo links one executable per `tests/*.rs` file, and every one of those
//! executables statically links this crate plus its whole dependency graph.
//! The 52 files below used to cost 52 such executables. Measured on this tree
//! at `[profile.dev] debug = "line-tables-only"`, 2026-08-19:
//!
//!   before  55 executables for `cargo test -p zeroship-auth --no-run`,
//!           5,786,412,312 bytes
//!   after   see the commit message; the 52 collapse into one.
//!
//! `tests/common/` was also compiled once per binary and is now compiled once.
//!
//! HOW TO ADD A TEST FILE
//! ----------------------
//! `Cargo.toml` sets `autotests = false` and declares this file as the only
//! `[[test]]`. A new `tests/<name>.rs` is therefore compiled by NOTHING until
//! it is listed below. Add `mod <name>;` in the same commit as the file, or
//! the tests in it never run and nothing says so.
//!
//! HOW TO RUN A SUBSET
//! -------------------
//! `cargo test -p zeroship-auth --test <file>` no longer resolves - there is
//! one target and its name is `main`. Use the filter instead:
//!
//!   cargo test -p zeroship-auth --test main -- <substring>
//!
//! WHAT THIS CHANGED ABOUT ISOLATION
//! ---------------------------------
//! These modules now share one process, so a `static` in one is visible to all
//! of them. That matters in exactly one place today:
//! `reset_hash_gate_test` reads a delta across the process-global
//! `password::hash_calls()` counter and guards it with a `static Mutex` that
//! only its own tests take. Any other module hashing a password concurrently
//! would corrupt that delta. `tests/run_auth_suite.sh` passes
//! `--test-threads 1`, so nothing in this binary runs concurrently under the
//! gate; a bare `cargo test -p zeroship-auth` does not, and that is the run
//! where the counter is exposed.

mod common;

mod account_deletion_test;
mod account_lockout_test;
mod audit_retention_test;
mod check_config_smtp_test;
mod cli_device_refresh_test;
mod consent_ui_test;
mod device_grant_test;
mod e2e_github;
mod e2e_google;
mod e2e_magic_native;
mod enum_defense;
mod framing_config_test;
mod identities_test;
mod link_lockout_test;
mod link_ratelimit_test;
mod logout_test;
mod m4_post_redeem_test;
mod magic_link_test;
mod magic_verify_test;
mod metadata_cache_headers_test;
mod oauth_start_intake_test;
mod oidc_authorization_code_test;
mod oidc_backchannel_logout_test;
mod oidc_brokered_login_test;
mod oidc_foundation_test;
mod oidc_login_consent_test;
mod oidc_refresh_token_test;
mod oidc_token_client_auth_test;
mod oidc_userinfo_test;
mod password_reset_test;
mod password_test;
mod postmark_webhook_test;
mod ratelimit_test;
mod relay_auto_revoke_test;
mod relay_dedup_test;
mod reset_hash_gate_test;
mod security_headers_test;
mod session_visibility_test;
mod signing_key_retention_test;
mod signup_continuation_test;
mod signup_forgot_ratelimit_test;
mod template_a11y_test;
mod template_csp_test;
mod template_css_test;
mod template_password_policy_test;
mod threat_model;
mod token_sweep_test;
mod totp_enroll_reauth_test;
mod totp_gates_every_mint_path_test;
mod totp_login_challenge_test;
mod totp_store_test;
mod verification_test;
