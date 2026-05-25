// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::StopTask &
// DestroyTask) on 2026-05-25 for Cloud Hypervisor support. The libvirt
// StopVM/DestroyVM path is replaced by the CH-specific ladder described in
// proposal § 7 ("StopTask" + "DestroyTask").
//
// T-2 implementation. The ladder mirrors the bash wrapper's cleanup trap
// (crates/sandbox/scripts/nomad-vm-wrapper.sh § "Cleanup trap"):
//
//  1. ch-remote shutdown-vmm (graceful, up to shutdownTimeout)
//  2. SIGTERM to the CH pid + wait sigtermTimeout
//  3. SIGKILL
//  4. Cleanup: remove the tap device, remove the API socket file
//
// StopTask honours the caller-supplied signal and timeout. DestroyTask runs
// the cleanup tail regardless of whether the CH process is still alive.

package ch

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
	"golang.org/x/sys/unix"
)

// Default per-step timeouts for the StopTask ladder. The brief calls for
// 5 s for ch-remote shutdown and 3 s for SIGTERM. They are exported as
// package-level vars (not consts) so tests can override them to keep the
// suite under 100 ms per case — see SetStopTimeoutsForTest.
var (
	// defaultShutdownTimeout is how long we wait for the CH process to
	// exit after issuing `ch-remote shutdown-vmm` before falling through
	// to SIGTERM. 5 s matches the bash wrapper's `sleep 0.2` × 25 idea
	// generously; CH shuts down in <500 ms on a healthy VM.
	defaultShutdownTimeout = 5 * time.Second

	// defaultSigtermTimeout is how long we wait for the CH process to
	// exit after SIGTERM before escalating to SIGKILL. 3 s matches the
	// raw_exec driver's default kill_timeout headroom.
	defaultSigtermTimeout = 3 * time.Second

	// stopTimeoutsMu guards the two defaults above so tests can flip them
	// without racing the production ladder. The Lock is held only for
	// the read at the top of StopTask, so contention is nil in practice.
	stopTimeoutsMu = struct {
		shutdownTimeout time.Duration
		sigtermTimeout  time.Duration
	}{
		shutdownTimeout: defaultShutdownTimeout,
		sigtermTimeout:  defaultSigtermTimeout,
	}

	// T-8b-stress-r4 r4-A: DestroyTask reap-wait budget. The OS releases
	// fcntl file locks (CH's rootfs.img ExclusiveWrite among them) when
	// the process is REAPED, not when it's SIGKILL'd. Between SIGKILL and
	// reap the kernel still attributes the locks to the now-zombie task,
	// so the next alloc's CH `--restore` on the same rootfs.img sees
	// `DiskLockError → AlreadyLocked` until init/runner reaps.
	//
	// superviseCH owns the single runner.Wait() call (production) /
	// processAlive poll loop (recovered tasks) and closes h.exitDone
	// AFTER Wait returns — by definition AFTER reap. So a bounded wait on
	// h.exitDone is the reap predicate.
	//
	// Budget: 5 s wall at 200 ms cadence (25 attempts). Generous against
	// the observed-in-prod gap (sub-second on a healthy host) but short
	// enough that a stuck reap surfaces in operator logs within one
	// stress-loop iteration. Mirrors the v15 r3-B waitForTapAbsent
	// pattern (poll on a single observable kernel state transition).
	destroyReapPollAttempts = 25
	destroyReapPollInterval = 200 * time.Millisecond

	// T-8b-stress-r5 r5-A: DestroyTask OFD-lock-probe budget. r4-A's
	// reap-wait predicate (Go's `cmd.Wait()` returning) is necessary
	// but not sufficient: Linux's `__fput` runs in a deferred kernel
	// workqueue after the last `close()`/`exit()`, so the OFD write
	// lock on `rootfs.img` persists (attributed to PID=-1) until the
	// workqueue runs. `/proc/locks` evidence at stress-r5 RED 3/60
	// (5%) showed `OFDLCK WRITE 0 fd:01:<inode> 0 EOF` with
	// owner=-1 — the strictly-stronger predicate is to try to
	// acquire the OFD write lock ourselves; if we succeed, `__fput`
	// has run (the kernel grants only one OFD write lock per
	// inode/range).
	//
	// Budget: 5 s wall at 200 ms cadence (25 attempts) — same shape
	// as the r4-A reap-wait. `__fput` workqueue latency is sub-ms on
	// a healthy host; the 5 s ceiling is generous against a backed-
	// up workqueue but short enough that a stuck deferred-fput
	// surfaces in operator logs within one stress-loop iteration.
	destroyLockPollAttempts = 25
	destroyLockPollInterval = 200 * time.Millisecond

	// T-8b-stress-r8 r24-A2-S2: DestroyTask tap-deletion-verify budget.
	// Stress-r8 RED 3/60 showed cycles 1-19 failing identically with
	// `Tap zsbx-nm-N already exists`: the kernel hadn't finished
	// evicting the tap netdev between DestroyTask's best-effort
	// removeTapFn call and the next StartTask's tuntap-add.
	// `ip link delete` returns synchronously but the tun-driver
	// releases the netdev asynchronously (same mechanism as the v15
	// r3-B collision-replace race, but on the destroy side this time).
	//
	// The strictly-stronger predicate: after issuing `ip link delete`,
	// poll `ip link show <tap>` until ENODEV before declaring the
	// task terminal. If the netdev is still listed, the next alloc's
	// tuntap-add WILL collide; the existing v13 pre-delete recovers
	// the EEXIST but adds a second tuntap-add cycle that's
	// load-bearing on a healthy kernel. Closing the window here is
	// the better fix.
	//
	// Budget: 5 s wall at 100 ms cadence (50 attempts). Generous
	// against a wedged tun-driver but short enough that a stuck
	// netdev surfaces in operator logs within one stress-loop
	// iteration. Mirrors the r4-A/r5-A 5 s ceiling, with a finer
	// cadence because `ip link show` is a single netlink RPC (not
	// a workqueue wait) so polling more often is cheap.
	destroyTapDeletePollAttempts = 50
	destroyTapDeletePollInterval = 100 * time.Millisecond
)

