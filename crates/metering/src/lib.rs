//! # appbase-metering
//!
//! Multi-tenant metering — per-app atomic usage counters, flush loop, and period rollover.
//!
//! Quota plan definitions are in `appbase-plan`.
//! Enforcement (quota checking, rate limiting, concurrency, error codes) is in `appbase-enforcement`.
//!
//! - `plan`: Re-exports from `appbase-plan` for backward compatibility
//! - `meter`: Per-app atomic usage counters
//! - `config`: TOML config parsing for metering plans

pub mod plan;
pub mod registry;
pub mod meter;
pub mod config;
pub mod event_channel;
pub mod event_logger;
pub mod flusher;
pub mod rollover;
pub mod store;
