//! Key-value storage independent of V8 and platform metering.
//!
//! Hosts open a [`KvStore`] from runtime [`KvConfig`] and issue scoped [`Kv`]
//! handles to Rust callers and language bindings. [`Namespace`] separates app
//! and platform keyspaces. [`Backend`] defines the low-level storage contract.
//! Enable backend implementations with Cargo features; configuration chooses
//! which implementation the host opens.

pub mod backend;
mod config;
pub mod error;
#[cfg(feature = "redb")]
pub mod holders;
pub mod limits;
mod namespace;
mod store;

pub use config::{Auth, KvConfig, PoolSettings, RedisConfig, Timeouts, TlsConfig, Topology};
pub use namespace::Namespace;
pub use store::{Kv, KvStore};

#[cfg(feature = "redb")]
pub use backend::redb::STATE_DIR_LOCK_MARKER;
#[cfg(feature = "redb")]
pub use backend::RedbBackend;
#[cfg(feature = "redis")]
pub use backend::Redis;
pub use backend::{Backend, TtlState};
pub use error::KvError;