// SetDestroyReapWaitForTest overrides the reap-wait poll budget so tests
// don't sleep 5 s. Returns the previous (attempts, interval) pair so the
// caller can restore them on cleanup. Mirrors SetStopTimeoutsForTest.
func SetDestroyReapWaitForTest(attempts int, interval time.Duration) (int, time.Duration) {
	prevA := destroyReapPollAttempts
	prevI := destroyReapPollInterval
	destroyReapPollAttempts = attempts
	destroyReapPollInterval = interval
	return prevA, prevI
}

// SetDestroyLockPollForTest overrides the OFD-lock-probe poll budget so
// tests don't sleep 5 s. Returns the previous (attempts, interval) pair
// so the caller can restore them on cleanup. Mirrors SetDestroyReapWaitForTest.
func SetDestroyLockPollForTest(attempts int, interval time.Duration) (int, time.Duration) {
	prevA := destroyLockPollAttempts
	prevI := destroyLockPollInterval
	destroyLockPollAttempts = attempts
	destroyLockPollInterval = interval
	return prevA, prevI
}

// SetDestroyTapDeletePollForTest overrides the tap-deletion-verify poll
// budget so tests don't sleep 5 s. Returns the previous (attempts,
// interval) pair so the caller can restore them on cleanup. Mirrors
// SetDestroyReapWaitForTest / SetDestroyLockPollForTest.
//
// T-8b-stress-r8 r24-A2-S2.
func SetDestroyTapDeletePollForTest(attempts int, interval time.Duration) (int, time.Duration) {
	prevA := destroyTapDeletePollAttempts
	prevI := destroyTapDeletePollInterval
	destroyTapDeletePollAttempts = attempts
	destroyTapDeletePollInterval = interval
	return prevA, prevI
}

// SetStopTimeoutsForTest overrides the per-step ladder timeouts so tests
// don't sleep 5+3 real seconds. Returns the previous (shutdown, sigterm)
// pair so the caller can restore them on cleanup.
//
// Picked parameterised timeouts over a clock seam: the ladder uses two
// `time.After` channels and nothing else, so a clock interface would carry
// more conceptual weight than the two-knob alternative. Matches the
// existing T-1 seam style (ensureTapUpFn, shutdownFn).
func SetStopTimeoutsForTest(shutdown, sigterm time.Duration) (time.Duration, time.Duration) {
	prevS := stopTimeoutsMu.shutdownTimeout
	prevT := stopTimeoutsMu.sigtermTimeout
	stopTimeoutsMu.shutdownTimeout = shutdown
	stopTimeoutsMu.sigtermTimeout = sigterm
	return prevS, prevT
}

// StopTask drives the graceful-stop ladder described at the top of this
// file. Honours Nomad's driver contract:
//
//   - If taskID is unknown, return nil (idempotent — Nomad may double-call
//     after a controller-triggered DestroyTask).
//   - If signal == "SIGKILL", skip steps 1+2 and go straight to step 3.
//   - The cumulative ladder time is bounded by `timeout` plus a small
//     per-step grace; once `timeout` expires, the ladder fast-forwards to
//     the next step.
//
// StopTask does NOT remove on-disk state — DestroyTask owns that. This
// matches the upstream virt driver and the proposal § 7.
func (p *Plugin) StopTask(taskID string, timeout time.Duration, signal string) error {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		p.logger.Warn("ch: StopTask on unknown task; ignoring", "task_id", taskID)
		return nil
	}

	// Snapshot the timeouts under the package-level "lock". This is a
	// plain struct read; the mutability is intentional (tests flip them).
	shutdownTimeout := stopTimeoutsMu.shutdownTimeout
	sigtermTimeout := stopTimeoutsMu.sigtermTimeout

	// Clip per-step waits against the caller-supplied total budget so
	// the ladder doesn't exceed `timeout` by more than one step's grace.
	// `timeout <= 0` means "use defaults" (mirrors raw_exec).
	if timeout > 0 {
		if shutdownTimeout > timeout {
			shutdownTimeout = timeout
		}
		// After step 1 consumes up to shutdownTimeout, step 2 gets the
		// remainder (clamped to sigtermTimeout). If timeout < shutdownTimeout,
		// step 2 may collapse to 0; SIGKILL still fires.
		remaining := timeout - shutdownTimeout
		if remaining < 0 {
			remaining = 0
		}
		if sigtermTimeout > remaining {
			sigtermTimeout = remaining
		}
	}

	// Fast path: caller asked for an immediate SIGKILL. Skip ch-remote
	// and SIGTERM; jump straight to step 3.
	signalUpper := strings.ToUpper(strings.TrimSpace(signal))
	if signalUpper == "SIGKILL" {
		p.logger.Info("ch: StopTask: SIGKILL requested; skipping graceful steps", "task_id", taskID)
		return p.escalateSigkill(h, sigtermTimeout)
	}

	// --- Step 1: ch-remote shutdown-vmm ------------------------------
	p.logger.Info("ch: StopTask: step 1 ch-remote shutdown-vmm",
		"task_id", taskID, "api_socket", h.apiSocket, "timeout", shutdownTimeout)
	shutdownErr := shutdownFn(p.chClient, h.apiSocket)
	if shutdownErr != nil {
		p.logger.Warn("ch: StopTask: ch-remote shutdown-vmm failed; falling through to SIGTERM",
			"task_id", taskID, "err", shutdownErr)
	} else if shutdownTimeout > 0 {
		if p.waitForExit(h, shutdownTimeout) {
			p.logger.Info("ch: StopTask: VM exited gracefully after ch-remote shutdown-vmm",
				"task_id", taskID)
			return nil
		}
		p.logger.Warn("ch: StopTask: VM did not exit within shutdown grace; escalating to SIGTERM",
			"task_id", taskID, "waited", shutdownTimeout)
	}

	// --- Step 2: SIGTERM ---------------------------------------------
	p.logger.Info("ch: StopTask: step 2 SIGTERM", "task_id", taskID, "timeout", sigtermTimeout)
	if err := p.signalHandle(h, syscall.SIGTERM); err != nil {
		// Process already gone is fine — supervisor will see the exit.
		p.logger.Warn("ch: StopTask: SIGTERM send failed (process may already be gone)",
			"task_id", taskID, "err", err)
	}
	if sigtermTimeout > 0 {
		if p.waitForExit(h, sigtermTimeout) {
			p.logger.Info("ch: StopTask: VM exited after SIGTERM", "task_id", taskID)
			return nil
		}
	}

	// --- Step 3: SIGKILL ---------------------------------------------
	// Post-SIGKILL grace reuses sigtermTimeout — it's a configurable
	// knob that already encodes the operator's appetite for waiting on
	// the supervisor goroutine.
	return p.escalateSigkill(h, sigtermTimeout)
}

