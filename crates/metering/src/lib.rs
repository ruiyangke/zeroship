//! # appbase-metering
//!
//! Multi-tenant metering, quota enforcement, and rate limiting.
//!
//! - `plan`: Quota plan definitions (free, pro, enterprise)
//! - `meter`: Per-app atomic usage counters
//! - `enforcer`: Quota checking logic (allow/warn/deny)
//! - `rate_limit`: Token bucket rate limiter

pub mod plan;
pub mod meter;
pub mod enforcer;
pub mod rate_limit;
pub mod concurrency;
pub mod store;
