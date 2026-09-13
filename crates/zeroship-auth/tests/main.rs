// The async integration cases need this limit on their own crate root.
#![recursion_limit = "256"]

//! Auth integration tests share an executable and keep fixtures private.
//!
//! Cargo auto-discovery is disabled for this crate. Register new modules here
//! or beneath an existing module so ordinary cargo test includes their cases.
//! Store tests live under `store`; HTTP and protocol modules exercise the
//! production routes. Owned database cases manage their servers and connection
//! tasks through `common::database`. Older cases still require the configured
//! platform database while their fixtures are being converted.
//!
//! Select a group with `cargo test -p zeroship-auth --test main -- store::`.
//! Each case must own mutable resources even though the executable is shared.

mod common;

mod account_deletion_test;
mod account_linking;
mod audit_retention_test;
mod check_config_smtp_test;
mod cli_device_refresh_test;
mod config_env_tier;
mod consent_ui_test;
mod device_grant_test;
mod enum_defense;
mod federation;
mod logout;
mod email_verification;
mod magic_login;
mod metadata_cache_headers_test;
mod oauth_start_intake_test;
mod oidc_authorization_code_test;
mod oidc_brokered_login_test;
mod oidc_foundation_test;
mod oidc_login_consent_test;
mod oidc_refresh_token_test;
mod oidc_token_client_auth_test;
mod oidc_userinfo_test;
mod password_login;
mod password_reset;
mod password_test;
mod postmark_webhook_test;
mod relay_auto_revoke_test;
mod relay_dedup_test;
mod security_headers_test;
mod second_factor;
mod signing_key_retention_test;
mod signup_continuation_test;
mod signup_forgot_ratelimit_test;
mod store;
mod template_a11y_test;
mod template_csp_test;
mod template_css_test;
mod template_password_policy_test;
mod threat_model;
mod token_sweep_test;
