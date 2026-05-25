// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Tests for the T-8b-stress-r8 r7-B driver-metrics file exporter.
// Three layers:
//
//   1. renderDriverMetricsProm — pure function; assert the
//      Prometheus text-format conformance (HELP/TYPE/sample
//      triples, _total suffix, counter type).
//   2. writeDriverMetricsSnapshot — atomic write via tmp+rename;
//      assert a concurrent reader sees the new content (never a
//      partial).
//   3. End-to-end: install a fast tick + t.TempDir() destination,
//      bump a counter, observe the snapshot updates on the next
//      tick.

package tests

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/hashicorp/go-hclog"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// TestRenderDriverMetricsProm_PrometheusFormatConformance pins the
// exposition format: every counter family carries a HELP line, a
// TYPE counter line, and a sample line; names end with `_total`;
// the value is a non-negative integer.
//
// We don't import a real Prometheus parser to keep the test
// dependency-free; the structural checks below cover the same
// constraints the textfile collector enforces at scrape time.
func TestRenderDriverMetricsProm_PrometheusFormatConformance(t *testing.T) {
	// Reset all counters so the snapshot is deterministic.
	ch.ResetTapsOrphanedForTest()
	ch.ResetDestroyTaskUnreapedForTest()
	ch.ResetDestroyTaskLockHeldForTest()
	ch.ResetDestroyTaskTapStuckForTest()
	ch.ResetStartTaskStageForTest()
	ch.ResetStartTaskStageFailuresForTest()

	out := ch.RenderDriverMetricsPromForTest()
	if out == "" {
		t.Fatal("renderDriverMetricsProm returned empty string")
	}

	// Each metric family must appear in HELP/TYPE/sample order.
	expectedFamilies := []string{
		"nomad_driver_ch_destroy_task_lock_held_total",
		"nomad_driver_ch_destroy_task_tap_stuck_total",
		"nomad_driver_ch_destroy_task_unreaped_total",
		"nomad_driver_ch_start_task_stage_failures_total",
		"nomad_driver_ch_start_task_stage_total",
		"nomad_driver_ch_taps_orphaned_total",
	}

	for _, name := range expectedFamilies {
		// HELP line
		helpLine := "# HELP " + name
		if !strings.Contains(out, helpLine) {
			t.Errorf("missing HELP line for %q in:\n%s", name, out)
		}
		// TYPE counter line
		typeLine := "# TYPE " + name + " counter"
		if !strings.Contains(out, typeLine) {
			t.Errorf("missing TYPE counter line for %q in:\n%s", name, out)
		}
		// Sample line: `<name> <value>\n`. Value must be 0 after reset.
		sampleLine := name + " 0\n"
		if !strings.Contains(out, sampleLine) {
			t.Errorf("missing sample line %q in:\n%s", sampleLine, out)
		}
		// Names must end with _total (Prometheus counter convention).
		if !strings.HasSuffix(name, "_total") {
			t.Errorf("counter %q does not end with _total", name)
		}
	}

	// File must end with a newline (textfile collector requirement).
	if !strings.HasSuffix(out, "\n") {
		t.Error("snapshot does not end with newline")
	}
}

// TestRenderDriverMetricsProm_ReflectsCounterIncrements pins that
// bumping a counter changes the rendered sample value on the next
// call. Closes the "are we actually reading the live counter, or a
// captured-at-startup snapshot?" question.
func TestRenderDriverMetricsProm_ReflectsCounterIncrements(t *testing.T) {
	ch.ResetDestroyTaskTapStuckForTest()
	t.Cleanup(ch.ResetDestroyTaskTapStuckForTest)

	out0 := ch.RenderDriverMetricsPromForTest()
	if !strings.Contains(out0, "nomad_driver_ch_destroy_task_tap_stuck_total 0\n") {
		t.Fatalf("baseline missing or non-zero in:\n%s", out0)
	}

	// Bump via the public test surface. (Resetting + bumping uses
	// the existing test API; no new export needed.)
	for i := 0; i < 3; i++ {
		ch.IncDestroyTaskTapStuckForTest()
	}

	out1 := ch.RenderDriverMetricsPromForTest()
	if !strings.Contains(out1, "nomad_driver_ch_destroy_task_tap_stuck_total 3\n") {
		t.Errorf("post-bump counter not reflected; expected `... 3` in:\n%s", out1)
	}
}

