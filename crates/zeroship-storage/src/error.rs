//! Errors shared by Rust callers, backends and language bindings.

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("{0}")]
    InvalidArgument(String),
    #[error("{0}")]
    LimitExceeded(String),
    #[error("{0}")]
    Backend(String),
    #[error("{0}")]
    Stream(String),
}

impl From<String> for StorageError {
    fn from(message: String) -> Self {
        Self::Backend(message)
    }
}

impl From<&str> for StorageError {
    fn from(message: &str) -> Self {
        Self::Backend(message.to_owned())
    }
}
