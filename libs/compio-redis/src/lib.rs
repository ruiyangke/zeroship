//! compio-native Redis/Dragonfly client.
//!
//! # Shipped surface
//!
//! Two client types, both backed by a per-node connection [`Pool`]:
//!
//! - [`Client`] — single-node. Owns one TCP connection and runs one command
//!   at a time. Covers the subset zeroship's kv plugin needs: GET, SET (with
//!   PX millisecond TTL, and NX for lock semantics), MGET/MSET, DEL, EXISTS,
//!   INCRBY/DECRBY, STRLEN, (P)EXPIRE, (P)TTL, PERSIST, EVAL, SCAN (prefix
//!   listing — KEYS is banned in prod), and PING/AUTH/SELECT/ASKING housekeeping.
//! - [`ClusterClient`] — cluster-mode. Bootstraps `CLUSTER SLOTS` topology
//!   from a trusted seed, hash-tag-routes keys to the owning node, and
//!   **handles MOVED/ASK redirects** (retrying against the redirect target,
//!   bounded by a redirect cap). `plugin-kv` selects this path for any
//!   cluster URL; it is a publicly exported, production type — not a stub.
//!
//! # Trust model
//!
//! The configured Redis URL/seed is operator-supplied and trusted, but the
//! **server reached at it is not** — it may be compromised, or the plaintext
//! `redis://` link may be MITM'd. The client is hardened against a hostile or
//! tampered server reply:
//!
//! - **Reply size is capped at 64 MB** (`MAX_REPLY_SIZE`, mirroring the
//!   sibling `compio-postgres` driver). An oversized declared bulk/aggregate
//!   length is rejected up front, before the body is buffered, so a single
//!   crafted reply can't OOM the worker.
//! - **Connections are dirty-barriered against reuse-after-error.** A
//!   connection that timed out, errored, or was cancelled mid-reply is
//!   discarded rather than recycled, so the next (possibly cross-tenant)
//!   caller can never read the previous caller's pending reply. Stale idle
//!   connections are PINGed on borrow and replaced if dead.
//! - **Redirect/topology connect targets are allowlisted.** In cluster mode,
//!   the client only opens credentialed connections to addresses present in
//!   the authoritative `CLUSTER SLOTS` topology learned from the trusted seed
//!   (∪ the operator seed set). A malicious node cannot pivot the worker to
//!   an arbitrary address (e.g. cloud metadata, an internal service) via a
//!   forged MOVED/ASK, and an out-of-range MOVED slot is rejected (no
//!   index-panic).
//!
//! # Concrete gaps
//!
//! - **No TLS.** Only plaintext `redis://` is supported; `rediss://` is not
//!   yet implemented, so every above-listed protection still assumes the link
//!   itself can be passively observed/actively MITM'd. Adding `rediss://` +
//!   server-identity pinning is the planned next step and would remove the
//!   plaintext-MITM precondition for the cluster findings.
//!
//! # Deliberately out of scope
//!
//! - Pub/sub
//! - Streams (XADD / XREAD)
//! - Pipelining (one command in flight per connection)
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