// escalateSigkill issues SIGKILL and waits up to `grace` for runner.Wait
// to observe the exit. Returns nil even if the supervisor doesn't close
// exitDone within `grace`; the kernel reap is asynchronous and the
// follow-up DestroyTask call (with force=true if needed) handles the
// pathological "kernel can't reap the pid" case.
func (p *Plugin) escalateSigkill(h *taskHandle, grace time.Duration) error {
	p.logger.Info("ch: StopTask: step 3 SIGKILL", "task_id", h.taskConfig.ID, "ch_pid", h.chPid)
	if err := p.signalHandle(h, syscall.SIGKILL); err != nil {
		p.logger.Warn("ch: StopTask: SIGKILL send failed (process may already be gone)",
			"task_id", h.taskConfig.ID, "err", err)
	}
	if grace <= 0 {
		// Defensive minimum so we still see "the supervisor noticed"
		// in the common case the runner exits within a scheduler tick.
		grace = 50 * time.Millisecond
	}
	if !p.waitForExit(h, grace) {
		p.logger.Warn("ch: StopTask: VM still appears alive after SIGKILL; deferring to DestroyTask",
			"task_id", h.taskConfig.ID, "waited", grace)
	}
	return nil
}

// signalHandle forwards a signal to the CH process backing the handle.
// Prefers the runner.Signal path (works against the fake runner in tests);
// falls back to os.FindProcess+Signal when the handle has no runner (e.g.
// a RecoverTask-produced handle in T-4).
func (p *Plugin) signalHandle(h *taskHandle, sig os.Signal) error {
	if h.runner != nil {
		return h.runner.Signal(sig)
	}
	if h.chPid <= 0 {
		return fmt.Errorf("ch: no pid on handle (task_id=%s)", h.taskConfig.ID)
	}
	proc, err := os.FindProcess(h.chPid)
	if err != nil {
		return fmt.Errorf("ch: FindProcess(%d): %w", h.chPid, err)
	}
	return proc.Signal(sig)
}

// sleepForReapPoll is the package-level seam tests swap so the
// DestroyTask reap-wait loop doesn't add real wall time. Default is
// time.Sleep — production callers block on the kernel's reap pending the
// supervisor goroutine consuming runner.Wait()'s return.
//
// Mirrors the v15 r3-B sleepForTapPoll seam shape so the test ergonomics
// are uniform across the two defense-in-depth poll loops.
var sleepForReapPoll = func(d time.Duration) {
	time.Sleep(d)
}

// SetSleepForReapPollForTest swaps the reap-wait sleep seam. Returns the
// previous fn so the caller can restore it on cleanup. Tests typically
// install a no-op so the poll loop spins through its budget instantly
// rather than waiting real wall time.
func SetSleepForReapPollForTest(fn func(d time.Duration)) func(d time.Duration) {
	prev := sleepForReapPoll
	if fn != nil {
		sleepForReapPoll = fn
	}
	return prev
}

// waitForReap blocks up to `destroyReapPollAttempts × destroyReapPollInterval`
// for the supervisor goroutine to close h.exitDone — the post-reap signal.
// Returns true on observed reap, false on budget exhaustion.
//
// On budget exhaustion, bumps `nomad_driver_ch_destroy_task_unreaped_total`
// and WARN-logs; the caller proceeds with cleanup regardless. The kernel
// reap is asynchronous to SIGKILL: between the SIGKILL syscall and the
// `wait()` syscall that reaps the zombie, fcntl file locks remain
// attributed to the dead process. Locks release ONLY when the kernel
// completes `do_exit() → exit_files() → fput()` and the parent (or init)
// reaps the zombie via wait/waitpid. The exit_files() path drops FDs
// during exit, but the fcntl-lock release window in stress tests has
// shown to be on the SIGKILL-to-reap interval — keep the bounded wait
// keyed to reap as the strongest guarantee.
//
// If h.exitDone is nil (legacy/test handles), returns true immediately:
// no supervisor → no reap to wait on → no harm.
func (p *Plugin) waitForReap(h *taskHandle, taskID string) bool {
	if h.exitDone == nil {
		return true
	}
	attempts := destroyReapPollAttempts
	interval := destroyReapPollInterval
	for i := 0; i < attempts; i++ {
		select {
		case <-h.exitDone:
			return true
		default:
		}
		if i+1 < attempts {
			sleepForReapPoll(interval)
		}
	}
	// Final peek (no sleep after the last attempt).
	select {
	case <-h.exitDone:
		return true
	default:
	}
	incDestroyTaskUnreaped()
	p.logger.Warn("ch: DestroyTask: CH process not reaped within budget; rootfs.img fcntl lock may linger for the next alloc",
		"task_id", taskID,
		"ch_pid", h.chPid,
		"budget", time.Duration(attempts)*interval,
		"destroy_task_unreaped_total", DestroyTaskUnreapedTotal())
	return false
}

// sleepForOFDLockPoll is the package-level seam tests swap so the
// DestroyTask OFD-lock-probe loop doesn't add real wall time. Default
// is time.Sleep — production callers block while the kernel `__fput`
// workqueue catches up after `wait4()` reaps the CH process.
//
// Mirrors the v15 r3-B sleepForTapPoll + r4-A sleepForReapPoll seam
// shape so the test ergonomics are uniform across the three
// defense-in-depth poll loops.
var sleepForOFDLockPoll = func(d time.Duration) {
	time.Sleep(d)
}

