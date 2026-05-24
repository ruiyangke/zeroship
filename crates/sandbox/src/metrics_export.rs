//! Prometheus text-exposition exporter for the sandbox controller's
//! atomic counters / gauge (`crate::metrics`).
//!
//! The producer side has lived in `metrics.rs` since Phase 2 (sandbox-pg-state
//! design § 14.1 + § 14.7) — process-global `AtomicU64`s incremented at every
//! observability site (takeover, lost-leadership, vm-index leak, wake-sync
//! deprecation, terminal-overwrite block, Nomad node-id lookup failure). The
//! `Phase 3 wires a /metrics exporter` TODO carried for multiple rounds; this
//! module is its landing.
//!
//! ## Format
//!
//! [Prometheus text exposition format v0.0.4] — one section per metric:
//!
//! ```text
//! # HELP sandbox_<name> <description>
//! # TYPE sandbox_<name> <type>
//! sandbox_<name>{label="value"} <number>
//! ```
//!
//! For monotonic counters the suffix `_total` is appended by the Prometheus
//! client-library convention; every metric in this module that is `counter`-
//! typed already encodes that in its name in `metrics.rs`.
//!
//! [Prometheus text exposition format v0.0.4]:
//!   https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details
//!
//! ## Naming
//!
//! Names mirror the rustdoc on each counter in `metrics.rs` verbatim
//! (`sandbox_ha_takeover_total`, `sandbox_vm_index_leaks_total`, etc).
//! Labels are static — `reason="lease_expiration"`, `reason="host_fence_timeout"`,
//! `reason="wait_failed"` — except `sandbox_ha_lost_leadership_total{op=...}`
//! whose label set is dynamic (observed call sites materialise lazily into
//! `LOST_LEADERSHIP_BY_OP`).
//!
//! ## Stability
//!
//! Output is deterministic across calls when no counters were touched in
//! between: metric blocks are emitted in alphabetical order, and within a
//! labelled block the label values are emitted in lexicographic order.
//!
//! ## Read path
//!
//! Pure function over the read accessors in `crate::metrics`. No allocation
//! per atomic read; the only heap traffic is the single output `String`.

use crate::metrics;

/// Render the full Prometheus text exposition body. Returns a `String`
/// whose contents are safe to write directly into an HTTP response body
/// with `Content-Type: text/plain; version=0.0.4`.
///
/// The body is terminated by a newline (Prometheus parsers require this;
/// `promtool check metrics` rejects bodies missing the trailing `\n`).
pub fn render() -> String {
    // Sized for the current 16 atomics × ~3 lines each + headers. Bumping
    // when new counters land is cheap (`String::push_str` reallocs amortise).
    let mut out = String::with_capacity(4096);

    // Emit blocks in alphabetical order by metric name for stable diffs.
    // The labelled variants live under the metric name they share.
    //
    // Counter blocks ────────────────────────────────────────────────────

    write_counter(
        &mut out,
        "sandbox_corrupt_id_total",
        "decode of a stored sandbox_id as a typed-id failed at any read site",
        &[(None, metrics::sandbox_corrupt_id_value())],
    );

    write_counter(
        &mut out,
        "sandbox_ha_clock_rewind_total",
        "scan observed now() - last_heartbeat < 0 (R-MM clock-rewind detector)",
        &[(None, metrics::clock_rewind_value())],
    );

    write_counter(
        &mut out,
        "sandbox_ha_dead_hosts_observed_total",
        "dead hosts observed across all takeover scans (including duplicates)",
        &[(None, metrics::dead_hosts_observed_value())],
    );

    // Lost-leadership: aggregate + per-op breakdown share the metric name.
    // The aggregate is the unlabelled total; the per-op series carry op="...".
    let lost_leadership_aggregate = metrics::lost_leadership_value();
    let lost_leadership_by_op = metrics::lost_leadership_snapshot_by_op();
    let mut lost_leadership_series: Vec<(Option<(&str, &str)>, u64)> =
        Vec::with_capacity(1 + lost_leadership_by_op.len());
    lost_leadership_series.push((None, lost_leadership_aggregate));
    for (op, v) in &lost_leadership_by_op {
        lost_leadership_series.push((Some(("op", op)), *v));
    }
    write_counter(
        &mut out,
        "sandbox_ha_lost_leadership_total",
        "CAS-guarded UPDATE returned 0 rows because (host_id, generation) was preempted",
        &lost_leadership_series,
    );

    write_counter(
        &mut out,
        "sandbox_ha_takeover_corrupt_total",
        "takeover-rehydrate hit a corrupt sealed record / unparseable typed-id / restore failure",
        &[(None, metrics::takeover_corrupt_value())],
    );

    write_counter(
        &mut out,
        "sandbox_ha_takeover_mismatched_total",
        "takeover-rehydrate sealed key disagreed with pg key_fp OR /version returned 401/different fp",
        &[(None, metrics::takeover_mismatched_value())],
    );

    write_counter(
        &mut out,
        "sandbox_ha_takeover_orphan_total",
        "post-takeover rehydrate found no sealed record locally (accepted as status='lost')",
        &[(None, metrics::takeover_orphan_value())],
    );

    write_counter(
        &mut out,
        "sandbox_ha_takeover_total",
        "successful takeovers; reason label distinguishes lease_expiration / operator_rebind",
        &[(
            Some(("reason", "lease_expiration")),
            metrics::takeover_lease_expiration_value(),
        )],
    );

    write_counter(
        &mut out,
        "sandbox_ha_takeover_unreachable_total",
        "takeover-rehydrate agent /version probe timed out / returned non-2xx non-401",
        &[(None, metrics::takeover_unreachable_value())],
    );

    write_counter(
        &mut out,
        "sandbox_nomad_node_id_lookup_failures_total",
        "boot-time GET /v1/agent/self failed; controller falls back to random cross-node placement",
        &[(None, metrics::nomad_node_id_lookup_failures_value())],
    );

    write_counter(
        &mut out,
        "sandbox_vm_index_leaks_total",
        "stop_inner leaked a vm_index; reason=host_fence_timeout|wait_failed",
        &[
            (
                Some(("reason", "host_fence_timeout")),
                metrics::vm_index_leak_value("host_fence_timeout"),
            ),
            (
                Some(("reason", "wait_failed")),
                metrics::vm_index_leak_value("wait_failed"),
            ),
        ],
    );

    write_counter(
        &mut out,
        "sandbox_wake_sync_uses_total",
        "legacy synchronous wake path was taken (deprecation telemetry; Phase 5 gate)",
        &[(None, metrics::wake_sync_deprecated_value())],
    );

    write_counter(
        &mut out,
        "sandbox_wake_terminal_overwrite_blocked_total",
        "update_wake_job_state returned rows_affected==0 on a terminal write (R20-C1 guard fire)",
        &[(None, metrics::wake_terminal_overwrite_blocked_value())],
    );

    // Gauge blocks ──────────────────────────────────────────────────────

    write_gauge(
        &mut out,
        "sandbox_ha_heartbeat_lag_seconds",
        "pg-side now() - last_heartbeat for THIS controller; NaN until first read",
        metrics::heartbeat_lag_value(),
    );

    out
}

