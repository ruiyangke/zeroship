//! Prometheus metrics for the agent.
//!
//! Emits a small, stable set of counters/gauges via the standard
//! Prometheus text format (v0.0.4). Hand-rolled so we don't pull a
//! large metrics crate into a binary that's mounted into every
//! microVM (image-size sensitive).
//!
//! ## Naming
//!
//! All metrics are namespaced `sbx_agent_*`. Per Prometheus best
//! practice, counters end in `_total`, gauges have a unit suffix
//! (e.g. `_seconds`), and label cardinality is bounded — only fixed
//! enums (handler name, auth-fail reason) are used as labels, never
//! per-request data like remote-addr or path.
//!
//! ## Authentication
//!
//! `/metrics` is **unauthenticated** by design. The endpoint exposes
//! only counters/gauges (no secrets, no sensitive request content),
//! and the in-cluster scrape from prometheus-operator can't easily
//! HMAC-sign each request without bespoke tooling. Cluster-level
//! NetworkPolicy restricts who can reach :7777 in the first place;
//! that's the primary access control. Cf. /livez and /readyz, also
//! unauthenticated for the same reason (kubelet probes).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ─── counters ────────────────────────────────────────────────────

/// HTTP requests dispatched to /exec.
static EXEC_REQUESTS: AtomicU64 = AtomicU64::new(0);
/// /exec invocations whose wall-clock timeout fired.
static EXEC_TIMEOUTS: AtomicU64 = AtomicU64::new(0);
/// /exec invocations that exited non-zero.
static EXEC_NONZERO: AtomicU64 = AtomicU64::new(0);
/// Auth failures: bad/missing signature.
static AUTH_FAIL_BAD_SIG: AtomicU64 = AtomicU64::new(0);
/// Auth failures: replayed nonce.
static AUTH_FAIL_REPLAY: AtomicU64 = AtomicU64::new(0);
/// Auth failures: timestamp out of skew window.
static AUTH_FAIL_SKEW: AtomicU64 = AtomicU64::new(0);
/// Auth failures: any other reason (malformed nonce, query string, etc).
static AUTH_FAIL_OTHER: AtomicU64 = AtomicU64::new(0);
/// /files PUT byte total.
static FILES_BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);
/// /files GET byte total.
static FILES_BYTES_READ: AtomicU64 = AtomicU64::new(0);

// ─── increment helpers ───────────────────────────────────────────
//
// The increment functions are the only mutator surface — keeps the
// Atomic ops contained so we don't accidentally use `Ordering::SeqCst`
// somewhere subtle. Counters never decrement; saturating-add'ing on
// u64 overflow is fine (would take centuries at any plausible rate).

