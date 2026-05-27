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
}
