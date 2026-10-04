//! The contracts of the servers this testkit runs on the shared-server protocol.
//!
//! The child cases are spawned from this binary by path
//! (`integration::testkit_shared_server::child_*`), so they live in the same
//! binary as their spawners.

mod testkit_shared_server;
