//! Key-value storage independent of V8 and platform metering.
//!
//! [`Backend`] defines app-scoped operations and their atomicity contract.
//! Enable `redb` for embedded persistence or `redis` for network storage.
//! Hosts supply the trusted app scope and validate input with [`limits`];
//! language bindings own argument conversion, scheduling, and error presentation.

pub mod backend;
pub mod error;
#[cfg(feature = "redb")]
pub mod holders;
pub mod limits;

#[cfg(feature = "redb")]
pub use backend::redb::STATE_DIR_LOCK_MARKER;
#[cfg(feature = "redb")]
pub use backend::RedbBackend;
#[cfg(feature = "redis")]
pub use backend::Redis;
pub use backend::{Backend, TtlState};
pub use error::KvError;
