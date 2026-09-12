//! Backend choices supplied by the host's runtime configuration.

pub use compio_redis::{Auth, PoolSettings, RedisConfig, Timeouts, TlsConfig, Topology};
use serde::Deserialize;
use std::path::PathBuf;

/// Runtime backend selection.
///
/// Hosts resolve configuration files, flags, and
/// environment variables before constructing this value. Cargo features only
/// determine which implementations are available in the executable.
#[derive(Clone, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "Hosts construct configuration at startup; keep the Rust configuration API direct."
)]
pub enum KvConfig {
    Redis { redis: RedisConfig },
    Redb { path: PathBuf },
}

impl KvConfig {
    pub fn from_toml(input: &str) -> Result<Self, crate::KvError> {
        let config: Self = toml::from_str(input)
            .map_err(|_| crate::KvError::invalid_argument("invalid KV configuration TOML"))?;
        if let Self::Redis { redis } = &config {
            redis
                .validate()
                .map_err(|error| crate::KvError::invalid_argument(error.to_string()))?;
        }
        Ok(config)
    }
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Redis { .. } => "redis",
            Self::Redb { .. } => "redb",
        }
    }
}

impl std::fmt::Debug for KvConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redis { .. } => f.debug_struct("Redis").finish_non_exhaustive(),
            Self::Redb { path } => f.debug_struct("Redb").field("path", path).finish(),
        }
    }
}
