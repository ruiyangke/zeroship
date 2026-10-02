//! Shared test fixtures for the zeroship workspace.
//!
//! The site is a crate rather than a folder of files included by relative path:
//! a shared fixture under `tests/fixtures` has no crate of its own, so every
//! consumer recompiles a private copy and a `#[path]` breaks when either end
//! moves. Consumers reach this crate through `[dev-dependencies]`, so nothing
//! here touches a shipped binary.
//!
//! - [`docker`] owns the containers a test process starts and the reaper that
//!   removes them once that process ends.
//! - [`lifetime`] carries the two measurements a fixture built on the reaper
//!   owes: its container is gone after the owning process exits, and after that
//!   process is killed while the container is still starting.
//! - [`postgres`] is the reaper-owned platform database one test process shares.
//! - [`tenant`] mints the scoped org/project/app/user ids a case owns.

pub mod docker;
pub mod postgres;
pub mod tenant;

pub use docker::lifetime;
pub use docker::{process_owner, start_owned, DockerCli, OwnedContainer, Ownership};
