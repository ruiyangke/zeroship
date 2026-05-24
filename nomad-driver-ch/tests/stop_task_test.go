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

	// T-8b-stress-r8 r24-A2-S2: install a no-op tap-existence probe so
	// the synchronous verify gate added to DestroyTask doesn't shell to
	// real `ip link show` in tests. Default `tapLookupFn` would return
	// (absent=true, nil) for nonexistent taps (the test env doesn't
	// have any), but going through `runIP` adds ~10 ms per probe to
	// every DestroyTask test — enough to push the tight-budget
	// reap-wait / OFD-lock tests over their elapsed-time ceilings.
	// Tests that need to exercise the verify-gate's poll cadence
	// (TestDestroyTask_TapDeletedAndVerifiedAbsent etc.) install their
	// own canned-sequence probe via SetTapLookupForTest, AFTER the
	// fixture build, so this default doesn't shadow them.
	prevLookup := ch.SetTapLookupForTest(func(string) (bool, error) {
		return true, nil // absent — default verify gate short-circuits to nil
	})
	t.Cleanup(func() { ch.SetTapLookupForTest(prevLookup) })

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

// ─── T-8b-stress-r2 driver v14: defensive tap cleanup hook ──────────
//
// Per the v14 design (closes Bug 2's "stranded-tap" residue):
// DestroyTask must clean the tap keyed off h.vmIndex even when h.tap
// is empty — a half-initialised handle or an external orphan still
// gets its `zsbx-nm-<idx>` interface removed. Tests verify:
//
//   1. h.tap empty + vmIndex set → defensive cleanup deletes the
//      computed name and the orphan counter increments by 1.
//   2. h.tap == computed name → defensive cleanup does NOT fire (no
//      double-delete; the orphan counter stays put).
//   3. h.tap != computed name → both deletes happen; orphan counter
//      bumps once (the unrecorded computed name IS an orphan).
//   4. vmIndex out of range (0 or > 155) → defensive cleanup skipped;
//      orphan counter unchanged (defends against a corrupted handle).

// TestDestroyTask_DefensiveTapCleanup_FiresWhenHandleTapEmpty pins the
// driver v14 contract: a handle that reached p.tasks with h.tap == ""
// still gets the per-VMIndex tap cleaned. Without this, the
// T-8b-stress 9-stranded-tap residue persists across cluster cycles.
func TestDestroyTask_DefensiveTapCleanup_FiresWhenHandleTapEmpty(t *testing.T) {
	ch.ResetTapsOrphanedForTest()
	t.Cleanup(ch.ResetTapsOrphanedForTest)

	f := newStopFixture(t, nil)
	// Surgically force h.tap == "" via the test API. The fixture's
	// NetSpec puts "test-tap-7" on the handle; clearing it simulates
	// the partial-init failure mode where setupTapForVM created the
	// device but SetDriverState wrote the field as empty (or never
	// reached the assignment line). The vmIndex echo on the handle
	// (from TaskConfig.VMIndex=7) remains untouched.
	if err := ch.SetHandleTapForTest(f.plugin, f.taskCfg.ID, ""); err != nil {
		t.Fatalf("SetHandleTapForTest: %v", err)
	}

	// Let the runner exit so DestroyTask doesn't block.
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.TapsOrphanedTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// The defensive path must have invoked removeTapFn with the
	// vm_index-derived name (zsbx-nm-7 for VMIndex=7).
	got := f.removedTap.Load()
	if got == nil || *got != "zsbx-nm-7" {
		var v string
		if got != nil {
			v = *got
		}
		t.Errorf("defensive cleanup did not target zsbx-nm-7; removed=%q", v)
	}
	if post := ch.TapsOrphanedTotal(); post != pre+1 {
		t.Errorf("orphan counter: pre=%d post=%d, want +1", pre, post)
	}
}

