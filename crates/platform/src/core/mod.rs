//! # zeroship-core
//!
//! Core interfaces for the zeroship platform.
//! This crate defines the contracts that all other crates depend on.
//! It contains **no implementation** — only traits, types, and config structs.
//!
//! ## Crate dependency graph
//!
//! ```text
//! zeroship-core          ← this crate (interfaces only)
//!   ↑
//! zeroship-isolate       ← V8 isolate management
//! zeroship-plugins       ← db, kv, env, auth, ...
//!   ↑
//! zeroship-server        ← HTTP layer (axum)
//!   ↑
//! zeroship (binary)      ← CLI, config, wiring
//! ```

pub mod billing;
pub mod config;
pub mod event_log;
pub mod meter_store;
pub mod plugin;
pub mod types;
