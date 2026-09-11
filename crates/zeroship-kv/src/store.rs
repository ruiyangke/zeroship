//! Configuration-driven storage and bound Rust handles.

use std::sync::Arc;

use crate::{limits, Backend, KvConfig, KvError, Namespace, TtlState};

/// Process-owned storage. Clones share the backend; opening a store again
/// constructs another backend and, for redb, attempts another file lock.
///
/// ```
/// use zeroship_kv::{Kv, KvConfig, KvError, KvStore, Namespace};
///
/// fn configure(config: &KvConfig) -> Result<Kv, KvError> {
///     let store = KvStore::open(config)?;
///     Ok(store.namespace(Namespace::platform("control")?))
/// }
///
/// async fn refresh(kv: &Kv) -> Result<(), KvError> {
///     kv.set("refresh-status", "ready", None).await
/// }
/// ```
#[derive(Clone)]
pub struct KvStore {
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for KvStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Backend diagnostics may contain connection credentials.
        f.debug_struct("KvStore").finish_non_exhaustive()
    }
}

impl KvStore {
    /// Construct the configured backend without consulting process globals.
    ///
    /// Embedded storage opens immediately; Redis connections are established
    /// lazily on the compio thread that performs an operation. An unavailable
    /// implementation or a failed open is an error, never a backend fallback.
    ///
    /// # Errors
    /// Returns `InvalidArgument` for a missing implementation or invalid Redis
    /// configuration, and `Connection` when embedded storage cannot be opened.
    pub fn open(config: &KvConfig) -> Result<Self, KvError> {
        match config {
            #[cfg(feature = "redis")]
            KvConfig::Redis { redis } => {
                redis
                    .validate()
                    .map_err(|error| KvError::invalid_argument(error.to_string()))?;
                Ok(Self::from_backend(Arc::new(crate::Redis::new(
                    redis.clone(),
                ))))
            }
            #[cfg(not(feature = "redis"))]
            KvConfig::Redis { .. } => Err(KvError::invalid_argument(
                "kv: the configured Redis backend requires the redis Cargo feature",
            )),
            #[cfg(feature = "redb")]
            KvConfig::Redb { path } => {
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        KvError::connection(format!(
                            "kv: cannot create '{}': {error}",
                            parent.display()
                        ))
                    })?;
                }
                Ok(Self::from_backend(Arc::new(crate::RedbBackend::open(
                    path,
                )?)))
            }
            #[cfg(not(feature = "redb"))]
            KvConfig::Redb { .. } => Err(KvError::invalid_argument(
                "kv: the configured redb backend requires the redb Cargo feature",
            )),
        }
    }

    /// Inject a host-owned implementation, including custom backends and test
    /// fixtures. Application code receives scoped handles from the store.
    #[must_use]
    pub fn from_backend(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    /// Bind a namespace chosen by the trusted host.
    #[must_use]
    pub fn namespace(&self, namespace: Namespace) -> Kv {
        Kv {
            backend: Arc::clone(&self.backend),
            namespace,
        }
    }
}

/// Cloneable Rust KV handle with its namespace fixed at construction.
///
/// Operations share the same validation and backend path as the V8 binding.
/// Values are strings; callers own serialization. TTL arguments are milliseconds.
/// Futures run on the caller's compio thread and need not be `Send`.
#[derive(Clone)]
pub struct Kv {
    backend: Arc<dyn Backend>,
    namespace: Namespace,
}

impl std::fmt::Debug for Kv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kv")
            .field("namespace", &self.namespace)
            .finish_non_exhaustive()
    }
}

#[allow(
    clippy::future_not_send,
    reason = "Backend futures execute on the calling compio thread."
)]
impl Kv {
    /// Read a string, returning `None` when the key is absent or expired.
    ///
    /// # Errors
    /// Returns key-validation or backend errors.
    pub async fn get(&self, key: &str) -> Result<Option<String>, KvError> {
        limits::validate_key(key)?;
        self.backend.get(self.namespace.as_str(), key).await
    }