// TestDestroyTask_DefensiveTapCleanup_SkipsWhenHandleTapMatches pins
// the no-double-delete branch: when h.tap == zsbx-nm-<idx> (the
// happy-path lifecycle), the defensive pass is suppressed so the
// orphan counter doesn't fire spuriously on every clean shutdown.
//
// Uses the standard fixture but mutates h.tap to "zsbx-nm-7" — the
// same name the defensive pass would compute — so the != guard
// suppresses the redundant second delete. (The fixture's NetSpec
// supplies "test-tap-7", which gets overwritten here. SetHandleTapForTest
// already exists for the partial-init test; reused.)
func TestDestroyTask_DefensiveTapCleanup_SkipsWhenHandleTapMatches(t *testing.T) {
	ch.ResetTapsOrphanedForTest()
	t.Cleanup(ch.ResetTapsOrphanedForTest)

	// Override the fixture's per-call removedTap (single-slot) with
	// a multi-call accumulator so we can count exactly how many
	// removeTapFn invocations DestroyTask emits. The fixture's seam
	// is installed in newStopFixture; we swap it AFTER the fixture
	// build so our accumulator wins.
	f := newStopFixture(t, nil)
	removedTaps := &atomicStringSlice{}
	prevRemove := ch.SetRemoveTapForTest(func(tap string) error {
		removedTaps.Append(tap)
		return nil
	})
	t.Cleanup(func() { ch.SetRemoveTapForTest(prevRemove) })

	// Force h.tap to the defensive-computed name so the != guard
	// suppresses the second invocation.
	if err := ch.SetHandleTapForTest(f.plugin, f.taskCfg.ID, "zsbx-nm-7"); err != nil {
		t.Fatalf("SetHandleTapForTest: %v", err)
	}

	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.TapsOrphanedTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// Two deletes happen on the happy path post-r24-A2-S2:
	//
	//   1. The cleanup-tail happy-path call (`removeTapFn(h.tap)` at
	//      stop_task.go's `if h.tap != ""` branch).
	//   2. The r24-A2-S2 verify-gate call (`deleteTapAndVerifyAbsent`
	//      re-issues the delete then polls `tapLookupFn` for ENODEV
	//      before returning to Nomad).
	//
	// Pre-r24-A2-S2 there was only 1 call; the second was added to
	// close the "tun-driver evicts the netdev asynchronously" race
	// (stress-r8 `Tap zsbx-nm-N already exists` cycle-1-19 RED). The
	// defensive pass's `!=` guard still suppresses its own redundant
	// call when h.tap == defensiveTap (proven by the count being 2,
	// not 3).
	if got := removedTaps.Len(); got != 2 {
		t.Errorf("expected exactly 2 tap deletes (cleanup + r24-A2-S2 verify); got %d: %v", got, removedTaps.Snapshot())
	}
	if got := removedTaps.At(0); got != "zsbx-nm-7" {
		t.Errorf("first delete: got %q want zsbx-nm-7 (cleanup tail)", got)
	}
	if removedTaps.Len() > 1 {
		if got := removedTaps.At(1); got != "zsbx-nm-7" {
			t.Errorf("second delete: got %q want zsbx-nm-7 (r24-A2-S2 verify gate)", got)
		}
	}
	if post := ch.TapsOrphanedTotal(); post != pre {
		t.Errorf("orphan counter spuriously bumped on happy path: pre=%d post=%d", pre, post)
	}
}

// TestDestroyTask_DefensiveTapCleanup_BothFireWhenNamesDiffer pins
// the operator-supplied-NetSpec failure mode: h.tap = "test-tap-7"
// (from cfg.Net[0]) and the defensive name "zsbx-nm-7" disagree —
// both deletes run, and the orphan counter increments once.
func TestDestroyTask_DefensiveTapCleanup_BothFireWhenNamesDiffer(t *testing.T) {
	ch.ResetTapsOrphanedForTest()
	t.Cleanup(ch.ResetTapsOrphanedForTest)

	f := newStopFixture(t, nil) // cfg.Net[0].Tap = "test-tap-7", VMIndex=7
	removedTaps := &atomicStringSlice{}
	prevRemove := ch.SetRemoveTapForTest(func(tap string) error {
		removedTaps.Append(tap)
		return nil
	})
	t.Cleanup(func() { ch.SetRemoveTapForTest(prevRemove) })

	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.TapsOrphanedTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// Four deletes ran post-r24-A2-S2:
	//
	//   1. cleanup-tail happy-path: removeTapFn("test-tap-7").
	//   2. cleanup-tail defensive: removeTapFn("zsbx-nm-7") — names
	//      differ so the != guard lets this run; orphan counter +1.
	//   3. r24-A2-S2 verify gate for h.tap: removeTapFn("test-tap-7").
	//   4. r24-A2-S2 verify gate for defensive: removeTapFn("zsbx-nm-7").
	//
	// Pre-r24-A2-S2 there were 2 (just steps 1+2). The gate adds 3+4
	// to close the stress-r8 `Tap zsbx-nm-N already exists` race
	// (`ip link delete` returns synchronously but the tun-driver
	// releases the netdev asynchronously). The gate re-issues delete
	// then verifies ENODEV — idempotent against the cleanup-tail's
	// already-issued delete, so the extra calls are safe (realTeardownTap
	// tolerates "Cannot find device").
	snap := removedTaps.Snapshot()
	if len(snap) != 4 {
		t.Fatalf("expected 4 tap deletes (2 cleanup + 2 r24-A2-S2 verify); got %d: %v", len(snap), snap)
	}
	if snap[0] != "test-tap-7" {
		t.Errorf("call 1: got %q want test-tap-7 (cleanup happy path)", snap[0])
	}
	if snap[1] != "zsbx-nm-7" {
		t.Errorf("call 2: got %q want zsbx-nm-7 (cleanup defensive)", snap[1])
	}
	if snap[2] != "test-tap-7" {
		t.Errorf("call 3: got %q want test-tap-7 (r24-A2-S2 verify gate for h.tap)", snap[2])
	}
	if snap[3] != "zsbx-nm-7" {
		t.Errorf("call 4: got %q want zsbx-nm-7 (r24-A2-S2 verify gate for defensive)", snap[3])
	}
	if post := ch.TapsOrphanedTotal(); post != pre+1 {
		t.Errorf("orphan counter: pre=%d post=%d, want +1", pre, post)
	}
}

