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
}
