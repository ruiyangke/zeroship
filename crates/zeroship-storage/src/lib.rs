//! Object storage for Rust callers and language bindings.
//!
//! Hosts open a [`StorageStore`] and issue [`Storage`] handles with a fixed
//! [`Namespace`]. The handle validates object coordinates and enforces limits
//! before calling a backend. V8 conversion and metering live in
//! `zeroship-storage-v8`.

pub mod backend;
pub mod config;
mod error;
pub mod limits;
mod namespace;
mod store;

pub use backend::{Backend, LocalFs};
#[cfg(feature = "s3")]
pub use backend::{S3UploadTuning, S3};
pub use config::{StorageBackendConfig, StorageConfigError};
pub use error::StorageError;
pub use namespace::Namespace;
pub use store::{Storage, StorageLimits, StorageStore};

zeroship_core::declare_env_consumer!(
    /// Object storage configuration read by trusted Rust hosts.
    pub StorageConsumer,
    target = "zeroship-storage",
    scope = "storage");