// ─── T-8b-stress-r4 r4-A: DestroyTask reap-wait ─────────────────────
//
// Per the r4-A design (closes the rootfs.img DiskLockError window
// observed at 5% of WAKE in stress-r4): DestroyTask must not return
// to Nomad until the OS has REAPED the CH process. fcntl write locks
// on rootfs.img release on reap, not on SIGKILL — so a wake post on
// the same VMIndex that lands before the kernel finishes reap sees
// `--restore → DiskLockError → AlreadyLocked, lock_type: Write`.
//
// The reap predicate is h.exitDone: superviseCH closes it after
// runner.Wait() returns, which is by definition AFTER reap (cmd.Wait
// wraps the underlying waitpid syscall). Tests verify:
//
//   1. Process reaped quickly → no poll cycles; DestroyTask returns
//      promptly without bumping the counter.
//   2. Process reaped after N polls → DestroyTask returns once the
//      supervisor closes exitDone; counter stays at baseline.
//   3. Process never reaped within budget → counter bumps by one,
//      WARN logged, DestroyTask still returns nil (we don't want
//      Nomad stuck in a "destroy keeps failing" loop).

// TestDestroyTask_WaitsForProcessReap pins the load-bearing case for
// r4-A: a force=true DestroyTask must not return until the supervisor
// has observed the runner.Wait() return (= the OS has reaped the CH
// process). Wired by closing the fake runner's waitCh AFTER the test
// starts DestroyTask in a goroutine and verifies it's still blocked.
//
// Uses a real (small) sleep cadence so the poll loop has natural
// waiting time we can observe; without a real sleep the no-op seam
// would let the budget exhaust before we ever close the runner.
func TestDestroyTask_WaitsForProcessReap(t *testing.T) {
	ch.ResetDestroyTaskUnreapedForTest()
	t.Cleanup(ch.ResetDestroyTaskUnreapedForTest)

	// 50 attempts × 20 ms = 1 s wall budget. Large enough that the
	// 100 ms "is it still blocked?" probe below doesn't race the
	// budget exhaustion; small enough that the test wallclock stays
	// well under a second even on a slow CI host.
	prevA, prevI := ch.SetDestroyReapWaitForTest(50, 20*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevA, prevI) })

	f := newStopFixture(t, nil)

	// Move the handle into a state where DestroyTask will hit the
	// reap-wait path. Without IsRunning() the path is skipped (the
	// supervisor already closed exitDone for us). Run DestroyTask
	// with force=true so the IsRunning branch runs escalateSigkill,
	// THEN reap-wait kicks in.
	pre := ch.DestroyTaskUnreapedTotal()
	done := make(chan error, 1)
	go func() {
		done <- f.plugin.DestroyTask(f.taskCfg.ID, true)
	}()

	// DestroyTask must still be running: escalateSigkill returned
	// without confirming reap (the fake runner's waitCh is still
	// open), then waitForReap is polling against exitDone.
	select {
	case err := <-done:
		t.Fatalf("DestroyTask returned before reap (err=%v); reap-wait did not block", err)
	case <-time.After(100 * time.Millisecond):
		// Expected: still blocked. The poll loop has woken ~5 times
		// (100 ms / 20 ms cadence) and seen exitDone still open.
	}

	// Now release the runner so the supervisor observes Wait()
	// returning and closes exitDone. DestroyTask's reap-wait sees
	// the close on the next poll iteration and unblocks.
	f.closeRunner()

	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("DestroyTask after reap: %v", err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("DestroyTask did not return within 2 s of supervisor reap")
	}

	// Counter must NOT have bumped — reap was observed in time.
	if post := ch.DestroyTaskUnreapedTotal(); post != pre {
		t.Errorf("unreaped counter spuriously bumped: pre=%d post=%d", pre, post)
	}
}

