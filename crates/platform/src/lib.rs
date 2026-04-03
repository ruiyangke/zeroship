//! # appbase-platform
//!
//! Multi-tenant platform layer for appbase. All platform concerns in a single crate:
//! core traits, plans, enforcement, metering, billing, control plane, HTTP server.
//!
//! ## Architecture
//!
//! ```text
//! Layer 1: Runtime (appbase-runtime)
//!   V8 isolate, ESM modules, crypto, fetch, URL, timers, KV
//!   Zero platform deps — works standalone for local dev
//!
//! Layer 2: Platform (this crate)
//!   core     — traits, config types, plugin interface
//!   plan     — quota definitions, policy evaluation
//!   enforcement — rate limiting, concurrency, quota checking
//!   metering — atomic counters, flush loop, period rollover
//!   billing  — pricing, spending limits, reconciler
//!   control  — app registry, SQLite/Postgres
//!   server   — axum HTTP, routing, V8 pool
//!
//! Layer 3: CLI (appbase binary)
//!   Thin wrapper dispatching to runtime or platform
//! ```

pub mod core;
pub mod plan;
pub mod enforcement;
pub mod metering;
pub mod billing;
pub mod control;
pub mod server;
