use super::{backend_for_url, BackendUrl};
use crate::{
    backend::{BackendHandle, PostgresBackend},
    encryption::ProjectKeySource,
    error::DbError,
};
use futures::future::LocalBoxFuture;
use sha2::{Digest, Sha256};
use std::{cell::Cell, fmt, num::NonZeroUsize, rc::Rc, sync::Arc};
use zeroship_data_sql::compile::SqlDialect;

/// Host-defined backend construction. Configuration crosses worker threads;
/// opening happens on the destination compio thread and returns a local handle.
pub trait BackendFactory: Send + Sync + 'static {
    fn dialect(&self) -> SqlDialect;
    fn connect(&self, keys: ProjectKeySource) -> LocalBoxFuture<'_, Result<BackendHandle, DbError>>;
}

/// Opaque identity for configuration that may share a local backend.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionIdentity([u8; 32]);
impl ConnectionIdentity {
    fn new(configuration: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"zeroship.orm.connection\0");
        hash.update(configuration.as_bytes());
        Self(hash.finalize().into())
    }
}
impl fmt::Debug for ConnectionIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ConnectionIdentity(<opaque>)")
    }
}

/// Validated connection configuration shared by native hosts and V8 adapters.
/// Debug output excludes credentials and custom factory internals.
#[derive(Clone)]
pub struct ConnectionFactory {
    identity: ConnectionIdentity,
    factory: Arc<dyn BackendFactory>,
    url: Option<String>,
}
impl fmt::Debug for ConnectionFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionFactory")
            .field("identity", &self.identity)
            .field("dialect", &self.dialect())
            .finish_non_exhaustive()
    }
}
impl ConnectionFactory {
    /// Register a custom factory. The identity must distinguish configurations
    /// that cannot safely share a backend, including credentials and routing.
    pub fn new(identity: &str, factory: impl BackendFactory) -> Self {
        Self {
            identity: ConnectionIdentity::new(&format!("custom\0{identity}")),
            factory: Arc::new(factory),
            url: None,
        }
    }
    /// Validate built-in configuration without opening a database.
    pub fn for_url(url: &str) -> Result<Self, DbError> {
        Self::for_url_with_limit(url, None)
    }
    pub(super) fn for_url_with_limit(
        url: &str,
        limit: Option<NonZeroUsize>,
    ) -> Result<Self, DbError> {
        let selection = backend_for_url(url)?;
        let url = url.trim().to_owned();
        let capacity = limit
            .map(NonZeroUsize::get)
            .unwrap_or_else(crate::backend::postgres::default_pool_capacity);
        Ok(Self {
            identity: ConnectionIdentity::new(&format!("builtin\0{url}\0{capacity}")),
            factory: Arc::new(BuiltinFactory {
                url: url.clone(),
                selection,
                capacity,
            }),
            url: Some(url),
        })
    }
    pub fn identity(&self) -> ConnectionIdentity {
        self.identity
    }
    pub fn dialect(&self) -> SqlDialect {
        self.factory.dialect()
    }
    /// Built-in URL for host services that still need their own SQL connection.
    /// Custom factories need not expose a URL.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
    pub async fn connect(&self, keys: ProjectKeySource) -> Result<BackendHandle, DbError> {
        let backend = self.factory.connect(keys).await?;
        if backend.dialect() != self.dialect() {
            return Err(DbError::config(
                "backend_dialect_mismatch",
                "factory returned a backend with a different dialect",
            ));
        }
        BACKENDS_OPENED.with(|count| count.set(count.get() + 1));
        Ok(backend)
    }
}

thread_local! {
    static BACKENDS_OPENED: Cell<u64> = const { Cell::new(0) };
}
/// Successful backend opens performed on the calling thread.
#[doc(hidden)]
pub fn backend_open_count() -> u64 {
    BACKENDS_OPENED.with(Cell::get)
}

struct BuiltinFactory {
    url: String,
    selection: BackendUrl,
    capacity: usize,
}
impl BackendFactory for BuiltinFactory {
    fn dialect(&self) -> SqlDialect {
        match self.selection {
            BackendUrl::Postgres => SqlDialect::Postgres,
            BackendUrl::Sqlite { .. } => SqlDialect::Sqlite,
        }
    }
    fn connect(&self, keys: ProjectKeySource) -> LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
        Box::pin(async move {
            match &self.selection {
                BackendUrl::Postgres => Ok(BackendHandle::new(Rc::new(
                    PostgresBackend::connect(&self.url, self.capacity, keys).await?,
                ))),
                BackendUrl::Sqlite { path } => Ok(BackendHandle::new(Rc::new(
                    crate::backend_selection::open_sqlite_backend(path, keys).await?,
                ))),
            }
        })
    }
}