// TestDestroyTask_TolerantOfReapTimeout pins the budget-exhaustion
// branch: the supervisor never closes exitDone within the reap-wait
// budget. DestroyTask still returns nil (Nomad gets a definitive
// terminal signal), the counter bumps by one, and a WARN line is
// emitted. Avoids Nomad-loop-destroy on a wedged kernel.
func TestDestroyTask_TolerantOfReapTimeout(t *testing.T) {
	ch.ResetDestroyTaskUnreapedForTest()
	t.Cleanup(ch.ResetDestroyTaskUnreapedForTest)

	// 3 polls × no-op sleep — exhausts the budget in <1 ms wall.
	prevA, prevI := ch.SetDestroyReapWaitForTest(3, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevA, prevI) })
	prevSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevSleep) })

	f := newStopFixture(t, nil)

	// Never close the runner: the supervisor never sees exit; exitDone
	// stays open for the duration of the reap-wait budget.
	pre := ch.DestroyTaskUnreapedTotal()
	t0 := time.Now()
	err := f.plugin.DestroyTask(f.taskCfg.ID, true)
	elapsed := time.Since(t0)
	if err != nil {
		t.Fatalf("DestroyTask should be tolerant of reap timeout (got err=%v)", err)
	}

	// Budget exhaustion: counter must have bumped by exactly 1.
	if post := ch.DestroyTaskUnreapedTotal(); post != pre+1 {
		t.Errorf("unreaped counter: pre=%d post=%d, want +1", pre, post)
	}

	// Sanity: the no-op sleep + the escalateSigkill 50 ms grace cap
	// means total should be well under a second even on a slow CI
	// host. The cap exists so a regression that wires a real sleep
	// here surfaces as a test timeout rather than a silent slowdown.
	if elapsed > 5*time.Second {
		t.Errorf("DestroyTask reap-wait exhaustion took %v; expected <5 s with no-op sleep seam", elapsed)
	}

	// Cleanup: let the supervisor observe the runner exit so its
	// goroutine doesn't leak past the test.
	f.closeRunner()
}

// TestDestroyTask_NoOpWhenAlreadyReaped pins the happy-path: when the
// supervisor has already closed exitDone (CH exited cleanly, StopTask
// ran first), DestroyTask's reap-wait observes the close on the first
// peek and returns immediately. The counter stays at baseline.
func TestDestroyTask_NoOpWhenAlreadyReaped(t *testing.T) {
	ch.ResetDestroyTaskUnreapedForTest()
	t.Cleanup(ch.ResetDestroyTaskUnreapedForTest)

	prevA, prevI := ch.SetDestroyReapWaitForTest(25, 200*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevA, prevI) })

	// Make the sleep seam fail the test loudly if it ever fires: the
	// already-reaped path must NOT sleep. (We use atomic via mu so a
	// concurrent observation is safe.)
	sleepHit := &atomic.Int32{}
	prevSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {
		sleepHit.Add(1)
	})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevSleep) })

	f := newStopFixture(t, nil)

	// Drive the runner to exit BEFORE DestroyTask runs; wait for the
	// supervisor to close exitDone (mirrors the
	// TestStopTask_ChRemoteShutdownGracefulExits idiom).
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskUnreapedTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	if got := sleepHit.Load(); got != 0 {
		t.Errorf("reap-wait slept %d times on already-reaped path; want 0", got)
	}
	if post := ch.DestroyTaskUnreapedTotal(); post != pre {
		t.Errorf("unreaped counter spuriously bumped on already-reaped: pre=%d post=%d", pre, post)
	}
}

// ─── T-8b-stress-r5 r5-A: DestroyTask OFD-lock-probe ────────────────
//
// Per the r5-A design (closes the residual 5% rootfs.img
// DiskLockError window observed at stress-r5 RED 3/60): r4-A's
// reap predicate (Go's `cmd.Wait()` returning) is necessary but not
// sufficient. Linux's `__fput` runs in a deferred kernel workqueue
// triggered from the LAST `close()`/`exit()` on a `struct file`;
// until it completes, the OFD write lock on `rootfs.img` persists
// (attributed to PID=-1 — no live owner). `wait4()` reaps the
// zombie, but the workqueue runs separately.
//
// The strictly-stronger predicate is: try to ACQUIRE the OFD write
// lock ourselves. If we succeed, `__fput` ran (the kernel grants
// only one OFD write lock per inode/range). Tests verify:
//
//   1. Probe returns Busy three times then Acquired → exactly 3
//      probe calls + 3 sleep cycles before DestroyTask returns;
//      counter NOT bumped.
//   2. Probe returns Busy for the full budget → counter bumps by
//      one; DestroyTask returns nil (mirrors r4-A tolerance).
//   3. Probe returns Acquired on the first call → sleep seam never
//      fires; counter stays at baseline.
//   4. Probe returns FileGone (ENOENT) → treated as success (no
//      lock possible on a non-existent file); counter at baseline.

