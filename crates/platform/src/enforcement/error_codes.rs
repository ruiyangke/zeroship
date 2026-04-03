//! JSON-RPC error codes for the appbase metering system.
//!
//! Per spec S8.4, each enforcement failure type has a distinct error code
//! in the JSON-RPC reserved range (-32000 to -32099).

/// Quota exceeded — a hard limit has been breached (spec S8.4).
pub const QUOTA_EXCEEDED: i32 = -32029;

/// Rate limit exceeded — too many requests per second (spec S8.4).
pub const RATE_LIMITED: i32 = -32030;

/// Spending limit reached — cost cap hit (spec S6).
pub const SPENDING_LIMIT: i32 = -32031;

/// Concurrency limit exceeded — too many in-flight requests (spec S8.4).
pub const CONCURRENCY_LIMIT: i32 = -32032;

/// Entitlement denied — feature not available on current plan (spec S8.4).
pub const ENTITLEMENT_DENIED: i32 = -32033;

/// Internal server error — V8 dispatch failure (JSON-RPC standard).
pub const INTERNAL_ERROR: i32 = -32603;
