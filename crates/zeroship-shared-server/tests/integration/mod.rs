//! The shared-server protocol contract, driven from the crate that owns it.
//!
//! The child cases are spawned from this binary by path
//! (`integration::shared_server::child_*`), so they live in the same binary as
//! their spawners.

mod shared_server;