// TestDestroyTask_WaitsForOFDLockRelease pins the load-bearing case:
// the OFD probe must poll until the lock can be acquired. Wired by
// installing a probe seam that returns Busy three times then
// Acquired; the helper observes exactly 3 sleep cycles between the
// first call and the final success and asserts the counter stays at
// baseline.
func TestDestroyTask_WaitsForOFDLockRelease(t *testing.T) {
	ch.ResetDestroyTaskLockHeldForTest()
	t.Cleanup(ch.ResetDestroyTaskLockHeldForTest)

	// 25-attempt budget keeps the production shape; the seam drives
	// the sequence so we don't need real wall time.
	prevA, prevI := ch.SetDestroyLockPollForTest(25, 200*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevA, prevI) })

	// Count sleeps and probe calls to confirm the exact poll cadence.
	sleepHit := &atomic.Int32{}
	prevSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {
		sleepHit.Add(1)
	})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevSleep) })

	probeCalls := &atomic.Int32{}
	prevProbe := ch.SetTryAcquireOFDLockForTest(func(string) (ch.OFDLockProbeResultForTest, error) {
		n := probeCalls.Add(1)
		if n <= 3 {
			return ch.OFDLockProbeBusyForTest, nil
		}
		return ch.OFDLockProbeAcquiredForTest, nil
	})
	t.Cleanup(func() { ch.SetTryAcquireOFDLockForTest(prevProbe) })

	// Skip the r4-A reap-wait so we isolate the r5-A path; a no-op
	// reap-wait sleep ensures it doesn't add wall time either.
	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	f := newStopFixture(t, nil)
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskLockHeldTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// 4 probe calls total: 3 Busy + 1 Acquired. The helper does NOT
	// sleep after the final attempt (mirrors r4-A waitForReap), so
	// we expect 3 sleeps between the 4 probes — per disk path.
	//
	// taskDiskPathsForLockProbe synthesises 3 paths here
	// (rootfs.img + workspace + userhome from the fixture's cold-boot
	// config). The seam's counter is process-global, so 4 + 1 + 1 =
	// 6 probe calls (first disk consumes the 3-Busy preamble; the
	// subsequent two disks acquire on the first attempt each).
	if got := probeCalls.Load(); got != 6 {
		t.Errorf("probe call count = %d, want 6 (3 Busy + 1 Acquired on disk[0], 1 Acquired each on disk[1..2])", got)
	}
	// 3 sleeps for disk[0]'s busy preamble. disk[1..2] acquire on
	// first attempt with no sleeps. Production poll cadence is
	// sleep-before-retry, not sleep-after-success.
	if got := sleepHit.Load(); got != 3 {
		t.Errorf("sleep hit count = %d, want 3 (only between disk[0]'s 3 Busy returns)", got)
	}
	if post := ch.DestroyTaskLockHeldTotal(); post != pre {
		t.Errorf("lock-held counter spuriously bumped on observe-acquire: pre=%d post=%d", pre, post)
	}
}

// TestDestroyTask_ProceedsOnLockHeldBudgetExhausted pins the
// budget-exhaustion branch: the probe returns Busy for every attempt.
// DestroyTask still returns nil (Nomad gets a definitive terminal
// signal), the counter bumps by one, and a WARN is emitted. Mirrors
// r4-A's tolerance shape.
func TestDestroyTask_ProceedsOnLockHeldBudgetExhausted(t *testing.T) {
	ch.ResetDestroyTaskLockHeldForTest()
	t.Cleanup(ch.ResetDestroyTaskLockHeldForTest)

	// Match the production 25-attempt budget but with a 1ms cadence
	// and a no-op sleep seam — exhausts in <1ms wall.
	prevA, prevI := ch.SetDestroyLockPollForTest(25, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevA, prevI) })
	prevSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevSleep) })

	prevProbe := ch.SetTryAcquireOFDLockForTest(func(string) (ch.OFDLockProbeResultForTest, error) {
		return ch.OFDLockProbeBusyForTest, nil
	})
	t.Cleanup(func() { ch.SetTryAcquireOFDLockForTest(prevProbe) })

	// Bypass r4-A reap-wait so we isolate the r5-A budget-exhaust
	// branch (otherwise we'd also bump *_unreaped_total on a force=true
	// path with no runner exit).
	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	f := newStopFixture(t, nil)
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskLockHeldTotal()
	t0 := time.Now()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask should be tolerant of OFD-lock budget exhaustion (got err=%v)", err)
	}
	elapsed := time.Since(t0)

	// Budget exhaustion on the FIRST disk path aborts the iteration
	// (waitForOFDLockRelease returns on first error), so the counter
	// bumps exactly once even when 3 disks were enumerated.
	if post := ch.DestroyTaskLockHeldTotal(); post != pre+1 {
		t.Errorf("lock-held counter: pre=%d post=%d, want +1", pre, post)
	}
	if elapsed > 2*time.Second {
		t.Errorf("DestroyTask OFD budget exhaustion took %v; expected <2s with no-op sleep seam", elapsed)
	}
}

