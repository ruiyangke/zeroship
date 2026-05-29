//! Auth server configuration.

use std::path::PathBuf;

use clap::Parser;
use zeroship_core::config::{AuthSection, ObservabilityFlags};

const DEFAULT_HYDRA_ADMIN_URL: &str = "http://127.0.0.1:4445";
const DEFAULT_HYDRA_PUBLIC_URL: &str = "https://auth.zeroship.ai";

#[derive(Debug, Clone, Parser)]
#[command(name = "zeroship-auth")]
pub struct AuthConfig {
    /// Optional shared config overlay path.
    #[arg(long = "config", env = "ZEROSHIP_CONFIG")]
    pub config_path: Option<PathBuf>,

    /// Observability CLI/env overrides.
    #[command(flatten)]
    pub obs: ObservabilityFlags,

    /// Listen address.
    #[arg(long, env = "AUTH_ADDR", default_value = "0.0.0.0:9092")]
    pub addr: String,

    /// `PostgreSQL` DSN.
    #[arg(long, env = "AUTH_DB_URL")]
    pub db_url: String,

    /// Hydra admin base URL (loopback).
    #[arg(long = "hydra-admin-url", env = "HYDRA_ADMIN_URL")]
    pub hydra_admin_url: Option<String>,

    /// Permit a non-loopback Hydra admin URL. Production deployments that
    /// enable this must protect Hydra admin externally with mTLS, firewall
    /// rules, or equivalent network policy.
    #[arg(
        long,
        env = "AUTH_ALLOW_REMOTE_HYDRA_ADMIN",
        action = clap::ArgAction::Set,
        default_value_t = false,
        default_missing_value = "true",
        num_args = 0..=1
    )]
    pub allow_remote_hydra_admin: bool,

    /// Hydra public base URL (issuer).
    #[arg(long = "hydra-public-url", env = "HYDRA_PUBLIC_URL")]
    pub hydra_public_url: Option<String>,

    /// Path to clients config TOML.
    #[arg(long, env = "AUTH_CLIENTS_CONFIG", default_value = "/etc/zeroship/auth-clients.toml")]
    pub clients_config: String,

    /// Allow first-boot JWK + client creation. Without this, an empty
    /// `hydra_jwk` set is a fatal startup error.
    #[arg(long, env = "AUTH_BOOTSTRAP")]
    pub bootstrap: bool,

    /// Dev mode: drop the Secure flag on cookies. ONLY for localhost.
    #[arg(long, env = "AUTH_INSECURE_DEV")]
    pub insecure_dev: bool,

    /// HMAC key (≥32 bytes recommended) used to sign the short-lived
    /// federation stash cookie (`__Host-zsidp_google_stash` etc.). A weak
    /// or default value lets an attacker forge stash cookies and bypass
    /// the OAuth state/PKCE check, so production deployments MUST set
    /// this explicitly. The dev default is accepted only with
    /// `--insecure-dev=true`.
    #[arg(
        long,
        env = "AUTH_STASH_SIGNING_KEY",
        default_value = "dev-only-stash-signing-key-not-for-production-use!!",
        hide_env_values = true
    )]
    pub stash_signing_key: String,

    // ─── Google OAuth (optional — federation routes registered only when set) ───
    /// Google OAuth 2.0 client ID. Without it, `/oauth/google/*` routes are
    /// not registered (auth still boots).
    #[arg(long, env = "AUTH_GOOGLE_CLIENT_ID")]
    pub google_client_id: Option<String>,

    /// Google OAuth 2.0 client secret.
    #[arg(long, env = "AUTH_GOOGLE_CLIENT_SECRET", hide_env_values = true)]
    pub google_client_secret: Option<String>,

    /// Redirect URI registered with Google. Must match the value configured in
    /// the Google Cloud Console exactly.
    #[arg(
        long,
        env = "AUTH_GOOGLE_REDIRECT_URI",
        default_value = "https://auth.zeroship.ai/oauth/google/callback"
    )]
    pub google_redirect_uri: String,

    /// Google's authorize endpoint. Overridable so the e2e tests can point
    /// at an in-process [`tests/common/mock_provider`] instead of the real
    /// Google. Production deployments should leave the default in place.
    #[arg(
        long,
        env = "AUTH_GOOGLE_AUTH_URL",
        default_value = "https://accounts.google.com/o/oauth2/v2/auth"
    )]
    pub google_auth_url: String,

    /// Google's token endpoint. Overridable for tests; production leaves
    /// the default.
    #[arg(
        long,
        env = "AUTH_GOOGLE_TOKEN_URL",
        default_value = "https://oauth2.googleapis.com/token"
    )]
    pub google_token_url: String,

    /// Google's JWKS endpoint. Overridable for tests; production leaves
    /// the default.
    #[arg(
        long,
        env = "AUTH_GOOGLE_JWKS_URL",
        default_value = "https://www.googleapis.com/oauth2/v3/certs"
    )]
    pub google_jwks_url: String,

    /// Expected `iss` claim on Google ID tokens. Overridable for tests
    /// (mock provider uses its own loopback base URL); production leaves
    /// the default — Google emits this exact string per its OIDC
    /// discovery document.
    #[arg(
        long,
        env = "AUTH_GOOGLE_ISSUER",
        default_value = "https://accounts.google.com"
    )]
    pub google_issuer: String,

    // ─── GitHub OAuth (optional) ───
    /// GitHub OAuth App client ID. Without it, `/oauth/github/*` routes are
    /// not registered (auth still boots).
    #[arg(long, env = "AUTH_GITHUB_CLIENT_ID")]
    pub github_client_id: Option<String>,

    /// GitHub OAuth App client secret.
    #[arg(long, env = "AUTH_GITHUB_CLIENT_SECRET", hide_env_values = true)]
    pub github_client_secret: Option<String>,

    /// Callback URL registered on the GitHub OAuth App. Must match what's set
    /// in the app's settings.
    #[arg(
        long,
        env = "AUTH_GITHUB_REDIRECT_URI",
        default_value = "https://auth.zeroship.ai/oauth/github/callback"
    )]
    pub github_redirect_uri: String,

    /// GitHub's authorize endpoint. Overridable for the federation e2e
    /// tests; production deployments leave the default in place.
    #[arg(
        long,
        env = "AUTH_GITHUB_AUTHORIZE_URL",
        default_value = "https://github.com/login/oauth/authorize"
    )]
    pub github_authorize_url: String,

    /// GitHub's token endpoint. Overridable for tests.
    #[arg(
        long,
        env = "AUTH_GITHUB_TOKEN_URL",
        default_value = "https://github.com/login/oauth/access_token"
    )]
    pub github_token_url: String,

    /// GitHub's `/user` endpoint. Overridable for tests.
    #[arg(
        long,
        env = "AUTH_GITHUB_USER_URL",
        default_value = "https://api.github.com/user"
    )]
    pub github_user_url: String,

    /// GitHub's `/user/emails` endpoint. Overridable for tests.
    #[arg(
        long,
        env = "AUTH_GITHUB_EMAILS_URL",
        default_value = "https://api.github.com/user/emails"
    )]
    pub github_emails_url: String,

    // ─── Mailer (driver selection + per-driver creds) ────────────────────
    /// Mailer driver: `stdout` (dev default) | `smtp` | `resend`.
    #[arg(long, env = "AUTH_MAILER", default_value = "stdout")]
    pub mailer: String,

    /// SMTP relay hostname (required when `--mailer=smtp`).
    #[arg(long, env = "AUTH_SMTP_HOST")]
    pub smtp_host: Option<String>,

    /// SMTP port. Defaults to 587 (STARTTLS); use 465 for implicit SMTPS.
    #[arg(long, env = "AUTH_SMTP_PORT", default_value = "587")]
    pub smtp_port: u16,

    /// SMTP username (optional — server may allow unauthenticated relays).
    #[arg(long, env = "AUTH_SMTP_USERNAME")]
    pub smtp_username: Option<String>,

    /// SMTP password (paired with `--smtp-username`).
    #[arg(long, env = "AUTH_SMTP_PASSWORD", hide_env_values = true)]
    pub smtp_password: Option<String>,

    /// `true` ⇒ open plaintext then upgrade with STARTTLS (port 587).
    /// `false` ⇒ open implicit TLS / SMTPS (port 465).
    #[arg(long, env = "AUTH_SMTP_STARTTLS", default_value = "true")]
    pub smtp_starttls: bool,

    /// Resend API key (required when `--mailer=resend`).
    #[arg(long, env = "AUTH_RESEND_API_KEY", hide_env_values = true)]
    pub resend_api_key: Option<String>,

    /// `From` address every transactional mail uses.
    #[arg(
        long,
        env = "AUTH_MAIL_FROM_EMAIL",
        default_value = "auth@zeroship.ai"
    )]
    pub mail_from_email: String,

    /// `From` display name every transactional mail uses.
    #[arg(long, env = "AUTH_MAIL_FROM_NAME", default_value = "zeroship")]
    pub mail_from_name: String,

    /// Public, externally-reachable origin of this auth server. Used when
    /// constructing absolute URLs embedded in outbound email (e.g. the
    /// magic-link href). Distinct from [`Self::addr`] — that's the bind
    /// address (`0.0.0.0:9092` in prod, which is NOT a real origin).
    ///
    /// Defaults to the dev-loopback value; production deployments MUST
    /// override with the auth host, including scheme + (optional) port.
    #[arg(
        long,
        env = "AUTH_PUBLIC_URL",
        default_value = "http://localhost:9092"
    )]
    pub public_url: String,

    // ─── Postmark webhook (bounce/complaint receiver, P5-U7) ─────────────
    /// HTTP Basic-auth username Postmark must present on every
    /// `POST /webhooks/postmark` request. Configured per-server in the
    /// Postmark dashboard's webhook settings. When unset, the webhook
    /// handler responds with 401 so misrouted traffic doesn't silently
    /// succeed.
    #[arg(long, env = "AUTH_POSTMARK_WEBHOOK_USER")]
    pub postmark_webhook_user: Option<String>,

    /// HTTP Basic-auth password paired with [`Self::postmark_webhook_user`].
    /// See that field for the rationale.
    #[arg(
        long,
        env = "AUTH_POSTMARK_WEBHOOK_PASSWORD",
        hide_env_values = true
    )]
    pub postmark_webhook_password: Option<String>,

    // ─── Cron (P6-U1: jwk_rotation; future units add audit retention) ───
    /// Days between JWK rotations. Once a set's `auth.cron_state` row is
    /// older than this, the next cron tick prepends fresh keys and they
    /// become the active signers (hydra signs with the head of the
    /// list). 90 days mirrors the OIDC operator handbook default.
    #[arg(long, env = "AUTH_JWK_ROTATION_DAYS", default_value = "90")]
    pub jwk_rotation_days: i64,

    /// Days to retain outgoing keys past their rotation. Old keys are
    /// retired once `(rotation_days + retain_days)` has elapsed since
    /// the most recent rotation — long enough for any access token
    /// signed by the outgoing key to expire (default 31 ≫ 1 h access
    /// token TTL, ≫ typical refresh window).
    #[arg(long, env = "AUTH_JWK_RETAIN_DAYS", default_value = "31")]
    pub jwk_retain_days: i64,

    /// Cron tick interval in seconds. Default 86400 (24 h). Operators
    /// drop this to seconds in staging/integration tests so a cron
    /// behaviour change is observable inside a single test run.
    #[arg(long, env = "AUTH_CRON_TICK_SECS", default_value = "86400")]
    pub cron_tick_secs: u64,

    /// Audit-retention sweeper tick interval in seconds. Default 3600
    /// (hourly). The sweep itself is cheap (one indexed DELETE per
    /// bucket) so hourly cadence keeps the table close to its hot-tier
    /// shape without making the sweeper hot. Operators can drop this
    /// for tests; production should leave the default.
    #[arg(
        long,
        env = "AUTH_AUDIT_RETENTION_CHECK_SECS",
        default_value = "3600"
    )]
    pub audit_retention_check_secs: u64,
}