// SetSleepForOFDLockPollForTest swaps the OFD-lock-probe sleep seam.
// Returns the previous fn so the caller can restore it on cleanup.
// Tests typically install a no-op so the poll loop spins through its
// budget instantly rather than waiting real wall time.
func SetSleepForOFDLockPollForTest(fn func(d time.Duration)) func(d time.Duration) {
	prev := sleepForOFDLockPoll
	if fn != nil {
		sleepForOFDLockPoll = fn
	}
	return prev
}

// tryAcquireOFDLock is the package-level seam tests swap so the OFD
// probe loop can be driven without touching real filesystem locks.
// Default is the real unix.FcntlFlock(F_OFD_SETLK) path. Returns one
// of:
//
//   - ofdLockProbeAcquired — the F_OFD_SETLK write lock was granted
//     (we released it immediately; `__fput` must have run for this to
//     succeed).
//   - ofdLockProbeBusy — EAGAIN/EACCES; the lock is held elsewhere
//     (deferred `__fput` still pending, or another writer holds it).
//   - ofdLockProbeFileGone — ENOENT; the file no longer exists, so no
//     lock is possible. Treated as success by the caller.
//   - ofdLockProbeError — any other syscall error; the caller surfaces
//     this as an unexpected probe failure (NOT a normal retry).
type ofdLockProbeResult int

const (
	ofdLockProbeAcquired ofdLockProbeResult = iota
	ofdLockProbeBusy
	ofdLockProbeFileGone
	ofdLockProbeError
)

// tryAcquireOFDLockFn is the actual function variable behind the seam.
// Tests swap it via SetTryAcquireOFDLockForTest; the production
// default delegates to realTryAcquireOFDLock.
var tryAcquireOFDLockFn = realTryAcquireOFDLock

// SetTryAcquireOFDLockForTest swaps the OFD-lock probe seam. Returns
// the previous fn so the caller can restore it on cleanup. Tests
// drive the probe loop by returning a canned sequence of
// (busy, busy, …, acquired) without engineering real file locks.
func SetTryAcquireOFDLockForTest(fn func(path string) (ofdLockProbeResult, error)) func(string) (ofdLockProbeResult, error) {
	prev := tryAcquireOFDLockFn
	if fn != nil {
		tryAcquireOFDLockFn = fn
	}
	return prev
}

// realTryAcquireOFDLock opens the path O_RDWR (required for F_WRLCK
// via F_OFD_SETLK — kernel returns EBADF if fd is O_RDONLY), tries to acquire the OFD
// write lock on the whole file, releases on success, and returns the
// classified outcome. Always closes the FD before returning.
//
// The OFD lock semantics (Linux's F_OFD_SETLK, kernel 3.15+, glibc
// 2.20+): each `struct file` (kernel name for an open FD) carries at
// most one OFD lock per range. Two distinct opens of the same inode
// CAN both attempt to acquire — but the kernel grants only the first;
// subsequent attempts get EAGAIN. After `__fput` releases the lock
// (deferred workqueue), the next acquire succeeds.
//
// This is the predicate r5-A needs: we successfully acquire ⇔ no
// other open file description holds the lock ⇔ `__fput` ran on the
// last open that DID hold it. The released lock leaves no residual
// kernel state, so it's safe to issue from cleanup paths.
func realTryAcquireOFDLock(path string) (ofdLockProbeResult, error) {
	fd, err := unix.Open(path, unix.O_RDWR|unix.O_CLOEXEC, 0)
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) || errors.Is(err, unix.ENOENT) {
			return ofdLockProbeFileGone, nil
		}
		return ofdLockProbeError, fmt.Errorf("open: %w", err)
	}
	defer unix.Close(fd)

	// F_OFD_SETLK with F_WRLCK on the whole file (Start=0, Len=0).
	flk := unix.Flock_t{
		Type:   unix.F_WRLCK,
		Whence: int16(unix.SEEK_SET),
		Start:  0,
		Len:    0, // whole file
	}
	if lockErr := unix.FcntlFlock(uintptr(fd), unix.F_OFD_SETLK, &flk); lockErr != nil {
		if errors.Is(lockErr, unix.EAGAIN) || errors.Is(lockErr, unix.EACCES) {
			return ofdLockProbeBusy, nil
		}
		return ofdLockProbeError, fmt.Errorf("F_OFD_SETLK F_WRLCK: %w", lockErr)
	}
	// Got the lock. Release immediately — we're just probing.
	flk.Type = unix.F_UNLCK
	if unlockErr := unix.FcntlFlock(uintptr(fd), unix.F_OFD_SETLK, &flk); unlockErr != nil {
		// Best-effort: the FD close on defer also releases the OFD lock.
		// Log via the result error so the caller can surface the
		// unusual case. Still report Acquired so the loop progresses
		// (the lock IS gone — we successfully detected `__fput` ran).
		return ofdLockProbeAcquired, fmt.Errorf("F_OFD_SETLK F_UNLCK (lock acquired but unlock failed; fd close will release): %w", unlockErr)
	}
	return ofdLockProbeAcquired, nil
}

// pollAcquireOFDLock retries the OFD-lock probe up to
// `destroyLockPollAttempts × destroyLockPollInterval` against a single
// disk path. Returns nil on observed acquire (or file-gone), and a
// descriptive error on budget exhaustion. Unexpected syscall errors
// (anything other than EAGAIN/EACCES) abort the loop immediately.
//
// The caller (DestroyTask via waitForOFDLockRelease) tolerates the
// returned error as a "best-effort: we tried, kernel still busy"
// signal: bumps the counter, WARN-logs, proceeds with cleanup.
// Surfacing an error to Nomad would loop the destroy forever; the
// operator's diagnostic is the metric + log line.
func pollAcquireOFDLock(path string) error {
	attempts := destroyLockPollAttempts
	interval := destroyLockPollInterval
	for attempt := 0; attempt < attempts; attempt++ {
		result, err := tryAcquireOFDLockFn(path)
		switch result {
		case ofdLockProbeAcquired:
			// `__fput` ran (or never had to). We held the lock briefly
			// and released it; if `err != nil` it's an unlock-failed
			// edge case the defer close handles.
			return nil
		case ofdLockProbeFileGone:
			// File doesn't exist — no lock possible. r5-A treats this
			// as success (the lock state we cared about is moot).
			return nil
		case ofdLockProbeError:
			return fmt.Errorf("probe error on attempt %d: %w", attempt+1, err)
		case ofdLockProbeBusy:
			// Expected during the deferred-fput window. Sleep + retry.
		}
		if attempt+1 < attempts {
			sleepForOFDLockPoll(interval)
		}
	}
	return fmt.Errorf("OFD write lock still held after %d × %v poll budget; kernel __fput may be stuck",
		attempts, interval)
}

