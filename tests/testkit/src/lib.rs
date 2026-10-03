//! Shared test fixtures for the zeroship workspace.
//!
//! The site is a crate rather than a folder of files included by relative path:
//! a shared fixture under `tests/fixtures` has no crate of its own, so every
//! consumer recompiles a private copy and a `#[path]` breaks when either end
//! moves. Consumers reach this crate through `[dev-dependencies]`, so nothing
//! here touches a shipped binary.
//!
//! - [`fingerprint`] hashes the platform migration corpus a working tree or a
//!   git tree would apply.
//! - [`shared`] elects one migrated server across every test process of a
//!   worktree and leases it.
//! - [`docker`] owns the containers a test process starts and the reaper that
//!   removes them once that process ends.
//! - [`lifetime`] carries the two measurements a fixture built on the reaper
//!   owes: its container is gone after the owning process exits, and after that
//!   process is killed while the container is still starting.
//! - [`postgres`] is the platform and bare servers every test process of a
//!   worktree shares, and the case databases cloned from them.
//! - [`redpanda`] is the broker every stream test process of a worktree shares.
//! - [`redis`] owns the Redis and Dragonfly servers the driver, KV and binding
//!   suites share.
//! - [`tenant`] mints the scoped org/project/app/user ids a case owns.
//! - [`s3`] owns an S3-compatible server with Docker-assigned ports.
//! - [`session_keys`] writes the key files in-process auth servers read.
//! - [`nested_cargo`] starts a cargo from inside a process cargo started.

pub mod docker;
pub mod fingerprint;
pub mod nested_cargo;
pub mod postgres;
pub mod redpanda;
pub mod redis;
pub mod s3;
pub mod session_keys;
pub mod shared;
pub mod tenant;

pub use docker::lifetime;
pub use docker::{process_owner, start_owned, DockerCli, OwnedContainer, Ownership};
