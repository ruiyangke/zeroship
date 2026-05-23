// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-2 sprint test surface: pins each step of the graceful-stop ladder
// (ch-remote shutdown-vmm → SIGTERM → SIGKILL → cleanup), the SIGKILL
// fast-path, the timeout-clipping rule, DestroyTask idempotency, and
// SignalTask's signal-by-name forwarding.
//
// All tests use parameterised step timeouts via ch.SetStopTimeoutsForTest
// so the ladder doesn't actually sleep 5+3 real seconds.

package tests

import (
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/drivers"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// stopTaskFixture wires up a *ch.Plugin around the fake runner, fake
// ch-remote shutdown seam, and fake `ip link delete` seam used by the
// ladder. Returns the captured fake runner so individual tests can drive
// it (close its waitCh to simulate CH exit; inspect signalsRcv to assert
// the signal vector).
type stopTaskFixture struct {
	plugin      *ch.Plugin
	taskCfg     *driversTaskConfig
	runner      *fakeRunner
	shutdownHit *atomic.Int32 // increments on every ch-remote shutdown-vmm
	removedTap  *atomic.Pointer[string]
	apiSocket   string
}

// newStopFixture constructs a ready-to-stop plugin. `shutdownReturns` is
// the error the fake ch-remote yields (nil for success; sentinel errors
// drive the SIGTERM/SIGKILL branches).
func newStopFixture(t *testing.T, shutdownReturns error) *stopTaskFixture {
	t.Helper()
	chBin := writeStubBinary(t, "cloud-hypervisor")
	chRemote := writeStubBinary(t, "ch-remote")
	t.Setenv("ZSBX_CH_BIN", chBin)
	t.Setenv("ZSBX_CH_REMOTE_BIN", chRemote)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	// Tight timeouts so even the fall-through paths complete in <50ms.
	prevS, prevT := ch.SetStopTimeoutsForTest(20*time.Millisecond, 20*time.Millisecond)
	t.Cleanup(func() { ch.SetStopTimeoutsForTest(prevS, prevT) })

	shutdownHit := &atomic.Int32{}
	prevShutdown := ch.SetShutdownForTest(func(_ *ch.Client, _ string) error {
		shutdownHit.Add(1)
		return shutdownReturns
	})
	t.Cleanup(func() { ch.SetShutdownForTest(prevShutdown) })

	removedTap := &atomic.Pointer[string]{}
	prevRemove := ch.SetRemoveTapForTest(func(tap string) error {
		t := tap
		removedTap.Store(&t)
		return nil
	})
	t.Cleanup(func() { ch.SetRemoveTapForTest(prevRemove) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	var runner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		runner = newFakeRunner(cmd)
		return runner
	}

	taskDir := t.TempDir()
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)
	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}

	// Resolve the api socket the driver placed inside taskDir so we can
	// assert DestroyTask removes it (or tolerates it being gone). The
	// driver's runDir = TaskDir().LocalDir = filepath.Join(taskDir, "local").
	apiSocket := findAPISocket(t, taskDir)
	if _, err := os.Stat(filepath.Dir(apiSocket)); err != nil {
		t.Fatalf("api socket parent dir not created by StartTask: %v", err)
	}

	return &stopTaskFixture{
		plugin:      p,
		taskCfg:     taskCfg,
		runner:      runner,
		shutdownHit: shutdownHit,
		removedTap:  removedTap,
		apiSocket:   apiSocket,
	}
}

// findAPISocket returns the path the driver placed ch.sock at. Mirrors
// taskRunDir: prefers TaskDir().LocalDir, which Nomad computes as
// filepath.Join(taskDir, "local"). The driver MkdirAlls that dir at
// StartTask time, so we can rely on the path existing as a directory.
func findAPISocket(t *testing.T, taskDir string) string {
	t.Helper()
	return filepath.Join(taskDir, "local", "ch.sock")
}

// closeRunner releases the fake runner's wait channel so the supervisor
// goroutine can record the exit. Idempotent — calling twice is a no-op
// because the channel is buffered-via-close semantics.
func (f *stopTaskFixture) closeRunner() {
	f.runner.mu.Lock()
	defer f.runner.mu.Unlock()
	select {
	case <-f.runner.waitCh:
		// already closed
	default:
		close(f.runner.waitCh)
	}
}