pub fn inc_exec_request() {
    EXEC_REQUESTS.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_exec_timeout() {
    EXEC_TIMEOUTS.fetch_add(1, Ordering::Relaxed);
}
pub fn inc_exec_nonzero() {
    EXEC_NONZERO.fetch_add(1, Ordering::Relaxed);
}
pub fn add_files_bytes_written(n: u64) {
    FILES_BYTES_WRITTEN.fetch_add(n, Ordering::Relaxed);
}
pub fn add_files_bytes_read(n: u64) {
    FILES_BYTES_READ.fetch_add(n, Ordering::Relaxed);
}

/// Bucket an auth failure reason (string carried in audit events) into
/// one of four counters. We don't expose the raw string as a label
/// to keep cardinality bounded — Prometheus treats every distinct
/// label set as a new series.
pub fn inc_auth_fail(reason: &str) {
    let counter = if reason.contains("BadSignature") || reason.contains("BadSignatureEncoding") {
        &AUTH_FAIL_BAD_SIG
    } else if reason.contains("ReplayedNonce") {
        &AUTH_FAIL_REPLAY
    } else if reason.contains("SkewTooLarge") || reason.contains("BadTimestamp") {
        &AUTH_FAIL_SKEW
    } else {
        &AUTH_FAIL_OTHER
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

// ─── render ──────────────────────────────────────────────────────

/// Render all metrics in Prometheus text exposition format. Includes
/// process-uptime as a gauge derived from `started_at_unix`, and the
/// reaper-healthy gauge (1/0) so a Prometheus alert can fire on a
/// fleet of agents whose reapers have died.
///
/// Output is `text/plain; version=0.0.4` per Prometheus convention.
#[must_use]
pub fn render(started_at_unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(started_at_unix);
    let uptime = now.saturating_sub(started_at_unix);
    let reaper = u8::from(crate::reap::is_healthy());

    let mut out = String::with_capacity(2048);

    push_counter(
        &mut out,
        "sbx_agent_exec_requests_total",
        "Total /exec requests received.",
        EXEC_REQUESTS.load(Ordering::Relaxed),
    );
    push_counter(
        &mut out,
        "sbx_agent_exec_timeouts_total",
        "/exec invocations whose wall-clock timeout fired.",
        EXEC_TIMEOUTS.load(Ordering::Relaxed),
    );
    push_counter(
        &mut out,
        "sbx_agent_exec_nonzero_exits_total",
        "/exec invocations that exited non-zero.",
        EXEC_NONZERO.load(Ordering::Relaxed),
    );

    push_help(&mut out, "sbx_agent_auth_failures_total", "HMAC auth failures, bucketed by reason.");
    push_type(&mut out, "sbx_agent_auth_failures_total", "counter");
    push_labeled(
        &mut out,
        "sbx_agent_auth_failures_total",
        "reason",
        "bad_signature",
        AUTH_FAIL_BAD_SIG.load(Ordering::Relaxed),
    );
    push_labeled(
        &mut out,
        "sbx_agent_auth_failures_total",
        "reason",
        "replay",
        AUTH_FAIL_REPLAY.load(Ordering::Relaxed),
    );
    push_labeled(
        &mut out,
        "sbx_agent_auth_failures_total",
        "reason",
        "skew",
        AUTH_FAIL_SKEW.load(Ordering::Relaxed),
    );
    push_labeled(
        &mut out,
        "sbx_agent_auth_failures_total",
        "reason",
        "other",
        AUTH_FAIL_OTHER.load(Ordering::Relaxed),
    );

    push_counter(
        &mut out,
        "sbx_agent_files_bytes_written_total",
        "Total bytes written via PUT /files.",
        FILES_BYTES_WRITTEN.load(Ordering::Relaxed),
    );
    push_counter(
        &mut out,
        "sbx_agent_files_bytes_read_total",
        "Total bytes returned via GET /files.",
        FILES_BYTES_READ.load(Ordering::Relaxed),
    );

    push_gauge(
        &mut out,
        "sbx_agent_uptime_seconds",
        "Seconds since the agent process started.",
        uptime,
    );
    push_gauge(
        &mut out,
        "sbx_agent_reaper_healthy",
        "1 if the PID 1 SIGCHLD reaper is installed and the thread is alive; 0 otherwise.",
        u64::from(reaper),
    );

    out
}

fn push_help(buf: &mut String, name: &str, help: &str) {
    buf.push_str("# HELP ");
    buf.push_str(name);
    buf.push(' ');
    buf.push_str(help);
    buf.push('\n');
}

fn push_type(buf: &mut String, name: &str, kind: &str) {
    buf.push_str("# TYPE ");
    buf.push_str(name);
    buf.push(' ');
    buf.push_str(kind);
    buf.push('\n');
}

fn push_counter(buf: &mut String, name: &str, help: &str, value: u64) {
    push_help(buf, name, help);
    push_type(buf, name, "counter");
    buf.push_str(name);
    buf.push(' ');
    buf.push_str(&value.to_string());
    buf.push('\n');
}

fn push_gauge(buf: &mut String, name: &str, help: &str, value: u64) {
    push_help(buf, name, help);
    push_type(buf, name, "gauge");
    buf.push_str(name);
    buf.push(' ');
    buf.push_str(&value.to_string());
    buf.push('\n');
}

fn push_labeled(buf: &mut String, name: &str, label_k: &str, label_v: &str, value: u64) {
    buf.push_str(name);
    buf.push('{');
    buf.push_str(label_k);
    buf.push_str("=\"");
    buf.push_str(label_v);
    buf.push_str("\"} ");
    buf.push_str(&value.to_string());
    buf.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_includes_required_metrics() {
        let s = render(0);
        for needle in [
            "sbx_agent_exec_requests_total",
            "sbx_agent_exec_timeouts_total",
            "sbx_agent_exec_nonzero_exits_total",
            "sbx_agent_auth_failures_total",
            "sbx_agent_files_bytes_written_total",
            "sbx_agent_files_bytes_read_total",
            "sbx_agent_uptime_seconds",
            "sbx_agent_reaper_healthy",
        ] {
            assert!(s.contains(needle), "missing metric: {needle}\nrendered:\n{s}");
        }
    }

    #[test]
    fn render_emits_help_and_type_for_each_metric() {
        let s = render(0);
        // Every counter/gauge name must have a # HELP and a # TYPE.
        for name in [
            "sbx_agent_exec_requests_total",
            "sbx_agent_exec_timeouts_total",
            "sbx_agent_uptime_seconds",
            "sbx_agent_reaper_healthy",
        ] {
            assert!(
                s.contains(&format!("# HELP {name}")),
                "missing HELP for {name}"
            );
            assert!(
                s.contains(&format!("# TYPE {name}")),
                "missing TYPE for {name}"
            );
        }
    }

    #[test]
    fn render_uptime_increases_from_started_at() {
        // started_at = far in the past → uptime is positive (>= the
        // gap we created).
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let s = render(now.saturating_sub(100));
        // uptime line: "sbx_agent_uptime_seconds <N>"
        let line = s
            .lines()
            .find(|l| l.starts_with("sbx_agent_uptime_seconds "))
            .expect("uptime line missing");
        let v: u64 = line
            .strip_prefix("sbx_agent_uptime_seconds ")
            .unwrap()
            .parse()
            .unwrap();
        assert!(v >= 100, "uptime should be >= 100, got {v}");
    }

    #[test]
    fn auth_fail_reason_buckets_correctly() {
        let before = AUTH_FAIL_BAD_SIG.load(Ordering::Relaxed);
        inc_auth_fail("BadSignature");
        assert_eq!(
            AUTH_FAIL_BAD_SIG.load(Ordering::Relaxed),
            before + 1
        );

        let before_replay = AUTH_FAIL_REPLAY.load(Ordering::Relaxed);
        inc_auth_fail("ReplayedNonce");
        assert_eq!(
            AUTH_FAIL_REPLAY.load(Ordering::Relaxed),
            before_replay + 1
        );

        let before_skew = AUTH_FAIL_SKEW.load(Ordering::Relaxed);
        inc_auth_fail("SkewTooLarge");
        assert_eq!(
            AUTH_FAIL_SKEW.load(Ordering::Relaxed),
            before_skew + 1
        );

        let before_other = AUTH_FAIL_OTHER.load(Ordering::Relaxed);
        inc_auth_fail("query-not-allowed");
        assert_eq!(
            AUTH_FAIL_OTHER.load(Ordering::Relaxed),
            before_other + 1
        );
    }
}