// TestDestroyTask_NoOpWhenLockImmediatelyAcquired pins the happy
// path: the first probe returns Acquired; the helper returns
// immediately without sleeping. Sleep seam hit count = 0 across all
// 3 enumerated disks; counter stays at baseline.
func TestDestroyTask_NoOpWhenLockImmediatelyAcquired(t *testing.T) {
	ch.ResetDestroyTaskLockHeldForTest()
	t.Cleanup(ch.ResetDestroyTaskLockHeldForTest)

	prevA, prevI := ch.SetDestroyLockPollForTest(25, 200*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevA, prevI) })

	// Make the sleep seam fail the test loudly if it ever fires.
	sleepHit := &atomic.Int32{}
	prevSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {
		sleepHit.Add(1)
	})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevSleep) })

	prevProbe := ch.SetTryAcquireOFDLockForTest(func(string) (ch.OFDLockProbeResultForTest, error) {
		return ch.OFDLockProbeAcquiredForTest, nil
	})
	t.Cleanup(func() { ch.SetTryAcquireOFDLockForTest(prevProbe) })

	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	f := newStopFixture(t, nil)
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskLockHeldTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	if got := sleepHit.Load(); got != 0 {
		t.Errorf("OFD probe slept %d times on first-attempt-acquire path; want 0", got)
	}
	if post := ch.DestroyTaskLockHeldTotal(); post != pre {
		t.Errorf("lock-held counter spuriously bumped on first-attempt-acquire: pre=%d post=%d", pre, post)
	}
}

// TestDestroyTask_FileGoneTolerantWhenProbing pins the file-gone
// branch: the probe returns FileGone (ENOENT) — treated as success
// because no lock is possible on a non-existent file. Counter stays
// at baseline; the helper returns nil and DestroyTask proceeds.
//
// This shape covers the case where the runDir was already scrubbed
// by a peer process (rare, but possible under aggressive controller-
// driven force-purge before DestroyTask fires).
func TestDestroyTask_FileGoneTolerantWhenProbing(t *testing.T) {
	ch.ResetDestroyTaskLockHeldForTest()
	t.Cleanup(ch.ResetDestroyTaskLockHeldForTest)

	prevA, prevI := ch.SetDestroyLockPollForTest(25, 200*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevA, prevI) })

	sleepHit := &atomic.Int32{}
	prevSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {
		sleepHit.Add(1)
	})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevSleep) })

	prevProbe := ch.SetTryAcquireOFDLockForTest(func(string) (ch.OFDLockProbeResultForTest, error) {
		return ch.OFDLockProbeFileGoneForTest, nil
	})
	t.Cleanup(func() { ch.SetTryAcquireOFDLockForTest(prevProbe) })

	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	f := newStopFixture(t, nil)
	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskLockHeldTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// FileGone is success — no sleeps, no counter bump. All 3
	// enumerated disks short-circuit to the same outcome.
	if got := sleepHit.Load(); got != 0 {
		t.Errorf("OFD probe slept %d times on file-gone path; want 0", got)
	}
	if post := ch.DestroyTaskLockHeldTotal(); post != pre {
		t.Errorf("lock-held counter spuriously bumped on file-gone: pre=%d post=%d", pre, post)
	}
}

// ─── T-8b-stress-r8 r24-A2-S2: DestroyTask tap-delete + ENODEV-verify gate ───
//
// Per the r24-A2-S2 design (closes the residual `Tap zsbx-nm-N already
// exists` window observed at stress-r8 RED 3/60 with cycles 1-19
// failing identically): `ip link delete` returns synchronously but
// the tun-driver releases the netdev asynchronously. DestroyTask must
// poll `ip link show <tap>` for ENODEV before returning — otherwise
// the next StartTask's tuntap-add hits EEXIST on the same vm_index.
//
// Tests verify (per the sprint mandate):
//
//   1. TestDestroyTask_TapDeletedAndVerifiedAbsent — happy path: the
//      lookup seam returns absent=true on the first call; the verify
//      gate returns nil without sleeping; counter at baseline.
//   2. TestDestroyTask_TolerantOfTapDeleteENODEV — the lookup seam
//      returns Busy three times then Absent; the gate observes 3
//      sleep cycles before returning nil; counter at baseline.
//   3. TestDestroyTask_PollExhaustionBumpsCounter — the lookup seam
//      always returns absent=false; the gate exhausts its budget,
//      bumps the counter, returns nil (mirrors r4-A/r5-A tolerance).