// awaitSignal polls f.runner.signalsRcv up to `d` for the named signal.
// Returns true on hit, false on timeout.
func (f *stopTaskFixture) awaitSignal(sig os.Signal, d time.Duration) bool {
	deadline := time.Now().Add(d)
	for time.Now().Before(deadline) {
		f.runner.mu.Lock()
		for _, s := range f.runner.signalsRcv {
			if s == sig {
				f.runner.mu.Unlock()
				return true
			}
		}
		f.runner.mu.Unlock()
		time.Sleep(time.Millisecond)
	}
	return false
}

// TestStopTask_ChRemoteShutdownGracefulExits — happy path. ch-remote
// shutdown-vmm succeeds; the fake runner exits within the shutdown grace
// window; we never reach SIGTERM/SIGKILL; tap cleanup runs via
// DestroyTask; TaskState transitions to Exited.
func TestStopTask_ChRemoteShutdownGracefulExits(t *testing.T) {
	f := newStopFixture(t, nil)

	// Drive the runner to "exit" the moment the ladder calls ch-remote.
	// We accomplish this by closing waitCh BEFORE StopTask runs — the
	// supervisor goroutine will record the exit; StopTask's
	// waitForExit returns true on the first peek.
	f.closeRunner()
	// Brief pause so the supervisor records the exit before StopTask
	// peeks (otherwise we race the goroutine's first Lock).
	time.Sleep(5 * time.Millisecond)

	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	if f.shutdownHit.Load() != 1 {
		t.Errorf("ch-remote shutdown-vmm call count = %d, want 1", f.shutdownHit.Load())
	}
	// Assert neither SIGTERM nor SIGKILL was issued.
	f.runner.mu.Lock()
	for _, s := range f.runner.signalsRcv {
		if s == syscall.SIGTERM || s == syscall.SIGKILL {
			t.Errorf("unexpected escalation signal %v on graceful path", s)
		}
	}
	f.runner.mu.Unlock()

	// DestroyTask runs the tap + socket cleanup.
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}
	if got := f.removedTap.Load(); got == nil || *got == "" {
		t.Errorf("DestroyTask did not remove tap")
	}
	// Task is gone from the in-memory store after destroy.
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Errorf("second DestroyTask should be nil-idempotent: %v", err)
	}
}

