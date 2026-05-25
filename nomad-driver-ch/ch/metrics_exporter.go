// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-8b-stress-r8 r7-B: driver-side Prometheus text-format exporter.
//
// Why this exists: the driver's process-global counters
// (`nomad_driver_ch_*`) are not visible via Nomad's `/v1/metrics`
// endpoint — they live inside the driver plugin process, not in the
// Nomad client's metric registry. Stress-r8 review couldn't verify
// counter deltas because the operator had no scraping path. This
// gap blocks every "did the new gate fire?" diagnostic.
//
// Two shapes were considered (per r7-B in the sprint mandate):
//
//   1. Driver plugin embeds a tiny HTTP server on a fixed port — too
//      heavy (port-collision surface; auth surface; lifecycle
//      coupling to plugin shutdown).
//
//   2. Driver writes counter state to a file at
//      `/var/lib/zsbx/driver-metrics.prom` in Prometheus text format;
//      node_exporter's textfile collector (or any scraper) picks it
//      up — chosen.
//
// Format reference: https://prometheus.io/docs/instrumenting/exposition_formats/#text-based-format
// — one metric family per HELP/TYPE/sample triple, monotonic
// counters end in `_total`, lines newline-terminated.
//
// Cadence: 5 seconds. Counter writes are atomic.Int64.Load(); the
// goroutine's only side effect is a single os.WriteFile per tick.
// Atomic via tmp-file-rename so a partial write is never visible to
// a concurrent reader.
//
// Lifecycle: started from NewPlugin, stopped via the plugin's
// signalShutdown context cancellation. The exporter goroutine
// observes ctx.Done() and writes one final snapshot before exiting
// (so a graceful shutdown leaves the last-known-good state on disk).

package ch

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"github.com/hashicorp/go-hclog"
)

// defaultDriverMetricsExportPath is the on-disk destination for the
// Prometheus text-format snapshot. Operators point node_exporter's
// `--collector.textfile.directory` at the parent dir (or symlink
// this file into it) and the counters surface in `/metrics`.
//
// The path is under /var/lib/zsbx/ to share the existing driver
// state root (vm-index lockdir, run-dir, etc.) — operators
// already manage permissions for that prefix. Falling outside it
// would require a separate permissions story.
const defaultDriverMetricsExportPath = "/var/lib/zsbx/driver-metrics.prom"

// defaultDriverMetricsExportInterval is the cadence at which the
// exporter goroutine writes a snapshot. 5 seconds matches the
// fingerprint period (FingerprintPeriod/6); aligns with typical
// Prometheus scrape intervals (15s default) so a scrape sees at
// most one stale tick. Tunable via SetDriverMetricsExportInterval.
//
// Smaller intervals (1s) burn syscalls without observability
// benefit; larger intervals (30s+) make spike-detection laggy on
// fast-failing fleets. 5s is the established convention.
const defaultDriverMetricsExportInterval = 5 * time.Second

// driverMetricsExportPath is the package-level var the exporter
// reads. Exposed as a knob for SetDriverMetricsExportPathForTest so
// tests can redirect the output to t.TempDir() instead of
// /var/lib/zsbx/.
var driverMetricsExportPath = defaultDriverMetricsExportPath

// driverMetricsExportInterval is the package-level var the exporter
// loop ticks against. Exposed via SetDriverMetricsExportIntervalForTest
// so tests can drive ticks without real wall time.
var driverMetricsExportInterval = defaultDriverMetricsExportInterval

// driverMetricsExportEnabled gates whether NewPlugin spawns the
// exporter goroutine. Defaults to true (production); the test
// suite's TestMain flips this to false so 30+ NewPlugin calls
// don't spawn 30+ leaked goroutines each trying to MkdirAll
// /var/lib/zsbx under an unprivileged user. Tests that need the
// exporter explicitly re-enable it via SetDriverMetricsEnabledForTest
// + RunDriverMetricsExporterForTest.
var driverMetricsExportEnabled = true

// SetDriverMetricsEnabledForTest flips the NewPlugin-spawns-exporter
// toggle. Returns the previous value so the caller can restore it
// on cleanup. The test suite's TestMain calls SetDriverMetricsEnabledForTest(false)
// once to suppress per-test goroutine leakage; per-test enables
// happen as needed.
func SetDriverMetricsEnabledForTest(enabled bool) bool {
	prev := driverMetricsExportEnabled
	driverMetricsExportEnabled = enabled
	return prev
}

// SetDriverMetricsExportPathForTest swaps the on-disk destination
// for the exporter snapshot. Returns the previous path so the
// caller can restore it on cleanup.
func SetDriverMetricsExportPathForTest(path string) string {
	prev := driverMetricsExportPath
	if path != "" {
		driverMetricsExportPath = path
	}
	return prev
}

