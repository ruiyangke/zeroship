// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::RecoverTask) on
// 2026-05-25 for Cloud Hypervisor support. The libvirt connection re-attach
// is replaced by a CH API-socket reconnect (proposal § 7 "RecoverTask").
//
// This is the headline feature of moving off the bash wrapper: Nomad-client
// restarts no longer orphan CH processes. cf. the volantvm spike, which
// identified the upstream CH driver's RecoverTask as broken (it only consults
// an in-memory map that doesn't survive restart). Ours persists CHPid +
// APISocket via drivers.TaskHandle.SetDriverState and re-attaches by pinging
// the API socket.

package ch

import (
	"context"
	"errors"
	"fmt"
	"os"
	"syscall"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// recoverPollInterval is how often the detachedRunner's supervisor polls
// the recovered PID with Signal(0) to detect exit. 2 s is the wrapper's
// historical poll cadence and is well below Nomad's WaitTask-poll budget
// (Nomad re-issues WaitTask every ~5 s when the channel is closed
// without a result).
var recoverPollInterval = 2 * time.Second

// SetRecoverPollIntervalForTest tightens the detachedRunner's polling
// cadence so tests can observe exit detection without waiting 2 s.
// Returns the previous interval so the test can restore it on cleanup.
func SetRecoverPollIntervalForTest(d time.Duration) time.Duration {
	prev := recoverPollInterval
	if d > 0 {
		recoverPollInterval = d
	}
	return prev
}

// processAliveFn is the package-level seam tests swap to fake PID liveness
// without depending on the host's PID layout. Default delegates to
// processAlive which uses os.FindProcess + Signal(0). Mirrors the
// shutdownFn / probeFn pattern.
var processAliveFn = processAlive

// SetProcessAliveForTest replaces the PID-liveness seam. Returns the
// previous fn so the caller can restore it on cleanup.
func SetProcessAliveForTest(fn func(pid int) error) func(int) error {
	prev := processAliveFn
	if fn != nil {
		processAliveFn = fn
	}
	return prev
}

// processAlive returns nil if a process with the given PID is alive
// (Signal(0) reaches it) or an error otherwise. The most common failure
// mode is ESRCH (process gone); EPERM (process exists but we lack
// permission to signal) is treated as alive because the discriminator we
// want is "kernel knows about this PID" rather than "we can kill it".
//
// On Linux, os.FindProcess never returns an error — the *os.Process is
// a thin wrapper around the PID until Signal/Wait is called. The actual
// liveness probe is Signal(syscall.Signal(0)): the kernel resolves the
// PID and returns ESRCH if no such process exists.
func processAlive(pid int) error {
	if pid <= 0 {
		return fmt.Errorf("ch: invalid pid %d", pid)
	}
	proc, err := os.FindProcess(pid)
	if err != nil {
		return fmt.Errorf("ch: FindProcess(%d): %w", pid, err)
	}
	if err := proc.Signal(syscall.Signal(0)); err != nil {
		// errors.Is(err, os.ErrProcessDone) covers the modern Go shape;
		// fall back to error-string matching for ESRCH on older runtimes.
		if errors.Is(err, os.ErrProcessDone) || errors.Is(err, syscall.ESRCH) {
			return fmt.Errorf("ch: process %d is gone", pid)
		}
		// EPERM means the process exists but we can't signal it.
		// That counts as "alive" for our purposes — RecoverTask only
		// needs to know the kernel still owns the PID. Forwarding the
		// signal at StopTask time may fail later, but that's a separate
		// branch.
		if errors.Is(err, syscall.EPERM) {
			return nil
		}
		return fmt.Errorf("ch: probe pid %d: %w", pid, err)
	}
	return nil
}

// RecoverTask rebuilds the in-memory taskHandle for a task that was running
// before this plugin process started. Called by Nomad at plugin load for
// every task it believes should still be running.
//
// Flow:
//
//  1. Decode TaskState from handle.GetDriverState; Validate the shape.
//  2. If a handle with the same ID is already in p.tasks, return nil
//     (idempotent — Nomad may double-call).
//  3. Probe the CH process via Signal(0); ESRCH means dead.
//  4. Probe the ch-remote API socket via Client.Probe (socket-stat +
//     ch-remote info). Failure surfaces stderr verbatim.
//  5. Construct a detachedRunner around the PID (no exec.Cmd; the
//     supervisor polls Signal(0) every recoverPollInterval and surfaces
//     exit through the same channel shape superviseCH uses).
//  6. Build the taskHandle and register it under p.tasks, with the
//     supervisor goroutine running.
//
// Returns nil on success. Failure modes are surfaced as explicit errors
// so the Nomad-client task log shows the precise cause; Nomad treats a
// RecoverTask error as "task lost" and the controller's reconciler
// recreates the alloc.
func (p *Plugin) RecoverTask(handle *drivers.TaskHandle) error {
	if handle == nil {
		return errors.New("ch: T-4: nil handle")
	}

	if handle.Config == nil {
		return errors.New("ch: T-4: handle.Config is nil")
	}

	if _, ok := p.tasks.Get(handle.Config.ID); ok {
		// Idempotent: Nomad may double-call after a flaky plugin
		// re-attach. Treat the existing handle as authoritative.
		return nil
	}

	var state TaskState
	if err := handle.GetDriverState(&state); err != nil {
		return fmt.Errorf("ch: T-4: failed to decode TaskState: %w", err)
	}
	if err := state.Validate(); err != nil {
		return fmt.Errorf("ch: T-4: invalid TaskState: %w", err)
	}

	p.logger.Info("ch: RecoverTask",
		"task_id", handle.Config.ID,
		"ch_pid", state.CHPid,
		"api_socket", state.APISocket,
		"vm_index", state.VMIndex,
		"mode", state.Mode)

	// Step 1: PID liveness probe. The cheapest gate — if the kernel has
	// already reaped the PID, there's no point dialing the socket.
	if err := processAliveFn(state.CHPid); err != nil {
		return fmt.Errorf("ch: T-4: %w", err)
	}

	// Step 2: API-socket + ch-remote info probe. Discriminates the case
	// "PID is alive but it's not our CH" (PID got reused for an
	// unrelated process) from the happy path. A non-CH process won't
	// respond on the per-VM api-socket path (the socket file either
	// won't exist or, if it does, ch-remote info will dial-fail).
	if err := probeFn(p.chClient, state.APISocket); err != nil {
		return fmt.Errorf("ch: T-4: %w", err)
	}

	// All gates passed. Reconstruct the in-memory handle with a
	// detached runner. The runner's supervisor polls Signal(0) on the
	// recovered PID to surface exit through the same exitDone shape
	// superviseCH writes — so WaitTask / StopTask / DestroyTask see no
	// difference between a StartTask-spawned handle and a recovered
	// one.
	ctx, cancel := context.WithCancel(context.Background())
	runner := newDetachedRunner(state.CHPid, recoverPollInterval, ctx)

	h := &taskHandle{
		logger:       p.logger.With("task_id", handle.Config.ID, "vm_index", state.VMIndex),
		taskConfig:   handle.Config,
		driverConfig: nil, // not round-tripped — RecoverTask works from TaskState alone
		procState:    drivers.TaskStateRunning,
		startedAt:    state.StartedAt,
		exitResult:   &drivers.ExitResult{},
		chPid:        state.CHPid,
		apiSocket:    state.APISocket,
		vmIndex:      state.VMIndex,
		tap:          state.Tap,
		mode:         state.Mode,
		runner:       runner,
		exitDone:     make(chan struct{}),
		ctx:          ctx,
		cancelFn:     cancel,
	}
	p.tasks.Set(handle.Config.ID, h)

	// Supervisor goroutine: blocks on the detached runner's polling
	// Wait, records exit, closes exitDone. Identical shape to
	// superviseCH so WaitTask subscribers don't need to special-case
	// recovered handles.
	go p.superviseCH(h)

	p.logger.Info("ch: RecoverTask: re-attached",
		"task_id", handle.Config.ID,
		"ch_pid", state.CHPid,
		"api_socket", state.APISocket,
		"tap", state.Tap)

	return nil
}

// detachedRunner is the processRunner impl RecoverTask attaches to a
// PID it didn't spawn. Unlike defaultRunner (which owns an *exec.Cmd
// and can call cmd.Wait), the detached path can only poll the PID via
// Signal(0) because the original parent (this plugin process) is gone
// across the restart and the CH process has been re-parented to init.
//
// Wait blocks until either:
//
//  1. processAliveFn(pid) reports ESRCH (process gone) → returns nil
//     with exitCode=-1 (we have no exit-status access for a non-child
//     process; -1 is the convention defaultRunner uses for
//     signal-terminated processes too).
//  2. ctx is cancelled (StopTask / DestroyTask drove a kill that
//     succeeded; the cancel arrives just before the kernel reaps the
//     PID, but the next poll loop will see ESRCH anyway) → returns
//     nil.
//
// Signal forwards via os.FindProcess.Signal; identical to defaultRunner's
// signal path. StderrTail returns nil — there's no stderr to capture for
// a process we didn't spawn. The supervisor + WaitTask code paths
// gracefully handle that (chWaitError only wraps when tail is non-empty).
type detachedRunner struct {
	pid          int
	pollInterval time.Duration
	ctx          context.Context

	// exitCode is the convention -1 (signal-terminated) for the entire
	// lifetime of a detached runner: we don't own the child, so we have
	// no waitstatus to read. Set once at construction; never mutated.
	exitCode int
}

// newDetachedRunner constructs a detachedRunner for a re-attached PID.
// The returned runner is "already started" — Start is a no-op (a noop
// matches defaultRunner's contract: Start spawns the process, and a
// detached runner has nothing to spawn).
func newDetachedRunner(pid int, pollInterval time.Duration, ctx context.Context) *detachedRunner {
	if pollInterval <= 0 {
		pollInterval = recoverPollInterval
	}
	return &detachedRunner{
		pid:          pid,
		pollInterval: pollInterval,
		ctx:          ctx,
		exitCode:     -1, // until Wait observes ESRCH; matches signal-terminated convention
	}
}

func (r *detachedRunner) Start() error {
	// No-op — the process was started across the plugin-restart boundary.
	// Returning nil keeps the StartTask/RecoverTask code paths uniform.
	return nil
}

func (r *detachedRunner) Wait() error {
	ticker := time.NewTicker(r.pollInterval)
	defer ticker.Stop()
	for {
		select {
		case <-r.ctx.Done():
			return nil
		case <-ticker.C:
			if err := processAliveFn(r.pid); err != nil {
				// ESRCH (or any kernel-level "this PID is gone")
				// is the signal we wait for; surface a clean exit
				// rather than the probe error (the probe error's
				// shape is an implementation detail).
				return nil
			}
		}
	}
}

func (r *detachedRunner) Pid() int {
	return r.pid
}

func (r *detachedRunner) Signal(sig os.Signal) error {
	if r.pid <= 0 {
		return fmt.Errorf("ch: detachedRunner.Signal: invalid pid %d", r.pid)
	}
	proc, err := os.FindProcess(r.pid)
	if err != nil {
		return fmt.Errorf("ch: detachedRunner.Signal: FindProcess(%d): %w", r.pid, err)
	}
	return proc.Signal(sig)
}

func (r *detachedRunner) ExitCode() int {
	return r.exitCode
}

// StderrTail returns nil for detached runners — we never owned the
// stderr fd. WaitTask's chWaitError wrap-path tolerates a nil tail.
func (r *detachedRunner) StderrTail(_ int) []byte {
	return nil
}