// taskDiskPathsForLockProbe enumerates the host-side disk paths a
// DestroyTask should OFD-probe before declaring the task terminal.
// The rule, derived from the StartTask staging code:
//
//   - If h.driverConfig.Disks is non-empty (operator-supplied or
//     restore-branch-rewritten), iterate every Path.
//   - Otherwise (cold-boot synthesised path), the canonical three
//     disks are: `<runDir>/rootfs.img`, the workspace image, and
//     the per-user-home image. Skip empties.
//
// Empty paths are filtered (defensive — a half-populated handle
// shouldn't make us syscall a "" path). Duplicates are kept (cost
// is one syscall per duplicate, which is in the noise).
//
// The mandate's r5-A evidence calls out `rootfs.img` specifically;
// probing the other disks costs ~1 ms per disk on the happy path
// (first acquire succeeds) and surfaces a wedged `__fput` on
// workspace/userhome images too — same defense-in-depth shape as the
// `incTapsOrphaned` defensive cleanup pass.
func taskDiskPathsForLockProbe(h *taskHandle, runDir string) []string {
	if h == nil {
		return nil
	}
	if h.driverConfig != nil && len(h.driverConfig.Disks) > 0 {
		paths := make([]string, 0, len(h.driverConfig.Disks))
		for _, d := range h.driverConfig.Disks {
			if d.Path != "" {
				paths = append(paths, d.Path)
			}
		}
		return paths
	}
	// Cold-boot synthesised three-disk list. rootfs.img is always
	// staged at runDir/rootfs.img by materializeRootfs (start_task.go)
	// or stageRootfsForRestore (restore_task.go); workspace and
	// userhome paths come straight from driverConfig.
	paths := make([]string, 0, 3)
	if runDir != "" {
		paths = append(paths, filepath.Join(runDir, chRootfsName))
	}
	if h.driverConfig != nil {
		if h.driverConfig.WorkspaceImg != "" {
			paths = append(paths, h.driverConfig.WorkspaceImg)
		}
		if h.driverConfig.UserHomeImg != "" {
			paths = append(paths, h.driverConfig.UserHomeImg)
		}
	}
	return paths
}

// uniqueTapsForVerify enumerates the distinct tap names this handle
// owns, for the r24-A2-S2 synchronous verify gate. The rule:
//
//   - If h.tap is non-empty, include it.
//   - If h.vmIndex is in range and the derived name differs from
//     h.tap, include the VMIndex-derived defensive name.
//
// Returns 0, 1, or 2 entries. The caller iterates and verifies each.
// Mirrors the pre-existing two-pass removeTapFn structure in
// DestroyTask so we cover both the happy-path lifecycle and the
// half-initialised / external-orphan failure modes the v14 defensive
// cleanup pass added.
//
// T-8b-stress-r8 r24-A2-S2.
func uniqueTapsForVerify(h *taskHandle) []string {
	if h == nil {
		return nil
	}
	out := make([]string, 0, 2)
	if h.tap != "" {
		out = append(out, h.tap)
	}
	if h.vmIndex >= 1 && h.vmIndex <= 155 {
		defensiveTap, _, _, _, err := computeTapAddresses(h.vmIndex, defaultSubnetBaseOctet)
		if err == nil && defensiveTap != "" && defensiveTap != h.tap {
			out = append(out, defensiveTap)
		}
	}
	return out
}

// waitForOFDLockRelease iterates the task's disk paths and polls
// F_OFD_SETLK acquisition on each one. Returns nil on success; on the
// first probe budget exhaustion or unexpected syscall error, returns a
// descriptive error naming the offending path.
//
// Best-effort: the caller bumps the metric + WARN-logs and proceeds
// with cleanup regardless. r5-A's strictly-stronger predicate is the
// OFD-lock acquire; if we got it, `__fput` has run. If we DIDN'T get
// it within budget, the next alloc's `--restore` is at risk of
// `DiskLockError → AlreadyLocked` — but holding up the destroy doesn't
// help (the kernel workqueue isn't waiting on us).
func waitForOFDLockRelease(diskPaths []string) error {
	for i, path := range diskPaths {
		if err := pollAcquireOFDLock(path); err != nil {
			return fmt.Errorf("disk[%d] %q: %w", i, path, err)
		}
	}
	return nil
}

// sleepForTapDeletePoll is the package-level seam tests swap so the
// DestroyTask tap-deletion-verify poll loop doesn't add real wall
// time. Default is time.Sleep — production callers block while the
// tun-driver evicts the netdev. Mirrors sleepForReapPoll /
// sleepForOFDLockPoll seam shapes so the test ergonomics are uniform.
//
// T-8b-stress-r8 r24-A2-S2.
var sleepForTapDeletePoll = func(d time.Duration) {
	time.Sleep(d)
}

// SetSleepForTapDeletePollForTest swaps the tap-deletion-verify sleep
// seam. Returns the previous fn so the caller can restore it on
// cleanup. Tests typically install a no-op so the poll loop spins
// through its budget instantly rather than waiting real wall time.
//
// T-8b-stress-r8 r24-A2-S2.
func SetSleepForTapDeletePollForTest(fn func(d time.Duration)) func(d time.Duration) {
	prev := sleepForTapDeletePoll
	if fn != nil {
		sleepForTapDeletePoll = fn
	}
	return prev
}

// tapLookupFn is the package-level seam tests swap to drive the
// tap-existence probe in deleteTapAndVerifyAbsent. Default
// (realTapLookup) shells to `ip link show <tap>` and classifies the
// result. Returns (absent, err): absent=true means the kernel
// reported ENODEV; err is non-nil only on unexpected syscall failure
// (transient netlink errors are treated as "still present" so the
// poll loop continues to retry).
//
// Tests swap this directly to drive the poll cadence without
// engineering real netdev state — same shape as tryAcquireOFDLockFn.
//
// T-8b-stress-r8 r24-A2-S2.
var tapLookupFn = realTapLookup

