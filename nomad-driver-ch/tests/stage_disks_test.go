// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Option C Phase 2 (2026-05-25 staging-locality ADR) tests: pin the
// driver-side disk-image staging surface that replaces the controller's
// spawn_blocking `create_ext4_image_if_missing` block at
// `crates/sandbox/src/backend/nomad_ch.rs:777-792`.
//
// Test shape mirrors r4-A's `exitDone`-wait tests and r5-A's
// `F_OFD_SETLK` probe tests — fake-out the side-effecting truncate +
// mkfs.ext4 via the SetStageImageOpForTest seam so tests can drive
// the failure paths without a real ENOSPC / missing-binary scenario.

package tests

import (
	"errors"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// TestStartTask_StagesDiskImages_WhenFlagSet pins the happy path: with
// TaskConfig.StageDiskImages=true, StartTask invokes the staging op
// for BOTH workspace_img and user_home_img BEFORE spawning CH, with
// the per-image size matching the controller's
// `workspace_image_size_gb` default (20 GiB → 20 × 1024^3 bytes).
//
// The test uses the SetStageImageOpForTest seam to record (path,
// sizeBytes) per invocation without engineering a real mkfs.ext4 run
// (which would need root + a few seconds per image). The seam writes
// a 1-byte stub at each path so the existing preflightDiskPaths
// stat-check downstream of staging still passes.
func TestStartTask_StagesDiskImages_WhenFlagSet(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prevTap := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTap) })

	type stageCall struct {
		path string
		size int64
	}
	var calls []stageCall
	prevOp := ch.SetStageImageOpForTest(func(path string, sizeBytes int64) error {
		calls = append(calls, stageCall{path: path, size: sizeBytes})
		// Write a 1-byte stub so preflightDiskPaths (size > 0 check)
		// passes downstream.
		if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
			return err
		}
		return os.WriteFile(path, []byte("x"), 0o600)
	})
	t.Cleanup(func() { ch.SetStageImageOpForTest(prevOp) })

	ch.ResetStartTaskStageForTest()
	ch.ResetStartTaskStageFailuresForTest()

	// Use scratch paths so the staging op's mkdir + write don't race
	// against a sibling test. Each path under t.TempDir() vanishes
	// at test-end.
	scratch := t.TempDir()
	workspaceImg := filepath.Join(scratch, "stage-ws", "workspace.img")
	userHomeImg := filepath.Join(scratch, "stage-uh", "userhome.img")

	cfg := ch.TaskConfig{
		VMIndex:         7,
		Kernel:          "/opt/zsbx/vmlinuz",
		CPUs:            2,
		MemoryMB:        256,
		SandboxId:       "sbx_test",
		WorkspaceImg:    workspaceImg,
		UserHomeImg:     userHomeImg,
		PubkeyHex:       "deadbeef",
		Net:             []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}},
		StageDiskImages: true,
	}

	var captured *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		captured = newFakeRunner(cmd)
		return captured
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if captured == nil {
		t.Fatal("runner factory not invoked")
	}
	t.Cleanup(func() { close(captured.waitCh) })

	// Two calls: workspace + user-home, in that order.
	if len(calls) != 2 {
		t.Fatalf("stageImageOp call count = %d, want 2 (workspace + user_home)", len(calls))
	}
	const want20GiB int64 = 20 * 1024 * 1024 * 1024
	for i, c := range calls {
		if c.size != want20GiB {
			t.Errorf("call[%d].size = %d, want %d (20 GiB matching controller's workspace_image_size_gb default)",
				i, c.size, want20GiB)
		}
	}
	// Path ordering: workspace first, then user-home (this matters for
	// the failure-path test below — first error wins).
	if calls[0].path != workspaceImg {
		t.Errorf("call[0].path = %q, want %q (workspace before user-home)", calls[0].path, workspaceImg)
	}
	if calls[1].path != userHomeImg {
		t.Errorf("call[1].path = %q, want %q (user-home after workspace)", calls[1].path, userHomeImg)
	}

	// Counter assertions: success counter bumps once (the staging
	// surface engaged); failures stays at 0.
	if got := ch.StartTaskStageTotal(); got != 1 {
		t.Errorf("StartTaskStageTotal = %d, want 1 (Option C Phase 2 staging engaged)", got)
	}
	if got := ch.StartTaskStageFailuresTotal(); got != 0 {
		t.Errorf("StartTaskStageFailuresTotal = %d, want 0 (happy path)", got)
	}
}

// TestStartTask_SkipsStaging_WhenFlagFalse pins the back-compat path:
// when TaskConfig.StageDiskImages=false (default), the staging op is
// NOT invoked — the controller-side `create_ext4_image_if_missing`
// path remains in charge of materialising the images, and the driver
// consumes the pre-staged paths verbatim. This is the Phase 2 default
// (Phase 4 flips it after stress validation).
func TestStartTask_SkipsStaging_WhenFlagFalse(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prevTap := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTap) })

	var staged atomic.Bool
	prevOp := ch.SetStageImageOpForTest(func(path string, sizeBytes int64) error {
		staged.Store(true)
		return nil
	})
	t.Cleanup(func() { ch.SetStageImageOpForTest(prevOp) })

	ch.ResetStartTaskStageForTest()
	ch.ResetStartTaskStageFailuresForTest()

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}
	// Explicitly leave StageDiskImages=false (the zero value).

	var captured *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		captured = newFakeRunner(cmd)
		return captured
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if captured == nil {
		t.Fatal("runner factory not invoked")
	}
	t.Cleanup(func() { close(captured.waitCh) })

	// The staging op MUST NOT have fired.
	if staged.Load() {
		t.Error("stageImageOp invoked despite StageDiskImages=false (back-compat violation)")
	}
	// Counters must remain at their reset baseline.
	if got := ch.StartTaskStageTotal(); got != 0 {
		t.Errorf("StartTaskStageTotal = %d, want 0 (flag was false)", got)
	}
	if got := ch.StartTaskStageFailuresTotal(); got != 0 {
		t.Errorf("StartTaskStageFailuresTotal = %d, want 0 (flag was false)", got)
	}
}

