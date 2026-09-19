//! Host configuration and built-in driver selection shared by Rust and V8.
use crate::{backend::BackendHandle, encryption::ProjectKeySource, error::DbError};
use std::{num::NonZeroUsize, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendUrl {
    Postgres,
    Sqlite { path: PathBuf },
}

/// Authority applied to sessions opened by a built-in backend.
///
/// Worker databases use [`Self::PerBindingRole`]. A trusted native service whose
/// connection already authenticates as its provisioned database role can use
/// [`Self::Connection`] to retain that role while the ORM still installs its
/// transaction-scoped resource limits.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub(crate) enum SessionAuthority {
    #[default]
    PerBindingRole,
    Connection,
}

/// Parse configuration without opening a database.
pub fn backend_for_url(url: &str) -> Result<BackendUrl, DbError> {
    CONFIGURATION_PARSES.with(|count| count.set(count.get() + 1));
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

mod factory;
mod local;
pub use factory::{backend_open_count, BackendFactory, ConnectionFactory, ConnectionIdentity};
pub use local::LocalConnection;

thread_local! {
    static CONFIGURATION_PARSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Configuration parses performed on the calling thread.
#[doc(hidden)]
pub fn configuration_parse_count() -> u64 {
    CONFIGURATION_PARSES.with(std::cell::Cell::get)
}

/// Connection configuration. Debug output never contains the database URL.
pub struct ConnectOptions {
    url: String,
    key_source: ProjectKeySource,
    max_connections: Option<NonZeroUsize>,
    session_authority: SessionAuthority,
    transaction_setting_namespaces: Vec<&'static str>,
}
impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("max_connections", &self.max_connections)
            .field("session_authority", &self.session_authority)
            .field(
                "transaction_setting_namespaces",
                &self.transaction_setting_namespaces,
            )
            .finish_non_exhaustive()
    }
}
impl ConnectOptions {
    pub fn new(url: impl Into<String>, key_source: ProjectKeySource) -> Self {
        Self {
            url: url.into(),
            key_source,
            max_connections: None,
            session_authority: SessionAuthority::PerBindingRole,
            transaction_setting_namespaces: Vec::new(),
        }
    }
    /// Set the capacity of a backend that uses a connection pool.
    pub fn max_connections(mut self, limit: NonZeroUsize) -> Self {
        self.max_connections = Some(limit);
        self
    }
    /// Select authority already established by this connection's credentials.
    ///
    /// Platform services use this with their own service-role database URL.
    /// The option accepts no role name and cannot elevate the connection.
    pub fn connection_authority(mut self) -> Self {
        self.session_authority = SessionAuthority::Connection;
        self
    }
    /// Allow transactions on this connection to set custom settings under
    /// `namespace`. Undeclared namespaces are refused, so this surface cannot
    /// reach a setting the host did not choose to expose. Repeat to declare
    /// several.
    pub fn transaction_setting_namespace(mut self, namespace: &'static str) -> Self {
        self.transaction_setting_namespaces.push(namespace);
        self
    }
    pub async fn connect(self) -> Result<BackendHandle, DbError> {
        for namespace in &self.transaction_setting_namespaces {
            if !crate::sql::coordination::valid_setting_namespace(namespace) {
                return Err(DbError::config_hinted(
                    "invalid_transaction_setting_namespace",
                    format!("'{namespace}' is not a transaction setting namespace"),
                    "A namespace is a lowercase identifier, such as app_ns.",
                ));
            }
        }
        let namespaces: std::rc::Rc<[&'static str]> =
            self.transaction_setting_namespaces.into();
        Ok(ConnectionFactory::for_url_with_limit(
            &self.url,
            self.max_connections,
            self.session_authority,
        )?
        .connect(self.key_source)
        .await?
        .declare_transaction_setting_namespaces(namespaces))
    }
}

#[cfg(test)]
mod tests;
