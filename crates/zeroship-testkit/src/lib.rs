//! Shared test fixtures for the zeroship workspace.
//!
//! The site is a crate rather than a folder of files included by relative path:
//! a shared fixture folder has no crate of its own, so every consumer
//! recompiles a private copy and a relative-path include breaks when either end
//! moves. Consumers reach this crate through `[dev-dependencies]`, so nothing
//! here touches a shipped binary.
//!
//! The lease protocol every server below is shared through, the watchdog image
//! builder and the lifetime measurements live in `zeroship-shared-server`, which
//! names no database driver.
//!
//! - [`fingerprint`] hashes the platform migration corpus a working tree would
//!   apply.
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
pub mod mysql;
pub mod nested_cargo;
pub mod prebuilt;
pub mod postgres;
pub mod recorded;
pub mod redpanda;
pub mod redis;
pub mod s3;
pub mod session_keys;
pub mod tenant;
pub mod tenant_cluster;
