//! The shared-server contract, driven from the crate that owns the protocol.
//!
//! The child cases are spawned from this binary by path
//! (`integration::testkit_shared_server::child_*`), so they live in the same
//! binary as their spawners.

mod testkit_shared_server;
