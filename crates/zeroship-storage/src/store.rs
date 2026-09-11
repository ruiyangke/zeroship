//! Configured storage and namespace-bound Rust operations.

use std::sync::Arc;

use crate::backend::{
    self, BoxByteStream, BoxChunkSource, ChunkResult, ChunkSource, ListPage, ListRequest,
    ObjectMeta,
};
use crate::{limits, Backend, Namespace, StorageBackendConfig, StorageConfigError, StorageError};

/// Limits applied equally to every backend by scoped handles.
#[derive(Debug, Clone, Copy)]
pub struct StorageLimits {
    pub max_buffered_bytes: u64,
    pub max_stream_bytes: u64,
}

impl Default for StorageLimits {
    fn default() -> Self {
        Self {
            max_buffered_bytes: limits::DEFAULT_MAX_OBJECT_BYTES,
            max_stream_bytes: limits::DEFAULT_MAX_STREAM_OBJECT_BYTES,
        }
    }
}

/// Host-owned storage. Share this value and issue scoped handles to callers.
#[derive(Clone)]
pub struct StorageStore {
    backend: Arc<dyn Backend>,
    limits: StorageLimits,
}

impl std::fmt::Debug for StorageStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageStore")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl StorageStore {
    /// Open the configured backend and resolve the host's environment limits.
    ///
    /// # Errors
    /// Returns configuration or credential-resolution errors.
    pub fn open(config: &StorageBackendConfig) -> Result<Self, StorageConfigError> {
        Ok(Self {
            backend: crate::config::build_backend(config)?,
            limits: StorageLimits {
                max_buffered_bytes: limits::max_object_bytes(),
                max_stream_bytes: limits::max_stream_object_bytes(),
            },
        })
    }

    /// Inject a backend using compiled default limits and no environment reads.
    #[must_use]
    pub fn from_backend(backend: Arc<dyn Backend>) -> Self {
        Self {
            backend,
            limits: StorageLimits::default(),
        }
    }

    /// Override the limits for handles issued from this store.
    ///
    /// # Errors
    /// Refuses zero limits.
    pub fn with_limits(mut self, limits: StorageLimits) -> Result<Self, StorageError> {
        if limits.max_buffered_bytes == 0 || limits.max_stream_bytes == 0 {
            return Err(StorageError::InvalidArgument(
                "storage: limits must be positive".into(),
            ));
        }
        self.limits = limits;
        Ok(self)
    }

    /// Fix the caller's namespace before handing it storage operations.
    #[must_use]
    pub fn namespace(&self, namespace: Namespace) -> Storage {
        Storage {
            store: self.clone(),
            namespace,
        }
    }
}

/// A Rust handle whose operations cannot select another namespace.
/// Futures execute on the caller's compio runtime and need not be `Send`.
#[derive(Debug, Clone)]
pub struct Storage {
    store: StorageStore,
    namespace: Namespace,
}

#[allow(
    clippy::future_not_send,
    reason = "Storage futures execute on the calling compio thread."
)]
impl Storage {
    /// The buffered-object limit captured when the host issued this handle.
    #[must_use]
    pub const fn max_buffered_bytes(&self) -> u64 {
        self.store.limits.max_buffered_bytes
    }

    /// Store an in-memory object.
    ///
    /// # Errors
    /// Returns validation, size-limit or backend errors.
    pub async fn put(
        &self,
        bucket: &str,
        key: &str,
        bytes: &[u8],
        content_type: Option<&str>,
    ) -> Result<u64, StorageError> {
        self.validate_object(bucket, key)?;
        if bytes.len() as u64 > self.store.limits.max_buffered_bytes {
            return Err(StorageError::LimitExceeded(
                "storage: buffered object exceeds size limit; use putStream".into(),
            ));
        }
        self.put_stream(
            bucket,
            key,
            Box::new(backend::OnceChunk::new(bytes::Bytes::copy_from_slice(
                bytes,
            ))),
            content_type,
        )
        .await
    }

    /// Read an object into memory, returning `None` for an absent key.
    ///
    /// # Errors
    /// Returns validation, size-limit or backend errors.
    pub async fn get(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, ObjectMeta)>, StorageError> {
        self.validate_object(bucket, key)?;
        self.store
            .backend
            .get(
                self.namespace.as_str(),
                bucket,
                key,
                self.store.limits.max_buffered_bytes,
            )
            .await
    }

    /// Stream an upload with a total-size limit and bounded buffering.
    ///
    /// # Errors
    /// Returns validation, size-limit, source or backend errors.
    pub async fn put_stream(
        &self,
        bucket: &str,
        key: &str,
        body: BoxChunkSource,
        content_type: Option<&str>,
    ) -> Result<u64, StorageError> {
        self.validate_object(bucket, key)?;
        let body = Box::new(LimitedSource {
            inner: body,
            remaining: self.store.limits.max_stream_bytes,
        });
        self.store
            .backend
            .put_stream(self.namespace.as_str(), bucket, key, body, content_type)
            .await
    }

    /// Open a streaming download. The caller owns and drops the returned source.
    ///
    /// # Errors
    /// Returns validation or backend errors.
    pub async fn get_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<(ObjectMeta, BoxByteStream)>, StorageError> {
        self.validate_object(bucket, key)?;
        self.store
            .backend
            .get_stream(self.namespace.as_str(), bucket, key)
            .await
    }

    /// Remove an object, returning whether it existed.
    ///
    /// # Errors
    /// Returns validation or backend errors.
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<bool, StorageError> {
        self.validate_object(bucket, key)?;
        self.store
            .backend
            .delete(self.namespace.as_str(), bucket, key)
            .await
    }

    /// Read a bounded page of literal-prefix matches in this namespace.
    ///
    /// # Errors
    /// Returns validation or backend errors.
    pub async fn list(
        &self,
        bucket: &str,
        mut request: ListRequest<'_>,
    ) -> Result<ListPage, StorageError> {
        backend::validate_list_coords(self.namespace.as_str(), bucket)?;
        backend::validate_list_request(&request)?;
        request.limit = if request.limit == 0 {
            limits::LIST_DEFAULT_LIMIT
        } else {
            request.limit.min(limits::LIST_MAX_LIMIT)
        };
        self.store
            .backend
            .list(self.namespace.as_str(), bucket, request)
            .await
    }

    fn validate_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        backend::validate_object_coords(self.namespace.as_str(), bucket, key)
    }
}

struct LimitedSource {
    inner: BoxChunkSource,
    remaining: u64,
}

#[async_trait::async_trait(?Send)]
impl ChunkSource for LimitedSource {
    async fn next_chunk(&mut self) -> Option<ChunkResult> {
        let chunk = match self.inner.next_chunk().await? {
            Ok(chunk) => chunk,
            Err(error) => return Some(Err(error)),
        };
        let Some(remaining) = self.remaining.checked_sub(chunk.len() as u64) else {
            return Some(Err(StorageError::LimitExceeded(
                "storage: streamed object exceeds size limit".into(),
            )));
        };
        self.remaining = remaining;
        Some(Ok(chunk))
    }
}