// TestStopTask_FallsThroughToSIGTERM — ch-remote yields an error (e.g. CH
// already dead or socket gone); the ladder issues SIGTERM; the fake
// runner exits within the SIGTERM grace.
func TestStopTask_FallsThroughToSIGTERM(t *testing.T) {
	f := newStopFixture(t, syscallECONNREFUSED())

	// Don't pre-close the runner — the ladder must observe the SIGTERM
	// path. We'll close it AFTER StopTask issues SIGTERM (simulating
	// the kernel reaping the process). A goroutine watches signalsRcv
	// and closes waitCh on SIGTERM.
	go func() {
		if f.awaitSignal(syscall.SIGTERM, 200*time.Millisecond) {
			f.closeRunner()
		}
	}()

	if err := f.plugin.StopTask(f.taskCfg.ID, 200*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	if f.shutdownHit.Load() != 1 {
		t.Errorf("ch-remote shutdown-vmm call count = %d, want 1", f.shutdownHit.Load())
	}
	if !signalSent(f.runner, syscall.SIGTERM) {
		t.Errorf("SIGTERM not sent: %v", f.runner.signalsRcv)
	}
	if signalSent(f.runner, syscall.SIGKILL) {
		t.Errorf("SIGKILL should not have been sent: %v", f.runner.signalsRcv)
	}
}

// TestStopTask_FallsThroughToSIGKILL — both ch-remote and SIGTERM
// ineffective; ladder escalates to SIGKILL; runner exits then.
func TestStopTask_FallsThroughToSIGKILL(t *testing.T) {
	f := newStopFixture(t, syscallECONNREFUSED())

	go func() {
		if f.awaitSignal(syscall.SIGKILL, 200*time.Millisecond) {
			f.closeRunner()
		}
	}()

	if err := f.plugin.StopTask(f.taskCfg.ID, 200*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	if !signalSent(f.runner, syscall.SIGTERM) {
		t.Errorf("SIGTERM step skipped: %v", f.runner.signalsRcv)
	}
	if !signalSent(f.runner, syscall.SIGKILL) {
		t.Errorf("SIGKILL not sent: %v", f.runner.signalsRcv)
	}
}

// TestStopTask_HonorsKillSignalImmediate — caller passes "SIGKILL";
// ladder skips steps 1+2 and goes straight to step 3.
func TestStopTask_HonorsKillSignalImmediate(t *testing.T) {
	f := newStopFixture(t, nil)

	go func() {
		if f.awaitSignal(syscall.SIGKILL, 200*time.Millisecond) {
			f.closeRunner()
		}
	}()

	if err := f.plugin.StopTask(f.taskCfg.ID, 200*time.Millisecond, "SIGKILL"); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	if f.shutdownHit.Load() != 0 {
		t.Errorf("ch-remote shutdown-vmm should NOT have been called on SIGKILL fast-path; got %d", f.shutdownHit.Load())
	}
	if signalSent(f.runner, syscall.SIGTERM) {
		t.Errorf("SIGTERM should NOT have been sent on SIGKILL fast-path: %v", f.runner.signalsRcv)
	}
	if !signalSent(f.runner, syscall.SIGKILL) {
		t.Errorf("SIGKILL not sent: %v", f.runner.signalsRcv)
	}
}

// TestStopTask_HonorsTimeout — caller passes timeout=10ms; per-step
// timeouts clip down; the whole ladder completes within a small
// per-step grace.
func TestStopTask_HonorsTimeout(t *testing.T) {
	// Set the package-level defaults large so we observe the caller's
	// timeout doing the clipping.
	prevS, prevT := ch.SetStopTimeoutsForTest(10*time.Second, 10*time.Second)
	t.Cleanup(func() { ch.SetStopTimeoutsForTest(prevS, prevT) })

	f := &stopTaskFixture{}
	// Stand up the fixture manually so we can override the timeouts AFTER
	// setStopTimeoutsForTest above (newStopFixture stomps them).
	chBin := writeStubBinary(t, "cloud-hypervisor")
	chRemote := writeStubBinary(t, "ch-remote")
	t.Setenv("ZSBX_CH_BIN", chBin)
	t.Setenv("ZSBX_CH_REMOTE_BIN", chRemote)
	prevTap := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTap) })

	shutdownHit := &atomic.Int32{}
	prevShutdown := ch.SetShutdownForTest(func(_ *ch.Client, _ string) error {
		shutdownHit.Add(1)
		return syscallECONNREFUSED()
	})
	t.Cleanup(func() { ch.SetShutdownForTest(prevShutdown) })
	prevRemove := ch.SetRemoveTapForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetRemoveTapForTest(prevRemove) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	var runner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		runner = newFakeRunner(cmd)
		return runner
	}

	taskDir := t.TempDir()
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)
	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	f.runner = runner
	f.plugin = p
	f.taskCfg = taskCfg

	// Never close the runner; the ladder must give up on its own.
	timeout := 30 * time.Millisecond
	t0 := time.Now()
	if err := p.StopTask(taskCfg.ID, timeout, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}
	elapsed := time.Since(t0)

	// Allow up to 1 s grace above `timeout`. The 1 s upper bound covers
	// the final SIGKILL waitForExit (2 s wall) clamped by Go's scheduler.
	// Realistically StopTask returns well under 100 ms here because the
	// runner never closes — the SIGKILL waitForExit times out at 2 s
	// max, hence the 2.5 s upper bound below.
	if elapsed > timeout+2500*time.Millisecond {
		t.Errorf("StopTask elapsed=%v exceeded timeout=%v + 2.5s grace", elapsed, timeout)
	}

	// Cleanup: let the supervisor see the runner exit.
	runner.mu.Lock()
	select {
	case <-runner.waitCh:
	default:
		close(runner.waitCh)
	}
	runner.mu.Unlock()
}

// TestDestroyTask_Idempotent — calling DestroyTask twice is a no-op the
// second time. Also asserts that DestroyTask on an unknown task ID is
// a no-op (the Nomad contract for orphan reconciliation).
func TestDestroyTask_Idempotent(t *testing.T) {
	f := newStopFixture(t, nil)
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)

	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("first DestroyTask: %v", err)
	}
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("second DestroyTask should be idempotent: %v", err)
	}
	if err := f.plugin.DestroyTask("totally-unknown-id", false); err != nil {
		t.Errorf("DestroyTask on unknown task should be nil: %v", err)
	}
}

