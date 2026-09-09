//! Host configuration and built-in driver selection shared by Rust and V8.
use crate::{
    backend::{BackendHandle, PostgresBackend},
    encryption::LocalKeySource,
    error::DbError,
};
use std::{num::NonZeroUsize, path::PathBuf, rc::Rc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendUrl {
    Postgres,
    Sqlite { path: PathBuf },
}

/// Parse configuration without opening a database.
pub fn backend_for_url(url: &str) -> Result<BackendUrl, DbError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(DbError::config_hinted(
            "invalid_database_url",
            "database URL is empty",
            "expected postgres://, postgresql://, sqlite:, file:, :memory:, or a filesystem path",
        ));
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower == ":memory:" {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(":memory:"),
        });
    }
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return Ok(BackendUrl::Postgres);
    }
    if lower.starts_with("sqlite://") {
        // SQLite URLs are always local-file selectors here, so any URI
        // authority is folded into the filesystem path (`sqlite://host/db`
        // becomes `host/db`, not a remote host lookup).
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(&trimmed["sqlite://".len()..]),
        });
    }
    if lower.starts_with("sqlite:") {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(&trimmed["sqlite:".len()..]),
        });
    }
    if lower.starts_with("file:") {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(&trimmed["file:".len()..]),
        });
    }

    let has_scheme = trimmed
        .split_once(':')
        .map(|(scheme, _)| {
            // Windows `C:\...` is rejected here as scheme `c`; that's
            // acceptable because zeroship only targets Linux workers.
            let mut chars = scheme.chars();
            matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
        })
        .unwrap_or(false);
    if has_scheme {
        return Err(DbError::config_hinted(
            "unsupported_database_url_scheme",
            format!("unsupported database URL scheme in `{trimmed}`"),
            "expected postgres://, postgresql://, sqlite:, file:, :memory:, or a filesystem path",
        ));
    }

    Ok(BackendUrl::Sqlite {
        path: PathBuf::from(trimmed),
    })
}

/// Connection configuration. Debug output never contains the database URL.
pub struct ConnectOptions {
    url: String,
    key_source: LocalKeySource,
    max_connections: Option<NonZeroUsize>,
}
impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("max_connections", &self.max_connections)
            .finish_non_exhaustive()
    }
}
impl ConnectOptions {
    pub fn new(url: impl Into<String>, key_source: LocalKeySource) -> Self {
        Self {
            url: url.into(),
            key_source,
            max_connections: None,
        }
    }
    /// Set the capacity of a backend that uses a connection pool.
    pub fn max_connections(mut self, limit: NonZeroUsize) -> Self {
        self.max_connections = Some(limit);
        self
    }
    pub async fn connect(self) -> Result<BackendHandle, DbError> {
        match backend_for_url(&self.url)? {
            BackendUrl::Postgres => {
                let limit = self
                    .max_connections
                    .map(NonZeroUsize::get)
                    .unwrap_or_else(crate::backend::postgres::default_pool_capacity);
                Ok(BackendHandle::new(Rc::new(
                    PostgresBackend::connect(self.url.trim(), limit, self.key_source).await?,
                )))
            }
            BackendUrl::Sqlite { path } => Ok(BackendHandle::new(Rc::new(
                crate::backend_selection::open_sqlite_backend(path, self.key_source).await?,
            ))),
        }
    }
}
