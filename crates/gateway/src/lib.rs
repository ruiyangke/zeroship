//! Library surface for integration tests. The binary is `main.rs`.
//!
//! Only the modules that have a stable cross-module / test-facing
//! surface are re-exported here. Internal gateway modules (`router`,
//! `proxy`, `enforce`, …) continue to be `mod xxx;` from `main.rs`.

pub mod error;
pub mod sessions;
