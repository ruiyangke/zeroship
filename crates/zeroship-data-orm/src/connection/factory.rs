use super::{backend_for_url, BackendUrl, SessionAuthority};
use crate::{
    backend::{BackendHandle, PostgresBackend},
    encryption::ProjectKeySource,
    error::DbError,
};
use futures::future::LocalBoxFuture;
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fmt,
    num::NonZeroUsize,
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

/// Host-defined backend construction. Configuration crosses worker threads;
/// opening happens on the destination compio thread and returns a local handle.
pub trait BackendFactory: Send + Sync + 'static {
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration;
    fn connect(&self, keys: ProjectKeySource)
        -> LocalBoxFuture<'_, Result<BackendHandle, DbError>>;
}

/// Opaque identity for configuration that may share a local backend.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConnectionIdentity([u8; 32]);
impl ConnectionIdentity {
    fn new(
        configuration: &str,
        registration: crate::sql::registration::RegistrationIdentity,
    ) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"zeroship.orm.connection\0");
        hash.update(configuration.as_bytes());
        registration.contribute_to(&mut hash);
        Self(hash.finalize().into())
    }

    pub(crate) fn for_backend(
        registration: crate::sql::registration::RegistrationIdentity,
    ) -> Self {
        static NEXT_BACKEND: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT_BACKEND
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .expect("backend identity space exhausted");
        let mut hash = Sha256::new();
        hash.update(b"zeroship.orm.backend\0");
        hash.update(sequence.to_le_bytes());
        registration.contribute_to(&mut hash);
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
    registration: crate::sql::registration::SqlRegistration,
    url: Option<String>,
}
impl fmt::Debug for ConnectionFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectionFactory")
            .field("identity", &self.identity)
            .field("sql", &self.registration)
            .finish_non_exhaustive()
    }
}
impl ConnectionFactory {
    /// Register a custom factory. The identity must distinguish configurations
    /// that cannot safely share a backend, including credentials and routing.
    pub fn new(identity: &str, factory: impl BackendFactory) -> Self {
        let registration = factory.sql_registration();
        Self::custom(identity, factory, registration)
    }
    /// Register compiler and storage codecs with a custom connection factory.
    pub fn with_sql(
        identity: &str,
        factory: impl BackendFactory,
        registration: crate::sql::registration::SqlRegistration,
    ) -> Result<Self, DbError> {
        if factory.sql_registration().family() != registration.family() {
            return Err(DbError::config(
                "factory_sql_mismatch",
                "connection factory and SQL registration use different SQL families",
            ));
        }
        Ok(Self::custom(identity, factory, registration))
    }
    fn custom(
        identity: &str,
        factory: impl BackendFactory,
        registration: crate::sql::registration::SqlRegistration,
    ) -> Self {
        Self {
            identity: ConnectionIdentity::new(
                &format!("custom\0{identity}"),
                registration.identity(),
            ),
            factory: Arc::new(factory),
            registration,
            url: None,
        }
    }
    /// Validate built-in configuration without opening a database.
    pub fn for_url(url: &str) -> Result<Self, DbError> {
        Self::for_url_with_limit(url, None, SessionAuthority::PerAppRole)
    }
    pub(super) fn for_url_with_limit(
        url: &str,
        limit: Option<NonZeroUsize>,
        session_authority: SessionAuthority,
    ) -> Result<Self, DbError> {
        let selection = backend_for_url(url)?;
        let url = url.trim().to_owned();
        let registration = match &selection {
            BackendUrl::Postgres => crate::sql::registration::SqlRegistration::postgres(),
            BackendUrl::Sqlite { .. } => crate::sql::registration::SqlRegistration::sqlite(),
        };
        let capacity = limit
            .map(NonZeroUsize::get)
            .unwrap_or_else(crate::backend::postgres::default_pool_capacity);
        Ok(Self {
            identity: ConnectionIdentity::new(
                &format!("builtin\0{url}\0{capacity}\0{session_authority:?}"),
                registration.identity(),
            ),
            factory: Arc::new(BuiltinFactory {
                url: url.clone(),
                selection,
                capacity,
                session_authority,
            }),
            registration,
            url: Some(url),
        })
    }
    pub fn identity(&self) -> ConnectionIdentity {
        self.identity
    }
    pub fn sql_registration(&self) -> &crate::sql::registration::SqlRegistration {
        &self.registration
    }
    /// Built-in URL for host services that still need their own SQL connection.
    /// Custom factories need not expose a URL.
    pub fn url(&self) -> Option<&str> {
        self.url.as_deref()
    }
    pub async fn connect(&self, keys: ProjectKeySource) -> Result<BackendHandle, DbError> {
        let backend = self.factory.connect(keys).await?;
        if backend.sql_registration().identity() != self.registration.identity() {
            return Err(DbError::config(
                "backend_sql_mismatch",
                "factory returned a backend with a different SQL registration",
            ));
        }
        BACKENDS_OPENED.with(|count| count.set(count.get() + 1));
        Ok(backend.bind_connection_identity(self.identity))
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
    session_authority: SessionAuthority,
}
impl BackendFactory for BuiltinFactory {
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration {
        match self.selection {
            BackendUrl::Postgres => crate::sql::registration::SqlRegistration::postgres(),
            BackendUrl::Sqlite { .. } => crate::sql::registration::SqlRegistration::sqlite(),
        }
    }
    fn connect(
        &self,
        keys: ProjectKeySource,
    ) -> LocalBoxFuture<'_, Result<BackendHandle, DbError>> {
        Box::pin(async move {
            match &self.selection {
                BackendUrl::Postgres => Ok(BackendHandle::new(Rc::new(
                    PostgresBackend::connect_with_session_authority(
                        &self.url,
                        self.capacity,
                        keys,
                        self.session_authority,
                    )
                    .await?,
                ))),
                BackendUrl::Sqlite { path } => Ok(BackendHandle::new(Rc::new(
                    crate::backend_selection::open_sqlite_backend(path, keys).await?,
                ))),
            }
        })
    }
}
