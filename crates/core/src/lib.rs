//! # appbase-core
//!
//! Core interfaces for the appbase platform.
//! This crate defines the contracts that all other crates depend on.
//! It contains **no implementation** — only traits, types, and config structs.
//!
//! ## Crate dependency graph
//!
//! ```text
//! appbase-core          ← this crate (interfaces only)
//!   ↑
//! appbase-isolate       ← V8 isolate management
//! appbase-plugins       ← db, kv, env, auth, ...
//!   ↑
//! appbase-server        ← HTTP layer (axum)
//!   ↑
//! appbase (binary)      ← CLI, config, wiring
//! ```

pub mod config;
pub mod plugin;
pub mod types;