// TestStartTask_StagingErrorPreservesNoSpawn pins the error contract:
// when the staging op fails (e.g. mkfs.ext4 ENOSPC, mkdir EACCES),
// StartTask MUST return the error WITHOUT spawning CH. The
// controller-side CreateGuard rollback contract depends on this — a
// half-spawned CH process with a failed staging op would leave a
// running VM whose disks are missing and the controller would have
// no signal to release the vm_index / DB row.
func TestStartTask_StagingErrorPreservesNoSpawn(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prevTap := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTap) })

	// Simulate mkfs.ext4 failing on the FIRST image (workspace). The
	// seam returns an error so stageDiskImages bails before the
	// second image is even attempted.
	sentinel := errors.New("simulated mkfs.ext4 -q -F failed: ENOSPC")
	prevOp := ch.SetStageImageOpForTest(func(path string, sizeBytes int64) error {
		return sentinel
	})
	t.Cleanup(func() { ch.SetStageImageOpForTest(prevOp) })

	ch.ResetStartTaskStageForTest()
	ch.ResetStartTaskStageFailuresForTest()

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}
	cfg.StageDiskImages = true

	// Track whether the runner factory was invoked — the factory is
	// the proxy for "did CH spawn happen". On a staging error the
	// factory MUST NOT be called.
	var factoryInvoked atomic.Bool
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		factoryInvoked.Store(true)
		return newFakeRunner(cmd)
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected staging error, got nil")
	}
	if !errors.Is(err, sentinel) {
		// Allow wrapped error chain — the production code uses fmt.Errorf
		// with %w so errors.Is unwraps.
		if !strings.Contains(err.Error(), sentinel.Error()) {
			t.Errorf("error %q does not wrap sentinel %q", err.Error(), sentinel.Error())
		}
	}
	// Driver-tagged prefix so the Nomad task log line is greppable.
	if !strings.Contains(err.Error(), "ch: stageDiskImages") {
		t.Errorf("error %q missing 'ch: stageDiskImages' prefix", err.Error())
	}
	// THE invariant: CH must not have been spawned.
	if factoryInvoked.Load() {
		t.Error("runner factory invoked despite staging failure (CreateGuard rollback contract violated)")
	}

	// Counters: total bumps once (entered the staging surface),
	// failures bumps once (errored out).
	if got := ch.StartTaskStageTotal(); got != 1 {
		t.Errorf("StartTaskStageTotal = %d, want 1 (one staging attempt entered)", got)
	}
	if got := ch.StartTaskStageFailuresTotal(); got != 1 {
		t.Errorf("StartTaskStageFailuresTotal = %d, want 1 (one staging attempt failed)", got)
	}
}

// TestStageDiskImagesForTest_RejectsEmptyPath is a pure-function
// pin: stageDiskImages must surface "field is empty" errors with a
// useful field name BEFORE touching the filesystem. Mirrors the
// validateColdBoot pattern: every error names the field so an
// operator triaging the task log knows what to fix.
func TestStageDiskImagesForTest_RejectsEmptyPath(t *testing.T) {
	cases := []struct {
		name    string
		mutate  func(*ch.TaskConfig)
		wantSub string
	}{
		{
			name:    "empty WorkspaceImg",
			mutate:  func(c *ch.TaskConfig) { c.WorkspaceImg = "" },
			wantSub: "workspace_img",
		},
		{
			name:    "empty UserHomeImg",
			mutate:  func(c *ch.TaskConfig) { c.UserHomeImg = "" },
			wantSub: "user_home_img",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			scratch := t.TempDir()
			cfg := ch.TaskConfig{
				WorkspaceImg:    filepath.Join(scratch, "ws.img"),
				UserHomeImg:     filepath.Join(scratch, "uh.img"),
				StageDiskImages: true,
			}
			tc.mutate(&cfg)

			ch.ResetStartTaskStageForTest()
			ch.ResetStartTaskStageFailuresForTest()

			// Swap in a no-op staging op so the failure mode is
			// strictly the field-validation branch (not a real
			// mkfs.ext4 failure on the t.TempDir() FS).
			prev := ch.SetStageImageOpForTest(func(string, int64) error { return nil })
			t.Cleanup(func() { ch.SetStageImageOpForTest(prev) })

			err := ch.StageDiskImagesForTest(&cfg)
			if err == nil {
				t.Fatalf("expected error for %s, got nil", tc.name)
			}
			if !strings.Contains(err.Error(), tc.wantSub) {
				t.Errorf("error %q does not mention field %q", err.Error(), tc.wantSub)
			}
			if got := ch.StartTaskStageFailuresTotal(); got != 1 {
				t.Errorf("StartTaskStageFailuresTotal = %d, want 1", got)
			}
		})
	}
}
