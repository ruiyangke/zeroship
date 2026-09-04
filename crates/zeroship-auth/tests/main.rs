// This target is its own crate ROOT and overflows rustc's layout query on the
// async block at the end of the `oidc_refresh_token_test` module.
// `recursion_limit` is per crate root, so the crate's lib does not cover it,
// and merging the files into one target means this entry file carries the
// attribute for all of them. Caught by `tests/clippy_gate.sh`, which lints
// `--all-targets`; a bare `cargo test -p zeroship-auth --lib` never builds it.
#![recursion_limit = "256"]

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
//!           5,786,412,312 bytes, linked in 197 s
//!   after    4 executables,       493,857,928 bytes, linked in  35 s
//!
//! `tests/common/` was also compiled once per binary and is now compiled once.
//! The 254 integration test names are unchanged: the merged binary's
//! `--list` output is byte-for-byte the union of the 52 old binaries'.
//!
//! WHAT THE MERGE EXPOSED: THE PROCESS USED TO BE THE RECLAMATION BOUNDARY
//! ----------------------------------------------------------------------
//! This section is history, not a warning: the target passes. It is here
//! because the merge did not CAUSE the failure below, it removed the thing
//! that had been hiding it, and anyone merging test binaries in another
//! crate will meet the same wall if the driver ever regresses.
//!
//! On the day it was written this target reported 387 passed and 146
//! failed against `tests/run_auth_suite.sh`'s 641. 145 of the 146 were one
//! error:
//!
//!   FATAL 53300 "sorry, too many clients already"
//!
//! Every live-PG test here opens a connection and drives it with
//! `compio::runtime::spawn(connection.run()).detach()`. `#[compio::test]`
//! builds a fresh `Runtime` per test and drops it at the end - and that
//! drop reclaimed nothing, because a task parked on an io_uring submission
//! holds a clone of the runtime's `Rc` (compio-runtime `runtime/future.rs`,
//! `Submit`) and `Runtime::drop` skips reclamation entirely while that `Rc`
//! is shared (`runtime/mod.rs`). Detaching was not the cause - a retained
//! `JoinHandle` leaks the same - the pending submission was. So the
//! connection lived until the PROCESS EXITED, and cargo's 52 processes were
//! the only thing reclaiming connections.
//!
//! Measured 2026-08-19 against `zs-auth-pg-5440` (`max_connections` = 100)
//! by sampling `pg_stat_activity` every 2 s while running the OLD,
//! PRE-MERGE binaries under `--test-threads 1`.
//!
//! `totp_store_test` isolated the mechanism: 8 tests, one `connect()` each,
//! and NO `ntex::web::test::server` anywhere in the file.
//!
//!   1 1 1 1 1 2 2 2 2 3 3 3 3 3 4 4 4 4 6 6 6 6 7 8 8 8 8
//!
//! Eight tests, eight connections, one per test, none reclaimed, and 0 the
//! moment the process exited. The HTTP fixtures were not involved.
//!
//! `oidc_refresh_token_test` showed the same shape at the scale that
//! mattered - 30 tests, 1.9 connections each because it also boots a
//! server:
//!
//!   0 -> 9 -> 17 -> 26 -> 35 -> 45 -> 56, then 0 once the process exited
//!
//! FIXED IN THE DRIVER, NOT HERE, AND NOT BY RAISING `max_connections`.
//! `libs/compio-postgres/src/release.rs`: the client half owns a dup of the
//! socket and shuts it down from its `Drop`, a syscall that needs no
//! executor, so a session ends with the client that opened it instead of
//! with the process.
//! `libs/compio-postgres/tests/suite/integration.rs`'s
//! `a_connection_does_not_outlive_the_runtime_that_opened_it` holds that
//! invariant, and holds it as a LIFETIME rather than a ceiling - it asserts
//! the server-side count returns to zero after each runtime, which a merely
//! bounded count would not.
//!
//! Sampled the same way over this whole 254-test target, 2026-08-19:
//! 197 samples, peak 6, and 80 of them read 0. Not "under the limit" -
//! back to baseline between tests.
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
mod config_env_tier;
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
mod totp_removal_notice_test;
mod totp_store_test;
mod verification_test;
