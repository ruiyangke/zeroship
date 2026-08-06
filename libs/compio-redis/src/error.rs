//! Error type for Redis operations.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// TCP / socket failures.
    Io(std::io::Error),
    /// RESP framing / decoding problem.
    Protocol(String),
    /// Server returned `-ERR ...` or `-WRONGTYPE ...` etc.
    Server(String),
    /// URL parse / config problem.
    Config(String),
    /// Connection pool exhausted / failed to acquire.
    Pool(String),
    /// Authentication rejected.
    Auth(String),
    /// Unexpected reply shape (e.g. string where int was expected).
    Unexpected(String),
    /// Cluster topology returned a MOVED redirect — the slot we hashed to
    /// is owned by a different node. Includes the slot + the authoritative
    /// `host:port`. Caller updates its slot map and retries.
    Moved { slot: u16, addr: String },
    /// Cluster topology returned an ASK redirect — transient during online
    /// resharding. The slot is being migrated; caller must connect to
    /// `addr` and issue `ASKING` before retrying exactly this one command.
    Ask { slot: u16, addr: String },
    /// Multi-key command spanned multiple slots. Redis/Dragonfly require
    /// all keys in a single command to hash to the same slot; caller must
    /// split the batch or use hash-tags.
    CrossSlot,
    /// The slot computed from the key isn't mapped in the current
    /// topology. Either the cluster hasn't finished coming up or a
    /// topology refresh is needed.
    NoRoute { slot: u16 },
    /// `CLUSTER SLOTS` either errored or returned a shape we don't understand.
    ClusterBootstrap(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "redis: I/O: {e}"),
            Error::Protocol(m) => write!(f, "redis: protocol: {m}"),
            Error::Server(m) => write!(f, "redis: server: {m}"),
            Error::Config(m) => write!(f, "redis: config: {m}"),
            Error::Pool(m) => write!(f, "redis: pool: {m}"),
            Error::Auth(m) => write!(f, "redis: auth: {m}"),
            Error::Unexpected(m) => write!(f, "redis: unexpected reply: {m}"),
            Error::Moved { slot, addr } => write!(f, "redis: MOVED slot {slot} → {addr}"),
            Error::Ask { slot, addr } => write!(f, "redis: ASK slot {slot} → {addr}"),
            Error::CrossSlot => write!(f, "redis: cross-slot multi-key command not supported in cluster mode"),
            Error::NoRoute { slot } => write!(f, "redis: no route for slot {slot}"),
            Error::ClusterBootstrap(m) => write!(f, "redis: cluster bootstrap: {m}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self { Error::Io(e) }
}

pub type Result<T> = std::result::Result<T, Error>;