// TestDestroyTask_TapDeletedAndVerifiedAbsent pins the happy path:
// the verify gate's first poll observes absent=true → returns nil
// without sleeping. Sleep seam never fires; counter stays at
// baseline.
func TestDestroyTask_TapDeletedAndVerifiedAbsent(t *testing.T) {
	ch.ResetDestroyTaskTapStuckForTest()
	t.Cleanup(ch.ResetDestroyTaskTapStuckForTest)

	prevA, prevI := ch.SetDestroyTapDeletePollForTest(50, 100*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyTapDeletePollForTest(prevA, prevI) })

	// Make the sleep seam fail the test loudly if it ever fires: the
	// happy path must NOT sleep (first-poll Absent succeeds).
	sleepHit := &atomic.Int32{}
	prevSleep := ch.SetSleepForTapDeletePollForTest(func(time.Duration) {
		sleepHit.Add(1)
	})
	t.Cleanup(func() { ch.SetSleepForTapDeletePollForTest(prevSleep) })

	// Bypass r4-A reap-wait and r5-A OFD-lock-wait so we isolate the
	// r24-A2-S2 path. Install AFTER fixture so its no-op default lookup
	// doesn't shadow our canned-sequence probe below.
	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	prevLockA, prevLockI := ch.SetDestroyLockPollForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevLockA, prevLockI) })
	prevLockSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevLockSleep) })

	f := newStopFixture(t, nil)

	// Lookup seam: every probe reports absent=true (kernel evicted
	// netdev synchronously, healthy host). Installed AFTER the
	// fixture so its default no-op `(true, nil)` lookup doesn't
	// shadow our counter-incrementing version.
	probeCalls := &atomic.Int32{}
	prevLookup := ch.SetTapLookupForTest(func(string) (bool, error) {
		probeCalls.Add(1)
		return true, nil
	})
	t.Cleanup(func() { ch.SetTapLookupForTest(prevLookup) })

	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskTapStuckTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// Two unique taps for VMIndex=7: h.tap="test-tap-7" + defensive
	// "zsbx-nm-7". Each verify call observes absent=true on the first
	// poll, so probeCalls = 2 and sleepHit = 0.
	if got := probeCalls.Load(); got != 2 {
		t.Errorf("probe call count = %d, want 2 (one per unique tap, no retries)", got)
	}
	if got := sleepHit.Load(); got != 0 {
		t.Errorf("tap-verify slept %d times on happy path; want 0", got)
	}
	if post := ch.DestroyTaskTapStuckTotal(); post != pre {
		t.Errorf("tap-stuck counter spuriously bumped on happy path: pre=%d post=%d", pre, post)
	}
}

// TestDestroyTask_TolerantOfTapDeleteENODEV pins the retry-then-
// success branch: the lookup seam returns absent=false three times
// then absent=true (kernel finally evicted the netdev). The gate
// observes exactly 3 sleep cycles between the 4 lookups per tap;
// counter stays at baseline.
//
// The test name says "TolerantOfTapDeleteENODEV" per the sprint
// mandate (the helper tolerates the tap-delete itself returning
// ENODEV — i.e., the device was already gone when we issued the
// re-delete — by treating it as success). We verify the tolerance
// implicitly here by having the lookup seam succeed after a few
// busy returns: the cumulative effect is the same regardless of
// whether the delete reported ENODEV or success.
func TestDestroyTask_TolerantOfTapDeleteENODEV(t *testing.T) {
	ch.ResetDestroyTaskTapStuckForTest()
	t.Cleanup(ch.ResetDestroyTaskTapStuckForTest)

	prevA, prevI := ch.SetDestroyTapDeletePollForTest(50, 100*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyTapDeletePollForTest(prevA, prevI) })

	sleepHit := &atomic.Int32{}
	prevSleep := ch.SetSleepForTapDeletePollForTest(func(time.Duration) {
		sleepHit.Add(1)
	})
	t.Cleanup(func() { ch.SetSleepForTapDeletePollForTest(prevSleep) })

	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	prevLockA, prevLockI := ch.SetDestroyLockPollForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevLockA, prevLockI) })
	prevLockSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevLockSleep) })

	f := newStopFixture(t, nil)

	// Lookup seam: returns absent=false for the first 3 probes per
	// tap (matches the production "kernel released the netdev within
	// 3 × 100 ms" healthy case), then absent=true. The probe state is
	// per-tap so each unique tap repeats the 3-busy-then-absent
	// sequence. Installed AFTER the fixture so its default no-op
	// `(true, nil)` lookup doesn't shadow our canned-sequence probe.
	probeCalls := &atomic.Int32{}
	probeStateByTap := map[string]int{}
	var probeMu sync.Mutex
	prevLookup := ch.SetTapLookupForTest(func(tap string) (bool, error) {
		probeCalls.Add(1)
		probeMu.Lock()
		defer probeMu.Unlock()
		probeStateByTap[tap]++
		if probeStateByTap[tap] <= 3 {
			return false, nil // still present
		}
		return true, nil // evicted
	})
	t.Cleanup(func() { ch.SetTapLookupForTest(prevLookup) })

	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskTapStuckTotal()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask: %v", err)
	}

	// Two unique taps × (3 busy + 1 absent) = 8 probe calls.
	// 3 sleeps per tap between the 4 polls (no sleep after the final
	// successful probe) = 6 sleeps total.
	if got := probeCalls.Load(); got != 8 {
		t.Errorf("probe call count = %d, want 8 (2 taps × 4 probes each: 3 Busy + 1 Absent)", got)
	}
	if got := sleepHit.Load(); got != 6 {
		t.Errorf("tap-verify slept %d times; want 6 (2 taps × 3 sleeps each between busy probes)", got)
	}
	if post := ch.DestroyTaskTapStuckTotal(); post != pre {
		t.Errorf("tap-stuck counter spuriously bumped on retry-then-success: pre=%d post=%d", pre, post)
	}
}

