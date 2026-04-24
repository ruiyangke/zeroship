//! Minimal compio-native Redis client.
//!
//! Covers the command subset zeroship's kv plugin needs:
//! - GET, SET (with PX for millisecond TTL)
//! - DEL, EXISTS
//! - INCR, INCRBY
//! - EXPIRE, PEXPIRE, TTL, PTTL
//! - SCAN (for prefix listing — KEYS is banned in prod)
//! - PING, AUTH, SELECT (connection housekeeping)
//!
//! What it deliberately doesn't do (out of scope for v1):
//! - Cluster mode (no MOVED/ASK redirection handling)
//! - Pub/sub
//! - Streams (XADD / XREAD)
//! - Pipelining
//! - TLS (add when we need rediss://)
//! - Transactions (MULTI/EXEC)
//!
//! Zero tokio: TCP via compio::net, framing via the `redis-protocol`
//! parsing crate (pure parsing, no runtime).

pub mod client;
pub mod cluster;
pub mod error;
pub mod pool;
pub mod protocol;

pub use client::Client;
pub use cluster::ClusterClient;
pub use error::{Error, Result};
pub use pool::{Pool, PoolConfig};