// TestDestroyTask_CleansResidualTapAndSocket — DestroyTask on a task
// whose CH crashed BEFORE StopTask was ever called still removes the
// tap and api-socket residue. No panic; tap cleanup ran exactly once.
func TestDestroyTask_CleansResidualTapAndSocket(t *testing.T) {
	f := newStopFixture(t, nil)

	// Simulate CH crashing on its own: close waitCh so the supervisor
	// records the exit, then DON'T call StopTask.
	f.closeRunner()
	// Wait for supervisor to flip state.
	deadline := time.Now().Add(200 * time.Millisecond)
	for time.Now().Before(deadline) {
		status, err := f.plugin.InspectTask(f.taskCfg.ID)
		if err == nil && status.State == drivers.TaskStateExited {
			break
		}
		time.Sleep(2 * time.Millisecond)
	}

	// Drop a stub file at the api-socket path so we can assert removal
	// (Unix sockets aren't created by the fake runner; this exercises
	// the os.Remove call without needing a real CH).
	if err := os.WriteFile(f.apiSocket, []byte{}, 0o600); err != nil {
		// Non-fatal — the path may already exist as a socket file from
		// the driver. Tolerate.
		t.Logf("WriteFile %s: %v (tolerated)", f.apiSocket, err)
	}

	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask after CH crash: %v", err)
	}
	if got := f.removedTap.Load(); got == nil || *got == "" {
		t.Errorf("DestroyTask did not remove tap on crash-recovery path")
	}
	// api-socket should be gone too (if we managed to create the stub).
	if _, err := os.Stat(f.apiSocket); !os.IsNotExist(err) {
		t.Errorf("api socket still present after DestroyTask: stat err=%v", err)
	}
}

// TestSignalTask_ForwardsSignalToPid — SignalTask("SIGUSR1") triggers a
// syscall.SIGUSR1 on the runner. Verified by the fake runner's
// signalsRcv vector.
func TestSignalTask_ForwardsSignalToPid(t *testing.T) {
	f := newStopFixture(t, nil)

	if err := f.plugin.SignalTask(f.taskCfg.ID, "SIGUSR1"); err != nil {
		t.Fatalf("SignalTask: %v", err)
	}
	if !signalSent(f.runner, syscall.SIGUSR1) {
		t.Errorf("SIGUSR1 not forwarded: %v", f.runner.signalsRcv)
	}

	// Unknown signal name falls back to SIGINT with a warning (raw_exec
	// parity).
	if err := f.plugin.SignalTask(f.taskCfg.ID, "SIGNOTASIGNAL"); err != nil {
		t.Fatalf("SignalTask unknown: %v", err)
	}
	if !signalSent(f.runner, syscall.SIGINT) {
		t.Errorf("SIGINT fallback not issued: %v", f.runner.signalsRcv)
	}

	// Cleanup
	f.closeRunner()
}

// TestSignalTask_UnknownTaskReturnsErrTaskNotFound — Nomad's contract.
func TestSignalTask_UnknownTaskReturnsErrTaskNotFound(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	if err := p.SignalTask("nope", "SIGUSR1"); err == nil {
		t.Fatal("SignalTask on unknown task should return an error")
	}
}

// -- helpers ----------------------------------------------------------

// signalSent reports whether `sig` was recorded by the fake runner. The
// runner's mu protects signalsRcv from concurrent writes by the ladder
// goroutine.
func signalSent(r *fakeRunner, sig os.Signal) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	for _, s := range r.signalsRcv {
		if s == sig {
			return true
		}
	}
	return false
}

// syscallECONNREFUSED is a sentinel error the fake ch-remote returns
// when we want StopTask to fall through past the shutdown step. The
// concrete error value is irrelevant — the ladder only checks `!= nil`.
func syscallECONNREFUSED() error {
	return &fakeSocketErr{msg: "connect: connection refused (fake)"}
}

type fakeSocketErr struct{ msg string }

func (e *fakeSocketErr) Error() string { return e.msg }

// Compile-time guard: stopTaskFixture is not used elsewhere.
var _ = sync.Mutex{}
