//! Shared test fixtures for the zeroship workspace.
//!
//! The site is a crate rather than a folder of files included by relative path:
//! a shared fixture folder has no crate of its own, so every consumer
//! recompiles a private copy and a relative-path include breaks when either end
//! moves. Consumers reach this crate through `[dev-dependencies]`, so nothing
//! here touches a shipped binary.
//!
//! - [`fingerprint`] hashes the platform migration corpus a working tree or a
//!   git tree would apply.
//! - [`shared`] elects one migrated server across every test process of a
//!   worktree and leases it, driving the daemon through `testcontainers::bollard`.
//! - [`lifetime`] measures that a shared-server fixture's container is removed
//!   once the process holding it exits or is killed while it starts.
//! - [`image`] builds a stock image with the shared-server watchdog on top.
//! - [`postgres`] is the platform and bare servers every test process of a
//!   worktree shares, and the case databases cloned from them.
//! - [`redpanda`] is the broker every stream test process of a worktree shares.
//! - [`mysql`] is the MySQL server every live-MySQL test process of a worktree
//!   shares, and the DSNs a case's own databases are reached through.
//! - [`redis`] owns the Redis and Dragonfly servers the driver, KV and binding
//!   suites share.
//! - [`tenant`] mints the scoped org/project/app/user ids a case owns.
//! - [`s3`] owns an S3-compatible server with Docker-assigned ports.
//! - [`session_keys`] writes the key files in-process auth servers read.
//! - [`nested_cargo`] starts a cargo from inside a process cargo started.
//! - [`recorded`] holds the recorded artifacts several crates pin against.
//! - [`prebuilt`] locates the service executables the workflow process suites
//!   run, refusing when one is absent or older than its build record.

pub mod fingerprint;
pub mod image;
pub mod lifetime;
pub mod mysql;
pub mod nested_cargo;
pub mod prebuilt;
pub mod postgres;
pub mod recorded;
pub mod redpanda;
pub mod redis;
pub mod s3;
pub mod session_keys;
pub mod shared;
pub mod tenant;
pub mod tenant_cluster;