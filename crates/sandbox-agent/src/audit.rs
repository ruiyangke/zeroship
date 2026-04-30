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
//!   - `fs.symlink_reject`      — caller tried to read/write/delete a
//!                                  workspace path that contains a
//!                                  symlink at any component (ELOOP
//!                                  from `RESOLVE_NO_SYMLINKS`).
//!                                  This catches both leaf symlinks
//!                                  (`/files/escape -> /etc/passwd`)
//!                                  and parent-component symlinks
//!                                  (`/files/safedir/x` where
//!                                  `safedir` is a link out).
//!   - `fs.escape_reject`       — `RESOLVE_BENEATH` refused a path
//!                                  that resolved outside the
//!                                  workspace WITHOUT a symlink
//!                                  involved (EXDEV). Rare in
//!                                  practice — defensive constant
//!                                  for future kernel/mount setups
//!                                  where this could fire (e.g. bind
//!                                  mounts crossing dirfds).
//!   - `fs.size_reject`         — over-cap write rejected
//!   - `exec.timeout`           — command killed at wall clock
//!   - `exec.truncated`         — output capped at `MAX_OUTPUT_BYTES`

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
///
/// `fields` may include attacker-controlled values (e.g., the
/// requested path on `auth.fail`). We rely on the JSON-formatted
/// subscriber configured in `main` to escape `\n`, quotes, etc. —
/// log injection on a non-JSON subscriber is a known caveat.
pub fn record(event: &'static str, fields: &str) {
    warn!(target: "audit", event, fields);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_does_not_panic_with_simple_input() {
        record(events::AUTH_FAIL, "method=GET path=/healthz");
    }

    #[test]
    fn record_handles_newlines_and_quotes() {
        // Defense-in-depth check: even with adversarial input, we
        // must not panic. JSON encoder downstream handles escaping.
        record(events::FS_SYMLINK_REJECT, "op=read path=foo\nbar=\"quux\"");
    }

    #[test]
    fn record_handles_empty_fields() {
        record(events::EXEC_TIMEOUT, "");
    }

    #[test]
    fn event_names_are_stable() {
        // Locks the wire-level identifier strings. Renaming any of
        // these is a breaking audit-pipeline change.
        assert_eq!(events::AUTH_FAIL, "auth.fail");
        assert_eq!(events::FS_SYMLINK_REJECT, "fs.symlink_reject");
        assert_eq!(events::FS_ESCAPE_REJECT, "fs.escape_reject");
        assert_eq!(events::FS_SIZE_REJECT, "fs.size_reject");
        assert_eq!(events::EXEC_TIMEOUT, "exec.timeout");
        assert_eq!(events::EXEC_TRUNCATED, "exec.truncated");
    }

    #[test]
    fn event_names_are_unique() {
        let names = [
            events::AUTH_FAIL,
            events::FS_SYMLINK_REJECT,
            events::FS_ESCAPE_REJECT,
            events::FS_SIZE_REJECT,
            events::EXEC_TIMEOUT,
            events::EXEC_TRUNCATED,
        ];
        let mut sorted = names.to_vec();
        sorted.sort();
        let len = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), len, "duplicate event names");
    }
}