/// Wire-format-safe counter writer. `series` is the list of label-value
/// pairs to emit under the shared `# HELP`/`# TYPE` header — `None` means
/// the unlabelled series (`metric_name <value>`); `Some((label, value))`
/// emits `metric_name{label="value"} <count>`.
///
/// The header is emitted ONCE per metric name (Prometheus parsers reject
/// repeated `# HELP` for the same metric). Empty `series` is rejected at
/// the call site — every counter we export has at least one series.
fn write_counter(
    out: &mut String,
    name: &str,
    help: &str,
    series: &[(Option<(&str, &str)>, u64)],
) {
    debug_assert!(
        !series.is_empty(),
        "every exported counter must have at least one series"
    );
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" counter\n");
    for (label, value) in series {
        out.push_str(name);
        if let Some((k, v)) = label {
            out.push('{');
            out.push_str(k);
            out.push_str("=\"");
            // Label values: per the Prometheus exposition spec the
            // characters `\\`, `"`, and `\n` MUST be escaped. All our
            // current label values are static-known and contain none
            // of these — but the escape logic stays for safety.
            for ch in v.chars() {
                match ch {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '\n' => out.push_str("\\n"),
                    other => out.push(other),
                }
            }
            out.push_str("\"}");
        }
        out.push(' ');
        out.push_str(&value.to_string());
        out.push('\n');
    }
}

