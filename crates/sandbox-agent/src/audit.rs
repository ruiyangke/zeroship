//! Security-event audit log.
//!
//! Routed through `tracing` with `target = "audit"`, so a downstream
//! `tracing-subscriber` filter can split audit lines into a separate
//! sink (file, syslog, alerting pipeline) while regular logs stay in
//! the application stream.
//!
//! Every recorded event includes:
//!   - the `event` kind string (stable identifier)
//!   - the relevant **relative path** (never the host path)
//!   - any other structured fields the call site cares about
//!
//! On a fleet of agents, a spike in any of these is an attack signal:
//!
//!   - `auth.fail`              — wrong / missing bearer
//!   - `auth.token_load_fail`   — agent boot couldn't load token
//!   - `fs.symlink_reject`      — caller tried to read/write/delete a
//!                                  symlink (or escape via parent)
//!   - `fs.escape_reject`       — RESOLVE_BENEATH refused a path
//!   - `fs.size_reject`         — over-cap write rejected
//!   - `exec.timeout`           — command killed at wall clock
//!   - `exec.truncated`         — output capped at MAX_OUTPUT_BYTES

use tracing::warn;

/// Structured audit-event names. Stable identifiers — once shipped,
/// only **add** entries; never rename or remove.
pub mod events {
    pub const AUTH_FAIL: &str = "auth.fail";
    pub const FS_SYMLINK_REJECT: &str = "fs.symlink_reject";
    pub const FS_ESCAPE_REJECT: &str = "fs.escape_reject";
    pub const FS_SIZE_REJECT: &str = "fs.size_reject";
    pub const EXEC_TIMEOUT: &str = "exec.timeout";
    pub const EXEC_TRUNCATED: &str = "exec.truncated";
}

/// Record an audit event. Goes to `tracing` with `target = "audit"`.
pub fn record(event: &'static str, fields: &str) {
    warn!(target: "audit", event, fields);
}
