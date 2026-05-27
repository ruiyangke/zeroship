//! Auth server configuration.

use clap::Parser;

#[derive(Debug, Clone, Parser)]
#[command(name = "zeroship-auth")]
pub struct AuthConfig {
    /// Listen address.
    #[arg(long, env = "AUTH_ADDR", default_value = "0.0.0.0:9092")]
    pub addr: String,

    /// `PostgreSQL` DSN.
    #[arg(long, env = "AUTH_DB_URL")]
    pub db_url: String,

    /// Hydra admin base URL (loopback).
    #[arg(long, env = "AUTH_HYDRA_ADMIN", default_value = "http://127.0.0.1:4445")]
    pub hydra_admin: String,

    /// Hydra public base URL (issuer).
    #[arg(long, env = "AUTH_HYDRA_PUBLIC", default_value = "https://auth.zeroship.ai")]
    pub hydra_public: String,

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
    /// this explicitly. The dev default loudly warns at boot.
    #[arg(
        long,
        env = "AUTH_STASH_SIGNING_KEY",
        default_value = "dev-only-stash-signing-key-not-for-production-use!!"
    )]
    pub stash_signing_key: String,

    // ─── Google OAuth (optional — federation routes registered only when set) ───
    /// Google OAuth 2.0 client ID. Without it, `/oauth/google/*` routes are
    /// not registered (auth still boots).
    #[arg(long, env = "AUTH_GOOGLE_CLIENT_ID")]
    pub google_client_id: Option<String>,

    /// Google OAuth 2.0 client secret.
    #[arg(long, env = "AUTH_GOOGLE_CLIENT_SECRET")]
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
    #[arg(long, env = "AUTH_GITHUB_CLIENT_SECRET")]
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
    #[arg(long, env = "AUTH_SMTP_PASSWORD")]
    pub smtp_password: Option<String>,

    /// `true` ⇒ open plaintext then upgrade with STARTTLS (port 587).
    /// `false` ⇒ open implicit TLS / SMTPS (port 465).
    #[arg(long, env = "AUTH_SMTP_STARTTLS", default_value = "true")]
    pub smtp_starttls: bool,

    /// Resend API key (required when `--mailer=resend`).
    #[arg(long, env = "AUTH_RESEND_API_KEY")]
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
}