/// Gauge writer. Emits a `# TYPE … gauge` block with a single unlabelled
/// series. `NaN` renders as the Prometheus sentinel `NaN`; positive /
/// negative infinity render as `+Inf` / `-Inf` per the spec.
fn write_gauge(out: &mut String, name: &str, help: &str, value: f64) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" gauge\n");
    out.push_str(name);
    out.push(' ');
    if value.is_nan() {
        out.push_str("NaN");
    } else if value == f64::INFINITY {
        out.push_str("+Inf");
    } else if value == f64::NEG_INFINITY {
        out.push_str("-Inf");
    } else {
        out.push_str(&value.to_string());
    }
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every counter name we promise to export must appear in the rendered
    /// body. Catches a regression where a `write_counter(...)` call is
    /// accidentally deleted during a refactor.
    #[test]
    fn render_emits_every_production_counter_name() {
        let body = render();
        for name in [
            "sandbox_corrupt_id_total",
            "sandbox_ha_clock_rewind_total",
            "sandbox_ha_dead_hosts_observed_total",
            "sandbox_ha_lost_leadership_total",
            "sandbox_ha_takeover_corrupt_total",
            "sandbox_ha_takeover_mismatched_total",
            "sandbox_ha_takeover_orphan_total",
            "sandbox_ha_takeover_total",
            "sandbox_ha_takeover_unreachable_total",
            "sandbox_nomad_node_id_lookup_failures_total",
            "sandbox_vm_index_leaks_total",
            "sandbox_wake_sync_uses_total",
            "sandbox_wake_terminal_overwrite_blocked_total",
            "sandbox_ha_heartbeat_lag_seconds",
        ] {
            assert!(
                body.contains(name),
                "rendered body must contain metric {name}; got:\n{body}"
            );
        }
    }

    /// Wire format: every metric name has exactly ONE `# HELP` line and
    /// exactly ONE `# TYPE` line. Repeating either confuses Prometheus
    /// parsers (the second `# HELP` for the same metric is a hard error
    /// in promtool).
    #[test]
    fn each_metric_has_one_help_and_one_type_line() {
        let body = render();
        for name in [
            "sandbox_corrupt_id_total",
            "sandbox_ha_clock_rewind_total",
            "sandbox_ha_lost_leadership_total",
            "sandbox_ha_takeover_total",
            "sandbox_vm_index_leaks_total",
            "sandbox_ha_heartbeat_lag_seconds",
        ] {
            let help_count = body
                .lines()
                .filter(|l| l.starts_with("# HELP ") && l.contains(name))
                .count();
            let type_count = body
                .lines()
                .filter(|l| l.starts_with("# TYPE ") && l.contains(name))
                .count();
            assert_eq!(help_count, 1, "metric {name} must have exactly one HELP");
            assert_eq!(type_count, 1, "metric {name} must have exactly one TYPE");
        }
    }

    /// Counter metrics must be tagged `counter`; the heartbeat-lag gauge
    /// must be tagged `gauge`. Mis-tagging confuses rate() at the query
    /// layer (rate() over a gauge is meaningless).
    #[test]
    fn counter_and_gauge_types_are_correct() {
        let body = render();
        assert!(
            body.contains("# TYPE sandbox_ha_takeover_total counter"),
            "takeover must be a counter; got:\n{body}"
        );
        assert!(
            body.contains("# TYPE sandbox_ha_heartbeat_lag_seconds gauge"),
            "heartbeat-lag must be a gauge; got:\n{body}"
        );
    }

    /// Labelled series must carry the labels promised by the rustdoc on
    /// each counter (`reason=lease_expiration`, `reason=host_fence_timeout`,
    /// `reason=wait_failed`). The labels are the contract scrapers depend
    /// on for `rate(...)` grouping.
    #[test]
    fn labelled_series_carry_documented_labels() {
        let body = render();
        assert!(
            body.contains("sandbox_ha_takeover_total{reason=\"lease_expiration\"}"),
            "takeover series must carry reason=lease_expiration"
        );
        assert!(
            body.contains("sandbox_vm_index_leaks_total{reason=\"host_fence_timeout\"}"),
            "vm_index_leaks series must carry reason=host_fence_timeout"
        );
        assert!(
            body.contains("sandbox_vm_index_leaks_total{reason=\"wait_failed\"}"),
            "vm_index_leaks series must carry reason=wait_failed"
        );
    }

    /// `LOST_LEADERSHIP_BY_OP` is a dynamic per-op breakdown — labels
    /// materialise lazily as ops are observed. Pre-bump (or in a freshly
    /// started process) only the aggregate series exists; post-bump the
    /// labelled series appears alongside.
    #[test]
    fn lost_leadership_per_op_label_appears_after_bump() {
        crate::metrics::inc_lost_leadership("test_op_metrics_export_pin");
        let body = render();
        assert!(
            body.contains(
                "sandbox_ha_lost_leadership_total{op=\"test_op_metrics_export_pin\"}"
            ),
            "per-op label must appear after inc_lost_leadership; got:\n{body}"
        );
    }

    /// Body MUST end with a newline — Prometheus's text parser rejects a
    /// body whose last byte is not `\n`.
    #[test]
    fn render_terminates_with_newline() {
        let body = render();
        assert!(
            body.ends_with('\n'),
            "Prometheus parsers require a trailing newline"
        );
    }

    /// Gauge NaN sentinel: heartbeat-lag is NaN until the first pg read.
    /// The exporter must render it as the literal token `NaN` (not `nan`,
    /// not `null`, not omitting the value).
    #[test]
    fn nan_gauge_renders_as_prometheus_sentinel() {
        let mut out = String::new();
        write_gauge(&mut out, "x_test_gauge", "test", f64::NAN);
        assert!(out.contains("x_test_gauge NaN\n"), "got:\n{out}");
    }

    /// Label-value escape: special chars must be `\\`-escaped. The static
    /// labels in metrics.rs are all alphanumeric + underscore so this is
    /// belt-and-suspenders for future label additions.
    #[test]
    fn label_value_escapes_special_chars() {
        let mut out = String::new();
        write_counter(
            &mut out,
            "x_test_total",
            "test",
            &[(Some(("k", "a\"b\\c\nd")), 1)],
        );
        // Each special char appears as its escaped form in the body.
        assert!(out.contains("k=\"a\\\"b\\\\c\\nd\""), "got:\n{out}");
    }
}
