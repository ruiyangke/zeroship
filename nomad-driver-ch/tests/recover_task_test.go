// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-4 sprint test surface: pins the RecoverTask matrix — happy path
// (re-attach to an alive CH process whose API socket still responds),
// process-gone (PID dead → refuse), socket-unresponsive (probe fails →
// refuse), idempotent double-call, plus the corruption-guard cases
// (nil handle, missing required fields).
//
// All tests use the package-level seams in ch/ (probeFn, processAliveFn,
// recoverPollInterval) so no real CH binary is required.

package tests

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/drivers"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// validRecoverState returns a TaskState whose every required field is
// populated, suitable for the matrix' happy path. Tests mutate the
// returned value (or its embedded TaskConfig) to drive the failure
// branches.
//
// CHPid is a synthetic high value rather than os.Getpid() so a forced
// DestroyTask in the test cleanup can SIGKILL the recovered PID without
// killing the test runner (kernel returns ESRCH harmlessly for a missing
// PID).
func validRecoverState(t *testing.T, apiSocket string) *ch.TaskState {
	t.Helper()
	return &ch.TaskState{
		TaskConfig:    &drivers.TaskConfig{ID: "recover-test-id", Name: "recover-test"},
		StartedAt:     time.Now().Add(-time.Minute),
		CHPid:         9999999, // synthetic; tests override processAliveFn
		APISocket:     apiSocket,
		VMIndex:       7,
		Tap:           "zsbx-nm-7",
		Mode:          "cold_boot",
		SandboxId:     "sbx_recover",
		NomadTaskName: "recover-test",
	}
}

// newRecoverHandle wraps state into a *drivers.TaskHandle the way Nomad
// would after persisting StartTask's output.
func newRecoverHandle(t *testing.T, state *ch.TaskState) *drivers.TaskHandle {
	t.Helper()
	h := drivers.NewTaskHandle(ch.TaskHandleVersion)
	// Config is what RecoverTask reads handle.Config.ID from; the
	// state.TaskConfig is the round-trip copy we persist alongside.
	if state.TaskConfig != nil {
		h.Config = state.TaskConfig
	}
	if err := h.SetDriverState(state); err != nil {
		t.Fatalf("SetDriverState: %v", err)
	}
	return h
}

// touchSocket creates an empty regular file at the given path so the
// Probe's stat gate passes. The actual ch-remote info call is faked
// via SetProbeForTest in each test that exercises a probe outcome.
func touchSocket(t *testing.T, path string) {
	t.Helper()
	if err := os.WriteFile(path, nil, 0o600); err != nil {
		t.Fatalf("touch socket %s: %v", path, err)
	}
}

// installAliveSeam overrides processAliveFn to always-alive and restores
// on test cleanup. Use when the test wants the validation gate to pass
// without depending on host PID layout.
func installAliveSeam(t *testing.T) {
	t.Helper()
	prev := ch.SetProcessAliveForTest(func(int) error { return nil })
	t.Cleanup(func() { ch.SetProcessAliveForTest(prev) })
}

// installProbeSeam overrides probeFn with the given fn.
func installProbeSeam(t *testing.T, fn func(*ch.Client, string) error) {
	t.Helper()
	prev := ch.SetProbeForTest(fn)
	t.Cleanup(func() { ch.SetProbeForTest(prev) })
}

// TestRecoverTask_NilHandle is the one case the stub already gets right.
// Retained as-is: nil handle must surface a clear error.
func TestRecoverTask_NilHandle(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	err := p.RecoverTask(nil)
	if err == nil {
		t.Fatal("RecoverTask(nil) should error")
	}
	if !strings.Contains(err.Error(), "nil handle") {
		t.Errorf("unexpected error: %v", err)
	}
}

