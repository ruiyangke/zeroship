//! Compio-native Redis-compatible connections and deployment routing.
//!
//! [`RedisConfig`] selects standalone, cluster, or Sentinel at runtime.
//! [`RedisClient`] executes commands against the configured primary or slot
//! owner. [`Client`] and [`Pool`] expose direct connections for native callers.
//! DNS, verified TLS, ACL authentication and bounded connection setup are
//! shared by seeds, discovered data nodes and Sentinel connections.
//!
//! Cluster routing follows MOVED/ASK replies and refreshes topology through
//! configured seeds or previously discovered nodes. Sentinel resolves the
//! primary on connection creation and verifies its role on checkout. A primary
//! change invalidates idle connections and prevents older leases being reused.
//!
//! Transport failures never trigger mutation replay: the server may have
//! applied a command before its reply was lost. Reply limits and dirty
//! connection barriers protect framing across errors and cancellation.
//!
//! Discovery authorities are operator-configured. Use TLS to authenticate
//! those authorities and the data servers they advertise. Arbitrary redirect
//! targets are refused unless confirmed by trusted topology discovery.

pub mod config;
pub mod client;
pub mod cluster;
pub mod error;
pub mod pool;
pub mod protocol;
mod transport;
mod sentinel;
mod deployment;

pub use config::{Auth, ConnectionConfig, PoolSettings, RedisConfig, Timeouts, TlsConfig, Topology};
pub use client::Client;
pub use cluster::ClusterClient;
pub use error::{Error, Result};
pub use pool::{Pool, PoolConfig};
pub use deployment::RedisClient;
pub use redis_protocol::resp2::types::OwnedFrame;