    /// Replace the value and expiry. `None` stores without expiry.
    ///
    /// # Errors
    /// Returns key, value, or TTL validation errors, or a backend error.
    pub async fn set(&self, key: &str, value: &str, ttl_ms: Option<u64>) -> Result<(), KvError> {
        limits::validate_key(key)?;
        limits::validate_value(value)?;
        validate_optional_ttl(ttl_ms)?;
        self.backend
            .set(self.namespace.as_str(), key, value, ttl_ms)
            .await
    }

    /// Delete a key, reporting whether the backend removed an entry.
    ///
    /// # Errors
    /// Returns key-validation or backend errors.
    pub async fn delete(&self, key: &str) -> Result<bool, KvError> {
        limits::validate_key(key)?;
        self.backend.delete(self.namespace.as_str(), key).await
    }

    /// Increment atomically, applying the TTL only when creating the key.
    ///
    /// # Errors
    /// Returns validation or backend errors, including `NonNumeric` and `Overflow`.
    pub async fn incr(&self, key: &str, delta: i64, ttl_ms: Option<u64>) -> Result<i64, KvError> {
        limits::validate_key(key)?;
        validate_optional_ttl(ttl_ms)?;
        self.backend
            .incr(self.namespace.as_str(), key, delta, ttl_ms)
            .await
    }

    /// Store only if absent, reporting whether this call stored the value.
    ///
    /// # Errors
    /// Returns key, value, or TTL validation errors, or a backend error.
    pub async fn set_if_absent(
        &self,
        key: &str,
        value: &str,
        ttl_ms: Option<u64>,
    ) -> Result<bool, KvError> {
        limits::validate_key(key)?;
        limits::validate_value(value)?;
        validate_optional_ttl(ttl_ms)?;
        self.backend
            .set_if_absent(self.namespace.as_str(), key, value, ttl_ms)
            .await
    }

    /// Change an existing key's expiry, returning false if it is missing.
    ///
    /// # Errors
    /// Returns key or TTL validation errors, or a backend error.
    pub async fn expire(&self, key: &str, ttl_ms: u64) -> Result<bool, KvError> {
        limits::validate_key(key)?;
        limits::validate_ttl_ms(ttl_ms)?;
        self.backend
            .expire(self.namespace.as_str(), key, ttl_ms)
            .await
    }

    /// Inspect a key's existence and expiry.
    ///
    /// # Errors
    /// Returns key-validation or backend errors.
    pub async fn ttl(&self, key: &str) -> Result<TtlState, KvError> {
        limits::validate_key(key)?;
        self.backend.ttl(self.namespace.as_str(), key).await
    }

    /// Remove an expiry, returning false for missing or already persistent keys.
    ///
    /// # Errors
    /// Returns key-validation or backend errors.
    pub async fn persist(&self, key: &str) -> Result<bool, KvError> {
        limits::validate_key(key)?;
        self.backend.persist(self.namespace.as_str(), key).await
    }

    /// List literal-prefix matches. Follow the returned cursor until `None`;
    /// the limit is a page-size hint and does not imply completion.
    /// Zero selects [`limits::LIST_DEFAULT_LIMIT`]; larger hints are capped at
    /// [`limits::LIST_MAX_LIMIT`].
    ///
    /// # Errors
    /// Returns backend errors, including invalid-cursor errors.
    pub async fn list(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<String>, Option<String>), KvError> {
        self.backend
            .list(
                self.namespace.as_str(),
                prefix,
                cursor,
                limits::normalize_list_limit(limit),
            )
            .await
    }
}

fn validate_optional_ttl(ttl_ms: Option<u64>) -> Result<(), KvError> {
    if let Some(ttl_ms) = ttl_ms {
        limits::validate_ttl_ms(ttl_ms)?;
    }
    Ok(())
}
