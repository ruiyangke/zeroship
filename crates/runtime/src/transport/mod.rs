//! Rust-side HTTP plumbing — separate from `web::fetch` (which owns the
//! spec algorithms).
//!
//! - `ssrf`    — `validate_url` + `is_blocked_ip` + `SsrfResolver`
//! - `client`  — per-thread cyper Client builder (used by `web::fetch::http_network`)
//! - `handler` — kernel bridge for inbound HTTP requests routed into V8

pub mod client;
pub mod handler;
pub mod net_policy;
pub mod ssrf;