impl AuthConfig {
    /// Apply the shared `[auth]` file overlay to fields that support it.
    ///
    /// Values already supplied by CLI flags or environment variables remain
    /// authoritative. Missing values fall back to the auth server defaults.
    pub fn resolve_file_overlay(&mut self, auth: AuthSection) {
        self.hydra_admin_url = Some(
            self.hydra_admin_url
                .take()
                .or(auth.hydra_admin_url)
                .unwrap_or_else(|| DEFAULT_HYDRA_ADMIN_URL.to_string()),
        );
        self.hydra_public_url = Some(
            self.hydra_public_url
                .take()
                .or(auth.hydra_public_url)
                .unwrap_or_else(|| DEFAULT_HYDRA_PUBLIC_URL.to_string()),
        );
    }

    /// Resolved Hydra admin API base URL.
    #[must_use]
    pub fn hydra_admin_url(&self) -> &str {
        self.hydra_admin_url
            .as_deref()
            .unwrap_or(DEFAULT_HYDRA_ADMIN_URL)
    }

    /// Resolved Hydra public issuer/base URL.
    #[must_use]
    pub fn hydra_public_url(&self) -> &str {
        self.hydra_public_url
            .as_deref()
            .unwrap_or(DEFAULT_HYDRA_PUBLIC_URL)
    }

