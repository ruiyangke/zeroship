//! Rust-side HTTP plumbing — separate from `web::fetch` (which owns the
//! spec algorithms).
//!
//! - `ssrf`    - `validate_url` + `is_blocked_ip` + `SsrfResolver`
//! - `egress`  - the `node:net` three-phase egress evaluator (rules + floor + DNS)
//! - `client`  — per-thread cyper Client builder (used by `web::fetch::http_network`)
//! - `handler` — kernel bridge for inbound HTTP requests routed into V8

pub mod client;
pub mod byte_pump;
pub mod egress;
pub mod handler;
pub mod net_policy;
pub mod ssrf;
#[cfg(feature = "runtime_tls")]
pub mod tls;