// TestRecoverTask_ProcessAliveSocketResponsive — happy path. PID liveness
// passes (seam returns nil); the fake probe returns nil (ch-remote info
// would have succeeded against a running CH). After RecoverTask returns
// nil:
//   - p.tasks has the handle (InspectTask succeeds)
//   - the recovered handle reports running state
//   - the persisted api_socket / tap / vm_index round-trip onto the
//     handle's DriverAttributes
//
// Cleanup makes the detached runner observe ESRCH on its next poll tick
// so the supervisor goroutine exits cleanly — no leaks under -race.
func TestRecoverTask_ProcessAliveSocketResponsive(t *testing.T) {
	// Tight poll cadence so the supervisor unblocks within the test.
	prevPoll := ch.SetRecoverPollIntervalForTest(5 * time.Millisecond)
	t.Cleanup(func() { ch.SetRecoverPollIntervalForTest(prevPoll) })

	// Validation calls processAliveFn once; subsequent calls (from the
	// detached runner's poll loop) must report gone so the supervisor
	// exits at test teardown.
	var liveCalls atomic.Int32
	prev := ch.SetProcessAliveForTest(func(int) error {
		if liveCalls.Add(1) == 1 {
			return nil // validation: alive
		}
		return errors.New("ch: process is gone") // supervisor poll: gone
	})
	t.Cleanup(func() { ch.SetProcessAliveForTest(prev) })

	installProbeSeam(t, func(_ *ch.Client, _ string) error { return nil })

	apiSocket := filepath.Join(t.TempDir(), "ch.sock")
	touchSocket(t, apiSocket)
	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	if err := p.RecoverTask(handle); err != nil {
		t.Fatalf("RecoverTask: %v", err)
	}

	// The recovered handle should be visible via InspectTask.
	status, err := p.InspectTask(handle.Config.ID)
	if err != nil {
		t.Fatalf("InspectTask after recover: %v", err)
	}
	if status.State != drivers.TaskStateRunning {
		t.Errorf("recovered task state = %v, want Running", status.State)
	}
	if status.DriverAttributes["api_socket"] != apiSocket {
		t.Errorf("recovered api_socket = %q, want %q", status.DriverAttributes["api_socket"], apiSocket)
	}
	if status.DriverAttributes["tap"] != state.Tap {
		t.Errorf("recovered tap = %q, want %q", status.DriverAttributes["tap"], state.Tap)
	}

	// Wait for the supervisor to observe ESRCH and close exitDone, so
	// the test doesn't leak the goroutine under -race.
	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()
	exitCh, err := p.WaitTask(ctx, handle.Config.ID)
	if err != nil {
		t.Fatalf("WaitTask: %v", err)
	}
	select {
	case <-exitCh:
		// supervisor exited; expected after the second poll returns
		// "gone" via the seam.
	case <-time.After(500 * time.Millisecond):
		t.Fatal("supervisor did not exit after ESRCH within 500ms")
	}
}

// TestRecoverTask_DoubleCallIsIdempotent — RecoverTask called twice
// against the same handle ID must return nil the second time without
// re-attaching (the existing in-memory handle is authoritative). The
// probe seam call count is the witness.
func TestRecoverTask_DoubleCallIsIdempotent(t *testing.T) {
	prevPoll := ch.SetRecoverPollIntervalForTest(5 * time.Millisecond)
	t.Cleanup(func() { ch.SetRecoverPollIntervalForTest(prevPoll) })

	var liveCalls atomic.Int32
	prev := ch.SetProcessAliveForTest(func(int) error {
		if liveCalls.Add(1) == 1 {
			return nil
		}
		return errors.New("ch: process is gone")
	})
	t.Cleanup(func() { ch.SetProcessAliveForTest(prev) })

	var probeCount atomic.Int32
	installProbeSeam(t, func(_ *ch.Client, _ string) error {
		probeCount.Add(1)
		return nil
	})

	apiSocket := filepath.Join(t.TempDir(), "ch.sock")
	touchSocket(t, apiSocket)
	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	if err := p.RecoverTask(handle); err != nil {
		t.Fatalf("first RecoverTask: %v", err)
	}
	if got := probeCount.Load(); got != 1 {
		t.Errorf("after first call, probe count = %d, want 1", got)
	}
	if err := p.RecoverTask(handle); err != nil {
		t.Fatalf("second RecoverTask: %v", err)
	}
	if got := probeCount.Load(); got != 1 {
		t.Errorf("after second call, probe count = %d, want 1 (idempotent)", got)
	}

	// Drain the supervisor goroutine.
	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()
	exitCh, err := p.WaitTask(ctx, handle.Config.ID)
	if err != nil {
		t.Fatalf("WaitTask: %v", err)
	}
	<-exitCh
}

