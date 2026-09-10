//! Backend choices supplied by the host's runtime configuration.

use std::path::PathBuf;

/// Runtime backend selection.
///
/// Hosts resolve configuration files, flags, and
/// environment variables before constructing this value. Cargo features only
/// determine which implementations are available in the executable.
#[derive(Clone)]
pub enum KvConfig {
    Redis { url: String },
    Redb { path: PathBuf },
}

impl KvConfig {
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
