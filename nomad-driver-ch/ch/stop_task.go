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
	"os"
	"strings"
	"syscall"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
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
)

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
//  4. Cancel handle.ctx so any per-task supervision goroutines (TaskStats,
//     future WaitTask monitors) exit.
//  5. Best-effort tap removal — failure is logged, not surfaced; the tap
//     may already be gone (CH crashed) or owned by an external systemd
//     unit (T-3 territory). Two passes: first keyed off h.tap (the
//     happy-path lifecycle), then a DEFENSIVE pass keyed off h.vmIndex
//     so a half-initialised handle (h.tap == "") or an external orphan
//     still gets cleaned (T-8b-stress-r2 driver v14 — see in-body
//     comment for the failure modes the defensive pass covers).
//  6. Best-effort API socket removal — file may already be gone (CH
//     unlinks on clean exit).
//  7. Delete the in-memory handle from p.tasks.
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
