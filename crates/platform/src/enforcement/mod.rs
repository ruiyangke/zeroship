//! # zeroship-enforcement
//!
//! Rate limiting, quota enforcement, concurrency control, and entitlements.
//!
//! - `quota`: Quota checking logic (allow/warn/deny)
//! - `rate_limit`: Token bucket rate limiter
//! - `concurrency`: CAS-based concurrency guard
//! - `error_codes`: JSON-RPC error codes for enforcement failures

pub mod concurrency;
pub mod error_codes;
pub mod quota;
pub mod rate_limit;