    /// External origin of this auth server (no trailing slash). Returns
    /// [`Self::public_url`] with any trailing `/` trimmed so callers can
    /// freely concatenate `/magic/verify?…`.
    #[must_use]
    pub fn public_url(&self) -> String {
        self.public_url.trim_end_matches('/').to_string()
    }
}

pub fn validate_stash_key(cfg: &AuthConfig) -> Result<(), String> {
    if cfg.insecure_dev {
        if cfg.stash_signing_key.starts_with("dev-only-") {
            tracing::warn!(
                "AUTH_STASH_SIGNING_KEY is the dev default — OK only because --insecure-dev=true"
            );
        }
        return Ok(());
    }

    if cfg.stash_signing_key.starts_with("dev-only-") {
        return Err(
            "AUTH_STASH_SIGNING_KEY is the dev default; refusing to boot without --insecure-dev=true. Set a strong (≥32 byte) value."
                .to_string(),
        );
    }

    if cfg.stash_signing_key.len() < 32 {
        return Err(format!(
            "AUTH_STASH_SIGNING_KEY is too short ({} bytes); minimum 32 bytes. Set a stronger key or pass --insecure-dev=true.",
            cfg.stash_signing_key.len()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use zeroship_core::config::FileConfig;

    static HYDRA_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn write(name: &str, contents: &str) -> Self {
            static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

            let path = std::env::temp_dir().join(format!(
                "zeroship-auth-config-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(path.as_path(), contents).expect("write temp config");
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.path.as_path());
        }
    }

    fn set_env_opt(key: &str, value: Option<&str>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    fn restore_env(key: &str, value: Option<OsString>) {
        if let Some(value) = value {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    fn with_hydra_env<T>(
        hydra_admin_url: Option<&str>,
        hydra_public_url: Option<&str>,
        f: impl FnOnce() -> T,
    ) -> T {
        let _guard = HYDRA_ENV_LOCK.lock().expect("hydra env lock poisoned");
        let old_admin = std::env::var_os("HYDRA_ADMIN_URL");
        let old_public = std::env::var_os("HYDRA_PUBLIC_URL");

        set_env_opt("HYDRA_ADMIN_URL", hydra_admin_url);
        set_env_opt("HYDRA_PUBLIC_URL", hydra_public_url);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        restore_env("HYDRA_ADMIN_URL", old_admin);
        restore_env("HYDRA_PUBLIC_URL", old_public);

        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn resolve_from_file(mut cfg: AuthConfig) -> AuthConfig {
        let file = FileConfig::load(cfg.config_path.as_deref()).expect("load config file");
        cfg.resolve_file_overlay(file.auth);
        cfg
    }

    fn test_config() -> AuthConfig {
        AuthConfig::parse_from(["zeroship-auth", "--db-url", "postgres://test"])
    }

    #[test]
    fn stash_key_default_rejected_in_production() {
        let cfg = test_config();

        assert!(validate_stash_key(&cfg).is_err());
    }

    #[test]
    fn stash_key_default_accepted_in_dev() {
        let mut cfg = test_config();
        cfg.insecure_dev = true;

        assert!(validate_stash_key(&cfg).is_ok());
    }

    #[test]
    fn stash_key_short_rejected_in_production() {
        let mut cfg = test_config();
        cfg.stash_signing_key = "a".repeat(20);

        assert!(validate_stash_key(&cfg).is_err());
    }

    #[test]
    fn stash_key_strong_accepted_in_production() {
        let mut cfg = test_config();
        cfg.stash_signing_key = "0123456789abcdef0123456789abcdef".to_string();

        assert!(validate_stash_key(&cfg).is_ok());
    }

    #[test]
    fn allow_remote_hydra_admin_flag_accepts_bare_switch() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--allow-remote-hydra-admin",
        ]);

        assert!(cfg.allow_remote_hydra_admin);
    }

    #[test]
    fn allow_remote_hydra_admin_flag_accepts_true_value() {
        let cfg = AuthConfig::parse_from([
            "zeroship-auth",
            "--db-url",
            "postgres://test",
            "--allow-remote-hydra-admin=true",
        ]);

        assert!(cfg.allow_remote_hydra_admin);
    }

    #[test]
    fn auth_file_overlay_supplies_hydra_urls_when_unset() {
        with_hydra_env(None, None, || {
            let file = TempFile::write(
                "auth-overlay.toml",
                r#"
[auth]
hydra_admin_url = "http://hydra-file:4445"
hydra_public_url = "https://hydra-file.example"
"#,
            );

            let cfg = resolve_from_file(AuthConfig::parse_from([
                "zeroship-auth",
                "--db-url",
                "postgres://test",
                "--config",
                file.path.to_str().expect("utf-8 temp path"),
            ]));

            assert_eq!(cfg.hydra_admin_url(), "http://hydra-file:4445");
            assert_eq!(cfg.hydra_public_url(), "https://hydra-file.example");
        });
    }

    #[test]
    fn cli_hydra_urls_override_file_overlay() {
        with_hydra_env(None, None, || {
            let file = TempFile::write(
                "auth-cli-override.toml",
                r#"
[auth]
hydra_admin_url = "http://hydra-file:4445"
hydra_public_url = "https://hydra-file.example"
"#,
            );

            let cfg = resolve_from_file(AuthConfig::parse_from([
                "zeroship-auth",
                "--db-url",
                "postgres://test",
                "--config",
                file.path.to_str().expect("utf-8 temp path"),
                "--hydra-admin-url",
                "http://hydra-cli:4445",
                "--hydra-public-url",
                "https://hydra-cli.example",
            ]));

            assert_eq!(cfg.hydra_admin_url(), "http://hydra-cli:4445");
            assert_eq!(cfg.hydra_public_url(), "https://hydra-cli.example");
        });
    }

    #[test]
    fn env_hydra_urls_override_file_overlay() {
        with_hydra_env(
            Some("http://hydra-env:4445"),
            Some("https://hydra-env.example"),
            || {
                let file = TempFile::write(
                    "auth-env-override.toml",
                    r#"
[auth]
hydra_admin_url = "http://hydra-file:4445"
hydra_public_url = "https://hydra-file.example"
"#,
                );

                let cfg = resolve_from_file(AuthConfig::parse_from([
                    "zeroship-auth",
                    "--db-url",
                    "postgres://test",
                    "--config",
                    file.path.to_str().expect("utf-8 temp path"),
                ]));

                assert_eq!(cfg.hydra_admin_url(), "http://hydra-env:4445");
                assert_eq!(cfg.hydra_public_url(), "https://hydra-env.example");
            },
        );
    }
}
