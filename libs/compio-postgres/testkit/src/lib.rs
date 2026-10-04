//! Dev-only fixtures shared by compio-postgres's test and bench targets.
//!
//! `compio-postgres` is a standalone, publishable driver with NO zeroship
//! dependency, so its test targets cannot use the workspace's own fixtures,
//! which link this driver. This crate is the one place its targets reach for
//! what they share, so no target carries a copy of its own:
//!
//! - [`server`] is the PostgreSQL server every suite and live bench dials,
//!   started in Docker and shared by every test process of the worktree.
//! - [`tls`] is the six servers the TLS suites dial, with the CA, CRL and
//!   client identity they are configured from.
//! - [`unix`] is the server the Unix-socket suite reaches through a socket
//!   file on this host.
//! - [`env`] is the sealed key enum for the one environment name a target may
//!   read, and the one accessor that reads it.

pub mod env;
pub mod server;
pub mod tls;
pub mod unix;