// SetTapLookupForTest swaps the tap-existence probe seam. Returns
// the previous fn so the caller can restore it on cleanup. Tests
// drive the poll loop by returning a canned sequence of
// (absent=false, …, absent=true) without engineering real netdev
// state.
//
// T-8b-stress-r8 r24-A2-S2.
func SetTapLookupForTest(fn func(tapName string) (bool, error)) func(string) (bool, error) {
	prev := tapLookupFn
	if fn != nil {
		tapLookupFn = fn
	}
	return prev
}

// realTapLookup runs `ip link show <tap>` and returns (absent, err).
// ENODEV (matched via isNoSuchDevice on the combined output) maps to
// absent=true / nil err. A successful exec means the device is still
// present → absent=false / nil err. A non-ENODEV exec error is
// returned as-is to the caller (transient netlink, EPERM, etc.); the
// poll loop treats it as "still present" and retries until budget
// exhausts.
//
// T-8b-stress-r8 r24-A2-S2.
func realTapLookup(tapName string) (bool, error) {
	if tapName == "" {
		return true, nil // never had a tap — trivially absent
	}
	out, err := runIP("link", "show", tapName)
	if err != nil {
		if isNoSuchDevice(out) {
			return true, nil
		}
		// Unexpected error — surface to the caller. The poll loop
		// retries; if every attempt fails the same way, budget
		// exhausts and the counter bumps with the error in the WARN.
		return false, fmt.Errorf("ip link show %s: %w (output=%q)", tapName, err, string(out))
	}
	// `ip link show` exited 0 → device exists.
	return false, nil
}

// deleteTapAndVerifyAbsent is the r24-A2-S2 synchronous gate.
// Issues `ip link delete <tap>` (tolerating ENODEV — already gone is
// success), then polls `ip link show <tap>` until the kernel reports
// ENODEV or the budget exhausts.
//
// Returns nil on observed ENODEV. Returns a descriptive error on
// budget exhaustion or on an unexpected `ip link delete` failure.
// The caller (DestroyTask) bumps the counter + WARN-logs on error
// and proceeds — mirrors r4-A/r5-A's "never block Nomad destroy"
// tolerance.
//
// Why this is in addition to the existing best-effort removeTapFn:
// the prior call swallowed errors but didn't verify the kernel
// observed the eviction. The next StartTask's tuntap-add can hit
// EEXIST until the tun-driver's internal cleanup tick runs. The v15
// r3-B collision-replace path absorbs this on the CREATE side, but
// closing the window on the DESTROY side eliminates the second
// tuntap-add cycle and the orphan-counter bump that comes with it.
//
// T-8b-stress-r8 r24-A2-S2.
func deleteTapAndVerifyAbsent(tapName string) error {
	if tapName == "" {
		return nil // never had a tap — nothing to verify
	}
	// Issue the delete via removeTapFn so the existing test seams
	// (SetRemoveTapForTest in stop_task_test.go) see this call too.
	// realTeardownTap already tolerates ENODEV via isNoSuchDevice —
	// so a fresh-from-removeTapFn-call-above handle takes the no-op
	// branch here on the second call. We still need the issue-then-
	// verify pair because the prior removeTapFn call site doesn't
	// guarantee the device is gone before returning (the kernel
	// release is async).
	if err := removeTapFn(tapName); err != nil {
		return fmt.Errorf("ip link delete %s: %w", tapName, err)
	}
	// Poll for ENODEV. The kernel release is dominated by the
	// tun-driver's internal cleanup tick, not poll cadence — 100 ms
	// is the same shape v15 r3-B uses for the symmetric race on the
	// CREATE side.
	attempts := destroyTapDeletePollAttempts
	interval := destroyTapDeletePollInterval
	var lastErr error
	for i := 0; i < attempts; i++ {
		absent, err := tapLookupFn(tapName)
		if err != nil {
			// Track the last transient error so a stuck netlink
			// surfaces in the budget-exhaust message instead of
			// being silently retried into oblivion.
			lastErr = err
		} else if absent {
			return nil
		}
		if i+1 < attempts {
			sleepForTapDeletePoll(interval)
		}
	}
	if lastErr != nil {
		return fmt.Errorf("tap %s still present after %d × %v poll budget (last lookup err: %v)",
			tapName, attempts, interval, lastErr)
	}
	return fmt.Errorf("tap %s still present after %d × %v poll budget; kernel netdev cleanup may be stuck",
		tapName, attempts, interval)
}

// waitForExit blocks up to `d` for the handle's supervisor goroutine to
// signal exit (closes h.exitDone). Returns true if the supervisor exited
// within the window.
//
// If h.exitDone is nil (RecoverTask handle without a runner — T-4
// territory), waitForExit returns false immediately; the caller falls
// through to the next ladder step (SIGTERM/SIGKILL via PID).
func (p *Plugin) waitForExit(h *taskHandle, d time.Duration) bool {
	if h.exitDone == nil {
		return false
	}
	if d <= 0 {
		// Non-blocking peek so a 0-timeout still notices an already-exited VM.
		select {
		case <-h.exitDone:
			return true
		default:
			return false
		}
	}
	timer := time.NewTimer(d)
	defer timer.Stop()
	select {
	case <-h.exitDone:
		return true
	case <-timer.C:
		return false
	}
}