// TestRecoverTask_ProcessGone — PID is dead (ESRCH). RecoverTask must
// refuse with a message naming the condition. The probe is never reached
// because PID liveness is the cheap gate before the socket dial.
func TestRecoverTask_ProcessGone(t *testing.T) {
	// Lie about PID liveness: return "gone" unconditionally.
	prev := ch.SetProcessAliveForTest(func(int) error {
		return errors.New("ch: process 9999999 is gone")
	})
	t.Cleanup(func() { ch.SetProcessAliveForTest(prev) })

	var probeCalled atomic.Bool
	installProbeSeam(t, func(_ *ch.Client, _ string) error {
		probeCalled.Store(true)
		return nil
	})

	apiSocket := filepath.Join(t.TempDir(), "ch.sock")
	touchSocket(t, apiSocket)
	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	err := p.RecoverTask(handle)
	if err == nil {
		t.Fatal("RecoverTask should fail when PID is gone")
	}
	if !strings.Contains(err.Error(), "gone") {
		t.Errorf("error %q does not mention 'gone'", err.Error())
	}
	if probeCalled.Load() {
		t.Error("probe should not be reached when PID liveness fails")
	}
	if _, err := p.InspectTask(handle.Config.ID); err == nil {
		t.Error("InspectTask should fail — handle must not be in p.tasks after refusal")
	}
}

// TestRecoverTask_ProcessAliveSocketDead — PID is alive, socket file is
// present, but ch-remote info fails (e.g. stale socket from a crashed CH;
// or process at PID is unrelated and doesn't speak the CH protocol).
// The probe-returns-error branch must surface stderr verbatim.
func TestRecoverTask_ProcessAliveSocketDead(t *testing.T) {
	installAliveSeam(t)
	installProbeSeam(t, func(_ *ch.Client, _ string) error {
		return errors.New("ch-remote: dial unix /run/ch/x.sock: connect: connection refused")
	})

	apiSocket := filepath.Join(t.TempDir(), "ch.sock")
	touchSocket(t, apiSocket)
	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	err := p.RecoverTask(handle)
	if err == nil {
		t.Fatal("RecoverTask should fail when ch-remote probe fails")
	}
	if !strings.Contains(err.Error(), "connection refused") {
		t.Errorf("error %q does not preserve probe stderr", err.Error())
	}
	if _, err := p.InspectTask(handle.Config.ID); err == nil {
		t.Error("InspectTask should fail — handle must not be in p.tasks after refusal")
	}
}

// TestRecoverTask_ProcessAliveButNotCH — the PID is alive but it's been
// reused for a process unrelated to CH. The discriminator is the
// ch-remote info call: a non-CH process won't respond on the per-VM
// api-socket; the probe returns an error and RecoverTask refuses.
//
// Pins the contract that we DO NOT trust PID liveness alone (the
// historical bash wrapper's bug — it `kill -0`'d the PID and called it
// good).
func TestRecoverTask_ProcessAliveButNotCH(t *testing.T) {
	installAliveSeam(t)
	installProbeSeam(t, func(_ *ch.Client, _ string) error {
		return errors.New("ch-remote: HTTP/1.1 404 Not Found (probably not cloud-hypervisor)")
	})

	apiSocket := filepath.Join(t.TempDir(), "ch.sock")
	touchSocket(t, apiSocket)
	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	err := p.RecoverTask(handle)
	if err == nil {
		t.Fatal("RecoverTask should fail when ch-remote probe says it's not CH")
	}
	if !strings.Contains(err.Error(), "404") {
		t.Errorf("error %q does not surface probe stderr", err.Error())
	}
	if _, err := p.InspectTask(handle.Config.ID); err == nil {
		t.Error("InspectTask should fail — PID-alive-but-not-CH must not be in p.tasks")
	}
}

