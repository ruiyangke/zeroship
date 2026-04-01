//! # appbase-plugins
//!
//! Built-in plugins for the appbase platform.
//! Each plugin is behind a feature flag — only pull in what you need.
//!
//! - `db` — SQLite document store (requires `uuid`)
//! - `kv` — In-memory key-value store
//! - `env` — Read-only environment variables

#[cfg(feature = "db")]
pub mod db;

#[cfg(feature = "kv")]
pub mod kv;

#[cfg(feature = "env")]
pub mod env;
