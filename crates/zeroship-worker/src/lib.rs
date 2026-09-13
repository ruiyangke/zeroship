pub mod cache;
pub mod config;
// The binary imports the boot posture validator from this library.
pub mod db_posture;
pub mod metrics;
// Dual-declared for the same reason as `cache` and `metrics`: `handler.rs` is
// a private module of the BINARY and reaches this one as same-crate code,
// while `cargo test -p zeroship-worker --lib` - the per-crate command
// AGENTS.md documents - only sees what this file declares. Its tests are
// synchronous and open nothing, so running them in both targets costs the
// same as `cache`'s do and buys a fence that neither command can miss.
pub mod policy;

#[cfg(test)]
mod test_database;