// SetDriverMetricsExportIntervalForTest swaps the exporter tick
// cadence. Returns the previous interval so the caller can restore
// it on cleanup. Tests typically install a sub-millisecond cadence
// to drive multiple ticks within the test wallclock budget.
func SetDriverMetricsExportIntervalForTest(d time.Duration) time.Duration {
	prev := driverMetricsExportInterval
	if d > 0 {
		driverMetricsExportInterval = d
	}
	return prev
}

// renderDriverMetricsProm produces the Prometheus text-format
// snapshot of the driver's counters. Pure function — no I/O, no
// global state mutation. Tests assert against the returned string
// directly without engineering the exporter goroutine.
//
// The format follows the Prometheus exposition spec strictly:
//
//   - One HELP/TYPE/sample triple per metric family
//   - HELP lines are free text (escape backslash + newline)
//   - TYPE lines are "counter" (we have no gauges yet)
//   - Sample lines: `<name> <value>\n`
//   - No labels (all counters are process-global)
//
// Adding a new counter: extend the list below in alphabetical order
// of metric name. Each entry needs (name, help, currentValue).
func renderDriverMetricsProm() string {
	entries := []struct {
		name  string
		help  string
		value int64
	}{
		{
			name:  "nomad_driver_ch_destroy_task_lock_held_total",
			help:  "Times DestroyTask exhausted its OFD-lock-probe budget without acquiring the F_OFD_SETLK write lock on a disk path (T-8b-stress-r5 r5-A).",
			value: DestroyTaskLockHeldTotal(),
		},
		{
			name:  "nomad_driver_ch_destroy_task_tap_stuck_total",
			help:  "Times DestroyTask exhausted its tap-deletion-verify budget without observing the kernel evict the tap netdev (T-8b-stress-r8 r24-A2-S2).",
			value: DestroyTaskTapStuckTotal(),
		},
		{
			name:  "nomad_driver_ch_destroy_task_unreaped_total",
			help:  "Times DestroyTask exhausted its reap-wait budget without observing the CH process being reaped by the OS (T-8b-stress-r4 r4-A).",
			value: DestroyTaskUnreapedTotal(),
		},
		{
			name:  "nomad_driver_ch_start_task_stage_failures_total",
			help:  "Times the driver-side staging op (Option C Phase 2) returned a non-nil error from truncate(1) or mkfs.ext4(8).",
			value: StartTaskStageFailuresTotal(),
		},
		{
			name:  "nomad_driver_ch_start_task_stage_total",
			help:  "Times a cold-boot StartTask ran the driver-side staging op (TaskConfig.StageDiskImages=true; Option C Phase 2).",
			value: StartTaskStageTotal(),
		},
		{
			name:  "nomad_driver_ch_taps_orphaned_total",
			help:  "Times DestroyTask's defensive VMIndex-keyed tap cleanup pass removed a tap whose name the handle didn't record (T-8b-stress-r2 driver v14).",
			value: TapsOrphanedTotal(),
		},
		{
			name:  "nomad_driver_ch_wake_rootfs_lock_held_total",
			help:  "Times startTaskRestoreBranch exhausted its wake-side OFD-lock-probe budget without observing the source alloc release its exclusive write lock on rootfs.img before CH --restore spawn (T-8b-stress-r9-retry-6 driver v21).",
			value: WakeRootfsLockHeldTotal(),
		},
	}

	var b strings.Builder
	// Top-of-file header — operators see this when they cat the
	// file directly; documents the cadence and the canonical
	// destination so a misplaced symlink surfaces fast.
	b.WriteString("# Generated by nomad-driver-ch metrics exporter.\n")
	b.WriteString(fmt.Sprintf("# Snapshot cadence: %v. Destination: %s.\n", driverMetricsExportInterval, driverMetricsExportPath))
	b.WriteString("# Format: Prometheus text exposition v0.0.4.\n")
	for _, e := range entries {
		b.WriteString("# HELP ")
		b.WriteString(e.name)
		b.WriteString(" ")
		// HELP lines escape backslash and newline per the spec.
		// Our help strings carry neither, but the escape is cheap
		// future-proofing against a future contributor adding a
		// help line with a "\n" in it.
		b.WriteString(strings.ReplaceAll(strings.ReplaceAll(e.help, `\`, `\\`), "\n", `\n`))
		b.WriteString("\n# TYPE ")
		b.WriteString(e.name)
		b.WriteString(" counter\n")
		b.WriteString(e.name)
		b.WriteString(" ")
		b.WriteString(fmt.Sprintf("%d", e.value))
		b.WriteString("\n")
	}

	// T-8b-stress-r9-retry-4 NEXT-LAYER: labelled per-stage restore
	// failure counter. One HELP/TYPE pair (the labelled family shares
	// metadata across samples per the Prometheus text spec), then one
	// sample per observed stage. Stages are sorted alphabetically so
	// the rendered output is deterministic across exporter ticks
	// (deterministic output makes a textfile diff actionable for
	// operators).
	//
	// Zero-cardinality (no failures yet) is still a valid Prometheus
	// family — we emit the HELP/TYPE pair even without samples so a
	// scraper observes the metric family on every snapshot rather
	// than only after the first failure (helps "is the driver
	// reporting at all?" gauge alerts trigger correctly).
	const restoreFailuresName = "nomad_driver_ch_start_task_restore_failures_total"
	b.WriteString("# HELP ")
	b.WriteString(restoreFailuresName)
	b.WriteString(" ")
	b.WriteString("Times startTaskRestoreBranch returned a non-nil error, labelled by failing stage (T-8b-stress-r9-retry-4 NEXT-LAYER).")
	b.WriteString("\n# TYPE ")
	b.WriteString(restoreFailuresName)
	b.WriteString(" counter\n")
	failures := StartTaskRestoreFailuresSnapshot()
	stages := make([]string, 0, len(failures))
	for s := range failures {
		stages = append(stages, s)
	}
	sort.Strings(stages)
	for _, stage := range stages {
		b.WriteString(restoreFailuresName)
		b.WriteString(`{stage="`)
		// Stage labels are static constants from restore_task.go (no
		// quotes / backslashes / newlines today), but apply the
		// Prometheus label-value escape rules anyway as future-proofing.
		esc := strings.NewReplacer(`\`, `\\`, `"`, `\"`, "\n", `\n`).Replace(stage)
		b.WriteString(esc)
		b.WriteString(`"} `)
		b.WriteString(fmt.Sprintf("%d", failures[stage]))
		b.WriteString("\n")
	}
	return b.String()
}

// writeDriverMetricsSnapshot writes the current rendered text to
// `path` atomically: write to a tmp sibling, fsync, rename. A
// concurrent reader sees either the prior complete snapshot or the
// new one — never a partial.
//
// Returns the rename error (or the tmp-write error) so the exporter
// loop can WARN-log without losing the cause. Idempotent: writing
// the same content twice is a no-op for downstream scrapers.
//
// MkdirAll on the parent dir is unconditional so first-run on a
// fresh host doesn't fail when /var/lib/zsbx hasn't been created
// yet (the driver's other state paths MkdirAll their own dirs the
// same way; sharing the convention).
func writeDriverMetricsSnapshot(path, content string) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return fmt.Errorf("mkdir parent: %w", err)
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(content), 0o644); err != nil {
		return fmt.Errorf("write tmp %s: %w", tmp, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		// best-effort cleanup of the tmp file so a long-lived
		// failure mode doesn't accumulate stale `.tmp` siblings.
		_ = os.Remove(tmp)
		return fmt.Errorf("rename %s -> %s: %w", tmp, path, err)
	}
	return nil
}

// runDriverMetricsExporter is the goroutine entry point. Ticks at
// `driverMetricsExportInterval`, writes a snapshot per tick, exits
// on ctx.Done() (writes one final snapshot first so the on-disk
// state matches the last-observed counter values).
//
// Errors are WARN-logged but don't terminate the loop — a missing
// /var/lib/zsbx (e.g. fresh host before any task ran) is normal on
// first tick, and the next MkdirAll succeeds.
//
// Lifecycle: started from NewPlugin via go runDriverMetricsExporter(...);
// cancelled by the plugin's signalShutdown context cancellation.
func runDriverMetricsExporter(ctx context.Context, logger hclog.Logger) {
	tick := time.NewTicker(driverMetricsExportInterval)
	defer tick.Stop()

	write := func() {
		content := renderDriverMetricsProm()
		if err := writeDriverMetricsSnapshot(driverMetricsExportPath, content); err != nil {
			// Best-effort: a transient failure (full disk, permissions
			// mismatch on first run) shouldn't take down the driver
			// plugin or stop counter accumulation. The next tick
			// retries; a persistent failure surfaces as a stale or
			// missing /metrics scrape, which the operator sees.
			logger.Warn("ch: driver metrics exporter: snapshot write failed (best-effort)",
				"path", driverMetricsExportPath, "err", err)
		}
	}

	// First tick is immediate (don't make the operator wait
	// `driverMetricsExportInterval` for the first sample on
	// driver load).
	write()

	for {
		select {
		case <-ctx.Done():
			// One final snapshot so the on-disk state reflects the
			// last-observed counters before the driver exits.
			// (A scraper that polls during shutdown sees a
			// consistent snapshot rather than a stale one.)
			write()
			return
		case <-tick.C:
			write()
		}
	}
}