// TestRecoverTask_MissingSocketFile — the api socket path does not exist
// on disk. The real Probe must surface a clear "socket does not exist"
// message before reaching ch-remote. Exercises the real Probe (no probe
// seam) so we pin the file-stat branch end-to-end.
func TestRecoverTask_MissingSocketFile(t *testing.T) {
	installAliveSeam(t)
	// Do NOT override probeFn — let Client.Probe run for real so we
	// pin the os.Stat → "does not exist" message.

	apiSocket := filepath.Join(t.TempDir(), "absent.sock")
	// Intentionally DO NOT touchSocket.

	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	err := p.RecoverTask(handle)
	if err == nil {
		t.Fatal("RecoverTask should fail when socket file is missing")
	}
	if !strings.Contains(err.Error(), "does not exist") {
		t.Errorf("error %q should mention 'does not exist'", err.Error())
	}
}

// TestRecoverTask_RejectsCorruptState exercises TaskState.Validate via
// RecoverTask: each malformed field must trigger a specific refusal with
// the field name surfaced.
func TestRecoverTask_RejectsCorruptState(t *testing.T) {
	installAliveSeam(t)

	cases := []struct {
		name      string
		mutate    func(*ch.TaskState)
		wantSubst string
	}{
		{"zero PID", func(s *ch.TaskState) { s.CHPid = 0 }, "CHPid"},
		{"negative PID", func(s *ch.TaskState) { s.CHPid = -1 }, "CHPid"},
		{"empty APISocket", func(s *ch.TaskState) { s.APISocket = "" }, "APISocket"},
		{"empty Tap", func(s *ch.TaskState) { s.Tap = "" }, "Tap"},
		{"VMIndex 0", func(s *ch.TaskState) { s.VMIndex = 0 }, "VMIndex"},
		{"VMIndex too large", func(s *ch.TaskState) { s.VMIndex = 200 }, "VMIndex"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			apiSocket := filepath.Join(t.TempDir(), "ch.sock")
			touchSocket(t, apiSocket)
			state := validRecoverState(t, apiSocket)
			tc.mutate(state)
			handle := newRecoverHandle(t, state)

			p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
			err := p.RecoverTask(handle)
			if err == nil {
				t.Fatalf("RecoverTask should fail for %s", tc.name)
			}
			if !strings.Contains(err.Error(), tc.wantSubst) {
				t.Errorf("error %q does not mention field %q", err.Error(), tc.wantSubst)
			}
		})
	}
}

// TestRecoverTask_DetachedRunnerObservesESRCH — drive the detached
// runner's polling loop directly: override processAliveFn to flip from
// alive-to-gone after a single tick, and confirm the supervisor closes
// exitDone (visible via WaitTask returning).
func TestRecoverTask_DetachedRunnerObservesESRCH(t *testing.T) {
	prevPoll := ch.SetRecoverPollIntervalForTest(5 * time.Millisecond)
	t.Cleanup(func() { ch.SetRecoverPollIntervalForTest(prevPoll) })

	var calls atomic.Int32
	prev := ch.SetProcessAliveForTest(func(int) error {
		if calls.Add(1) == 1 {
			return nil
		}
		return errors.New("ch: process is gone")
	})
	t.Cleanup(func() { ch.SetProcessAliveForTest(prev) })

	installProbeSeam(t, func(_ *ch.Client, _ string) error { return nil })

	apiSocket := filepath.Join(t.TempDir(), "ch.sock")
	touchSocket(t, apiSocket)
	state := validRecoverState(t, apiSocket)
	handle := newRecoverHandle(t, state)

	p := ch.NewPluginForTest(hclog.NewNullLogger(), nil)
	if err := p.RecoverTask(handle); err != nil {
		t.Fatalf("RecoverTask: %v", err)
	}

	exitCh, err := p.WaitTask(context.Background(), handle.Config.ID)
	if err != nil {
		t.Fatalf("WaitTask: %v", err)
	}
	select {
	case result := <-exitCh:
		// Detached runner: -1 (signal-terminated convention; we have
		// no exit-status access for a non-child process).
		if result.ExitCode != -1 {
			t.Errorf("detached exit code = %d, want -1", result.ExitCode)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("WaitTask did not return within 2s — supervisor stuck?")
	}
}
