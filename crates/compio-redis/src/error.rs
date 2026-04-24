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
