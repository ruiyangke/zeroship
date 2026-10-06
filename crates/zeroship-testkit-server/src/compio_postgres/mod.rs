//! The `PostgreSQL` servers compio-postgres's test and bench targets dial.
//!
//! `compio-postgres` is a standalone, publishable driver with NO zeroship
//! dependency, and this crate names no database driver, so the driver's own
//! targets lease these servers here without linking a second copy of the
//! driver under test:
//!
//! - [`server`] is the `PostgreSQL` server every suite and live bench dials,
//!   started in Docker and shared by every test process of the worktree.
//! - [`tls`] is the six servers the TLS suites dial, with the CA, CRL and
//!   client identity they are configured from.
//! - [`unix`] is the server the Unix-socket suite reaches through a socket
//!   file on this host.

pub mod server;
pub mod tls;
pub mod unix;
