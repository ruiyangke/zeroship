//! Shared fixtures for plugin-db's integration targets.
//!
//! Declared by `integration.rs` and `sqlite_integration.rs`; each test binary
//! compiles its own copy, which is why every item here has to be reachable from
//! either parent without one depending on the other.

pub mod tables;