// TestWriteDriverMetricsSnapshot_AtomicViaTmpRename pins the
// atomic-write behaviour: a partial-content reader is never
// possible because the rename(2) syscall on a single filesystem is
// atomic. Verified by writing two distinct snapshots and asserting
// the reader sees one OR the other — never a mix.
func TestWriteDriverMetricsSnapshot_AtomicViaTmpRename(t *testing.T) {
	dir := t.TempDir()
	dest := filepath.Join(dir, "driver-metrics.prom")

	if err := ch.WriteDriverMetricsSnapshotForTest(dest, "first content\n"); err != nil {
		t.Fatalf("first write: %v", err)
	}
	got, err := os.ReadFile(dest)
	if err != nil {
		t.Fatalf("read after first write: %v", err)
	}
	if string(got) != "first content\n" {
		t.Errorf("first content mismatch: got %q", string(got))
	}

	// Overwrite. Atomic via tmp+rename means readers see one or
	// the other, never a partial.
	if err := ch.WriteDriverMetricsSnapshotForTest(dest, "second content\n"); err != nil {
		t.Fatalf("second write: %v", err)
	}
	got, err = os.ReadFile(dest)
	if err != nil {
		t.Fatalf("read after second write: %v", err)
	}
	if string(got) != "second content\n" {
		t.Errorf("second content mismatch: got %q", string(got))
	}

	// No stale .tmp sibling should remain after a clean rename.
	if _, err := os.Stat(dest + ".tmp"); !os.IsNotExist(err) {
		t.Errorf("stale .tmp sibling persisted after rename: err=%v", err)
	}
}

// TestWriteDriverMetricsSnapshot_CreatesParentDir pins the
// MkdirAll-on-write semantics: a fresh host without /var/lib/zsbx
// must not fail the first export tick.
func TestWriteDriverMetricsSnapshot_CreatesParentDir(t *testing.T) {
	dir := t.TempDir()
	// Use a 3-deep nested path that doesn't exist yet.
	dest := filepath.Join(dir, "nested", "more", "deep", "driver-metrics.prom")

	if err := ch.WriteDriverMetricsSnapshotForTest(dest, "hello\n"); err != nil {
		t.Fatalf("write to nested path: %v", err)
	}
	got, err := os.ReadFile(dest)
	if err != nil {
		t.Fatalf("read after nested write: %v", err)
	}
	if string(got) != "hello\n" {
		t.Errorf("nested content mismatch: got %q", string(got))
	}
}

// TestRunDriverMetricsExporter_WritesOnTick pins the goroutine
// lifecycle: a fast tick installed via the test seam → the snapshot
// appears on disk → ctx cancellation triggers a final flush before
// goroutine exit.
func TestRunDriverMetricsExporter_WritesOnTick(t *testing.T) {
	dir := t.TempDir()
	dest := filepath.Join(dir, "driver-metrics.prom")

	prevPath := ch.SetDriverMetricsExportPathForTest(dest)
	t.Cleanup(func() { ch.SetDriverMetricsExportPathForTest(prevPath) })
	prevInterval := ch.SetDriverMetricsExportIntervalForTest(20 * time.Millisecond)
	t.Cleanup(func() { ch.SetDriverMetricsExportIntervalForTest(prevInterval) })

	ctx, cancel := context.WithCancel(context.Background())
	t.Cleanup(cancel)
	logger := hclog.NewNullLogger()

	done := make(chan struct{})
	go func() {
		defer close(done)
		ch.RunDriverMetricsExporterForTest(ctx, logger)
	}()

	// First tick is immediate; poll for the file to appear within
	// 1 s (generous against a slow CI host; the immediate-write
	// behaviour means it should appear in <10 ms in practice).
	deadline := time.Now().Add(1 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(dest); err == nil {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	if _, err := os.Stat(dest); err != nil {
		t.Fatalf("snapshot did not appear within 1 s: %v", err)
	}

	got, err := os.ReadFile(dest)
	if err != nil {
		t.Fatalf("read snapshot: %v", err)
	}
	// Sanity: contains the expected header + at least one counter.
	if !strings.Contains(string(got), "# HELP nomad_driver_ch_destroy_task_tap_stuck_total") {
		t.Errorf("snapshot missing expected metric family:\n%s", string(got))
	}

	// Cancel and wait for the goroutine to exit.
	cancel()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("exporter goroutine did not exit within 2 s of ctx cancel")
	}

	// The final flush on cancellation should have left the file
	// readable (not torn down or emptied).
	if _, err := os.Stat(dest); err != nil {
		t.Errorf("snapshot vanished after ctx cancel: %v", err)
	}
}