// DestroyTask tears down all resources associated with a task. Idempotent
// per Nomad's driver contract: calling it twice (or against an unknown task,
// or against a task whose CH already exited) must not error.
//
// Flow:
//
//  1. Look up taskHandle; if absent, return nil (idempotent).
//  2. If still running and !force, return an error so Nomad calls
//     StopTask first.
//  3. If still running and force, SIGKILL via the StopTask ladder's last
//     step.
//  4. **T-8b-stress-r4 r4-A**: bounded wait for the OS to REAP the CH
//     process before declaring the task terminal to Nomad. fcntl file
//     locks (CH's rootfs.img ExclusiveWrite among them) release on reap,
//     not on SIGKILL — so a subsequent alloc's `--restore` on the same
//     rootfs.img hits `DiskLockError → AlreadyLocked` until the kernel
//     finishes reaping. waitForReap selects on h.exitDone, which
//     superviseCH closes AFTER runner.Wait() returns (cmd.Wait reaps).
//  4b. **T-8b-stress-r5 r5-A**: AFTER reap, bounded poll on
//      `F_OFD_SETLK F_WRLCK` acquire against each disk path. The
//      strictly-stronger predicate: reap removes the zombie but
//      Linux's deferred `__fput` workqueue still holds the file open
//      (OFD write lock attributed to PID=-1) until it runs. We
//      succeed acquiring ⇔ `__fput` ran. See `pollAcquireOFDLock`.
//  5. Cancel handle.ctx so any per-task supervision goroutines (TaskStats,
//     future WaitTask monitors) exit.
//  6. Best-effort tap removal — failure is logged, not surfaced; the tap
//     may already be gone (CH crashed) or owned by an external systemd
//     unit (T-3 territory). Two passes: first keyed off h.tap (the
//     happy-path lifecycle), then a DEFENSIVE pass keyed off h.vmIndex
//     so a half-initialised handle (h.tap == "") or an external orphan
//     still gets cleaned (T-8b-stress-r2 driver v14 — see in-body
//     comment for the failure modes the defensive pass covers).
//  7. Best-effort API socket removal — file may already be gone (CH
//     unlinks on clean exit).
//  8. Delete the in-memory handle from p.tasks.
//
// T-3 (per-VM /30 tap) and T-7 (controller-integration) extend this with
// vm_index lock release and per-task run-dir scrubbing; this T-2 surface
// covers the artifacts the cold-boot path actually creates.
func (p *Plugin) DestroyTask(taskID string, force bool) error {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		p.logger.Debug("ch: DestroyTask on unknown task; idempotent no-op", "task_id", taskID)
		return nil
	}

	if h.IsRunning() {
		if !force {
			return errors.New("ch: DestroyTask: task is still running; StopTask first or pass force=true")
		}
		// force=true: escalate the ladder's final step. This also waits
		// for the supervisor to record the exit before we proceed to
		// cleanup, so the in-memory state is consistent.
		_ = p.escalateSigkill(h, stopTimeoutsMu.sigtermTimeout)
	}

	// T-8b-stress-r4 r4-A: bounded wait for the CH process to be REAPED
	// before we declare the task terminal to Nomad. Without this, the next
	// alloc on the same rootfs.img sees `DiskLockError → AlreadyLocked`
	// (the fcntl write lock attaches to a still-zombie task that the
	// kernel hasn't finished reaping). h.exitDone is closed by superviseCH
	// AFTER runner.Wait() returns — for the default os/exec runner that
	// IS the reap; for the detachedRunner (RecoverTask) it's an ESRCH on
	// kill(pid,0), which fires only after the kernel completes reap of
	// the orphaned-then-reparented-to-init child.
	//
	// Best-effort: on budget exhaustion we bump the metric, WARN-log, and
	// proceed. Nomad needs a definitive terminal signal — surfacing an
	// error here would leave Nomad in a "destroy keeps failing" loop that
	// doesn't help the cluster recover. The next StartTask is what would
	// see the residual lock; the operator's diagnostic is the metric +
	// WARN line.
	p.waitForReap(h, taskID)

	// T-8b-stress-r5 r5-A: AFTER the reap-wait above, poll
	// F_OFD_SETLK acquire on every disk path to ensure the kernel's
	// `__fput` workqueue has actually released the OFD write lock.
	//
	// Why this layer exists: r4-A's predicate (Go's `cmd.Wait()`
	// returning) is necessary but not sufficient. Linux's `__fput`
	// runs in a deferred workqueue triggered from the LAST `close()`/
	// `exit()` on a `struct file`; until it completes, the OFD write
	// lock on `rootfs.img` persists with `owner=-1` (the dead task).
	// `wait4()` reaps the zombie, but the workqueue runs separately —
	// `/proc/locks` evidence at stress-r5 RED 3/60 (5%) showed
	// `OFDLCK WRITE 0 fd:01:<inode> 0 EOF` with owner=-1 for hundreds
	// of milliseconds after Go's `cmd.Wait()` returned.
	//
	// The strictly-stronger predicate is the inverse: try to ACQUIRE
	// the OFD write lock ourselves. If we succeed, `__fput` ran (the
	// kernel grants only one OFD write lock per inode/range). Acquire-
	// and-release per disk path gives us the actual kernel guarantee.
	//
	// Best-effort: on budget exhaustion we bump
	// `nomad_driver_ch_destroy_task_lock_held_total`, WARN-log, and
	// proceed — mirroring r4-A's tolerance. The operator's
	// diagnostic is the metric + WARN line; an alert on the rate of
	// this counter signals a backed-up kernel `__fput` workqueue
	// (orthogonal to the driver — but only the driver is positioned
	// to observe it cheaply).
	runDir := taskRunDir(h.taskConfig, p.config)
	diskPaths := taskDiskPathsForLockProbe(h, runDir)
	if len(diskPaths) > 0 {
		if err := waitForOFDLockRelease(diskPaths); err != nil {
			incDestroyTaskLockHeld()
			p.logger.Warn("ch: DestroyTask: OFD write lock not released within budget; deferred __fput may be stuck",
				"task_id", taskID,
				"ch_pid", h.chPid,
				"err", err,
				"destroy_task_lock_held_total", DestroyTaskLockHeldTotal())
		}
	}

	// Stop the per-task ctx (cancels any background goroutines T-5
	// introduces; safe even if cancelFn is a no-op).
	if h.cancelFn != nil {
		h.cancelFn()
	}

	// Cleanup tail — best-effort, errors only logged. The bash wrapper
	// also runs these unconditionally; failures (e.g. tap already gone)
	// are expected on the crash-recovery path.
	//
	// T-8b-stress-r2 driver v14: DEFENSIVE tap cleanup keyed off VMIndex.
	//
	// Pre-v14 the only call site was `if h.tap != "" { removeTapFn(h.tap) }` —
	// which is correct for a happy-path lifecycle (StartTask sets h.tap,
	// DestroyTask removes it). But two failure modes leave taps stranded:
	//
	//   1. **Partial StartTask** — setupTapForVM creates the tap, then a
	//      later step (decode validation, runDir mkdir, processRunner
	//      spawn) fails. We unwind via `tapRollback` in start_task.go
	//      (G4) BUT only when the handle was never registered in p.tasks.
	//      Once the handle is registered, DestroyTask owns the cleanup —
	//      and if a *different* failure path reaches DestroyTask via a
	//      handle that didn't reach the SetDriverState line where h.tap
	//      is assigned, `h.tap == ""` and the cleanup is skipped.
	//   2. **External orphan** — a prior alloc's DestroyTask never ran
	//      (Nomad client SIGKILL'd, controller-driven force-purge before
	//      DestroyTask fires, etc.). The next alloc on the same VMIndex
	//      hits the EEXIST in setupTapForVM and recovers via the v13
	//      pre-delete; but if THIS alloc later destroys and the tap was
	//      created by a peer process / external script, h.tap == "" and
	//      again the cleanup is skipped.
	//
	// Fix: in addition to the conditional removal keyed off h.tap (which
	// still runs first so an attached test seam observes both paths),
	// derive the deterministic tap name `zsbx-nm-<vmIndex>` from
	// h.vmIndex and `ip link delete` it. `realTeardownTap` already
	// tolerates `Cannot find device` (line :215 of net.go), so a fresh-
	// from-StartTask handle that already cleaned up via h.tap takes the
	// no-op branch on the second call. Only count this as an "orphan"
	// when h.tap was empty (otherwise it's the normal lifecycle delete,
	// just done twice — the second call is a free idempotency check).
	if h.tap != "" {
		if err := removeTapFn(h.tap); err != nil {
			p.logger.Warn("ch: DestroyTask: tap removal failed (best-effort)",
				"task_id", taskID, "tap", h.tap, "err", err)
		}
	}
	if h.vmIndex >= 1 && h.vmIndex <= 155 {
		// Use computeTapAddresses for the deterministic name so the
		// defense-in-depth uses the same arithmetic as setupTapForVM
		// (single source of truth — a drift between the two would
		// silently disable this hook).
		defensiveTap, _, _, _, err := computeTapAddresses(h.vmIndex, defaultSubnetBaseOctet)
		if err == nil {
			// Skip the second delete when h.tap already covered the
			// same name (avoid double-logging the noop).
			//
			// Goes through removeTapFn (the T-2 alias for
			// teardownTapFn) so an existing test that swaps
			// SetRemoveTapForTest sees the second call too. The seam
			// names are split for historical reasons (ch_client.go:
			// 369-371) but funnel into the same impl in production —
			// using removeTapFn here keeps the test ergonomics
			// uniform.
			if h.tap != defensiveTap {
				if delErr := removeTapFn(defensiveTap); delErr != nil {
					p.logger.Warn("ch: DestroyTask: defensive tap removal failed (best-effort)",
						"task_id", taskID, "tap", defensiveTap, "vm_index", h.vmIndex, "err", delErr)
				} else {
					// Successful defensive cleanup — count as orphan
					// observability. Skip the bump when h.tap was the
					// SAME name and just ran above (no orphan, just
					// happy-path); the != guard above already gates that.
					incTapsOrphaned()
					p.logger.Info("ch: DestroyTask: defensive tap cleanup fired (vm_index keyed)",
						"task_id", taskID, "tap", defensiveTap, "vm_index", h.vmIndex,
						"taps_orphaned_total", TapsOrphanedTotal())
				}
			}
		}
	}

	// T-8b-stress-r8 r24-A2-S2: synchronous tap-delete + ENODEV-verify
	// gate. The two best-effort removeTapFn calls above swallow errors
	// and don't verify the kernel observed the eviction. Stress-r8 RED
	// 3/60 showed cycles 1-19 failing identically with
	// `Tap zsbx-nm-N already exists`: `ip link delete` returns
	// synchronously but the tun-driver releases the netdev
	// asynchronously, so the next StartTask's tuntap-add can collide
	// until the kernel's internal cleanup tick runs. The v13 collision-
	// replace path absorbs the EEXIST on the CREATE side but adds a
	// second tuntap-add cycle (and the orphan-counter bump that comes
	// with it) — closing the window here is the better fix.
	//
	// Strategy: enumerate the unique tap names this handle owns (h.tap
	// + the VMIndex-derived defensive name), then for each one run
	// `deleteTapAndVerifyAbsent` which re-issues the delete (idempotent
	// — realTeardownTap tolerates ENODEV) and polls `ip link show`
	// until ENODEV or the budget exhausts.
	//
	// On budget exhaustion: bump `destroy_task_tap_stuck_total`,
	// WARN-log, proceed. Mirrors r4-A/r5-A's "never block Nomad
	// destroy" tolerance — the operator's diagnostic is the metric +
	// WARN line, not a destroy-loop.
	for _, tapName := range uniqueTapsForVerify(h) {
		if err := deleteTapAndVerifyAbsent(tapName); err != nil {
			incDestroyTaskTapStuck()
			p.logger.Warn("ch: DestroyTask: tap netdev not evicted within budget; next StartTask on the same vm_index may hit EEXIST",
				"task_id", taskID,
				"tap", tapName,
				"vm_index", h.vmIndex,
				"err", err,
				"destroy_task_tap_stuck_total", DestroyTaskTapStuckTotal())
		}
	}

	if h.apiSocket != "" {
		if err := os.Remove(h.apiSocket); err != nil && !os.IsNotExist(err) {
			p.logger.Warn("ch: DestroyTask: api socket removal failed (best-effort)",
				"task_id", taskID, "api_socket", h.apiSocket, "err", err)
		}
	}

	p.tasks.Delete(taskID)
	p.logger.Info("ch: DestroyTask: complete", "task_id", taskID)
	return nil
}

// Compile-time guard against drift: drivers.ErrTaskNotFound is the
// sentinel Nomad expects from WaitTask/InspectTask/TaskStats on an
// unknown task. StopTask/DestroyTask are EXEMPT — they return nil on
// unknown task per the contract — but keeping the import here documents
// the convention for the surrounding code.
var _ = drivers.ErrTaskNotFound