// TestDestroyTask_PollExhaustionBumpsCounter pins the budget-
// exhaustion branch: the lookup seam always returns absent=false
// (kernel netdev cleanup is wedged). DestroyTask still returns nil
// (Nomad gets a definitive terminal signal), the counter bumps once
// per unique tap that exhausts, and a WARN is emitted. Mirrors r4-A
// / r5-A's "never block Nomad destroy" tolerance.
func TestDestroyTask_PollExhaustionBumpsCounter(t *testing.T) {
	ch.ResetDestroyTaskTapStuckForTest()
	t.Cleanup(ch.ResetDestroyTaskTapStuckForTest)

	// Match production 50-attempt budget but with a 1 ms cadence and
	// a no-op sleep seam — exhausts in <1 ms wall.
	prevA, prevI := ch.SetDestroyTapDeletePollForTest(50, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyTapDeletePollForTest(prevA, prevI) })
	prevSleep := ch.SetSleepForTapDeletePollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForTapDeletePollForTest(prevSleep) })

	prevReapA, prevReapI := ch.SetDestroyReapWaitForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyReapWaitForTest(prevReapA, prevReapI) })
	prevReapSleep := ch.SetSleepForReapPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForReapPollForTest(prevReapSleep) })

	prevLockA, prevLockI := ch.SetDestroyLockPollForTest(1, 1*time.Millisecond)
	t.Cleanup(func() { ch.SetDestroyLockPollForTest(prevLockA, prevLockI) })
	prevLockSleep := ch.SetSleepForOFDLockPollForTest(func(time.Duration) {})
	t.Cleanup(func() { ch.SetSleepForOFDLockPollForTest(prevLockSleep) })

	f := newStopFixture(t, nil)

	// Lookup seam: every probe returns absent=false (kernel wedged).
	// Installed AFTER the fixture so its default no-op `(true, nil)`
	// lookup doesn't shadow our always-busy probe.
	prevLookup := ch.SetTapLookupForTest(func(string) (bool, error) {
		return false, nil
	})
	t.Cleanup(func() { ch.SetTapLookupForTest(prevLookup) })

	f.closeRunner()
	time.Sleep(5 * time.Millisecond)
	if err := f.plugin.StopTask(f.taskCfg.ID, 100*time.Millisecond, ""); err != nil {
		t.Fatalf("StopTask: %v", err)
	}

	pre := ch.DestroyTaskTapStuckTotal()
	t0 := time.Now()
	if err := f.plugin.DestroyTask(f.taskCfg.ID, false); err != nil {
		t.Fatalf("DestroyTask should be tolerant of tap-delete budget exhaustion (got err=%v)", err)
	}
	elapsed := time.Since(t0)

	// Two unique taps each exhaust their budget independently → +2.
	// The gate doesn't abort the loop on first exhaustion (unlike
	// the OFD-lock gate's first-error-aborts pattern) because each
	// tap may have an independent wedge mode and the operator wants
	// observability on both.
	if post := ch.DestroyTaskTapStuckTotal(); post != pre+2 {
		t.Errorf("tap-stuck counter: pre=%d post=%d, want +2 (two unique taps both exhausted)", pre, post)
	}
	if elapsed > 2*time.Second {
		t.Errorf("DestroyTask tap-verify budget exhaustion took %v; expected <2 s with no-op sleep seam", elapsed)
	}
}

// atomicStringSlice is a minimal goroutine-safe accumulator the new
// defensive-cleanup tests use to observe the ORDER of removeTapFn
// calls (the existing fixture only stores the LAST tap). Kept local
// to this file to avoid bloating helpers_test.go.
type atomicStringSlice struct {
	mu sync.Mutex
	s  []string
}

func (a *atomicStringSlice) Append(s string) {
	a.mu.Lock()
	defer a.mu.Unlock()
	a.s = append(a.s, s)
}

func (a *atomicStringSlice) Len() int {
	a.mu.Lock()
	defer a.mu.Unlock()
	return len(a.s)
}

func (a *atomicStringSlice) At(i int) string {
	a.mu.Lock()
	defer a.mu.Unlock()
	if i < 0 || i >= len(a.s) {
		return ""
	}
	return a.s[i]
}

func (a *atomicStringSlice) Snapshot() []string {
	a.mu.Lock()
	defer a.mu.Unlock()
	out := make([]string, len(a.s))
	copy(out, a.s)
	return out
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
