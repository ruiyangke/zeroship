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
            "expected postgres://, postgresql://, sqlite:, file:, or a filesystem path",
        ));
    }

    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        return Ok(BackendUrl::Postgres);
    }
    if let Some(path) = zeroship_core::db_url::sqlite_file_path(trimmed) {
        return Ok(BackendUrl::Sqlite {
            path: PathBuf::from(path),
        });
    }
    let sqlite_selector = lower.starts_with("sqlite:")
        || lower.starts_with("file:")
        || lower == ":memory:"
        || !trimmed.contains(':');
    Err(DbError::config_hinted(
        if sqlite_selector {
            "sqlite_file_required"
        } else {
            "unsupported_database_url_scheme"
        },
        "database configuration must select PostgreSQL or a SQLite file",
        "use postgres://, postgresql://, sqlite:/path/to/database.sqlite, file:/path/to/database.sqlite, or a filesystem path; memory databases and SQLite URI options are unsupported",
    ))
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
