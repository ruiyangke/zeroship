// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::WaitTask) on
// 2026-05-25 for Cloud Hypervisor support. The libvirt domStats poller is
// replaced by subscription to the per-task exitDone channel that
// superviseCH (in start_task.go) closes when runner.Wait() returns.
//
// One supervisor goroutine per task owns the (single) runner.Wait()
// invocation; N concurrent WaitTask subscribers all read the cached
// ExitResult via the shared exitDone channel. This matches Nomad's
// at-least-once WaitTask contract — Nomad may call WaitTask repeatedly
// after StopTask, and each call must return the same exit result.

package ch

import (
	"context"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// stderrTailLimit is the byte budget for the stderr tail attached to the
// exit result. The proposal target was 512 B; 1 KiB gives a little more
// breathing room for a multi-line CH error without ballooning the log.
const stderrTailLimit = 1024

// WaitTask returns a channel that fires exactly once when the CH process
// exits (or when ctx is cancelled).
//
// Per Nomad's driver contract:
//   - If the task is unknown → drivers.ErrTaskNotFound.
//   - If the task already exited → emit the cached ExitResult immediately.
//   - Otherwise → subscribe to handle.exitDone (closed by superviseCH
//     when runner.Wait returns).
//
// A WaitTask call after StopTask is valid (Nomad relies on this); a
// WaitTask after DestroyTask must return drivers.ErrTaskNotFound — that
// invariant is enforced by DestroyTask removing the handle from p.tasks.
func (p *Plugin) WaitTask(ctx context.Context, taskID string) (<-chan *drivers.ExitResult, error) {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		return nil, drivers.ErrTaskNotFound
	}

	exitCh := make(chan *drivers.ExitResult, 1)

	// Fast path: if the supervisor has already recorded an exit, just
	// emit the cached result. Reads under RLock so a concurrent
	// supervisor write doesn't race.
	if cached := h.snapshotExit(); cached != nil {
		exitCh <- cached
		close(exitCh)
		return exitCh, nil
	}

	// No exitDone — RecoverTask or pre-StartTask handle. Surface the
	// missing supervisor explicitly so callers don't hang forever.
	if h.exitDone == nil {
		go func() {
			defer close(exitCh)
			exitCh <- &drivers.ExitResult{
				ExitCode: -1,
				Err:      ErrNotImplemented,
			}
		}()
		return exitCh, nil
	}

	go func() {
		defer close(exitCh)
		select {
		case <-h.exitDone:
			// Supervisor finished; the cached result must be non-nil now.
			if cached := h.snapshotExit(); cached != nil {
				exitCh <- cached
				return
			}
			// Defensive — supervisor closed exitDone without recording a
			// result. Shouldn't happen, but emit a sentinel rather than
			// hang.
			exitCh <- &drivers.ExitResult{
				ExitCode: -1,
				Err:      ErrNotImplemented,
			}
		case <-ctx.Done():
			exitCh <- &drivers.ExitResult{
				ExitCode: -1,
				Err:      ctx.Err(),
			}
		}
	}()

	return exitCh, nil
}

// snapshotExit returns a copy of the cached ExitResult if the supervisor
// has already recorded one; nil otherwise. Cheap; the common case (still
// running) takes only the RLock + a state-enum check.
func (h *taskHandle) snapshotExit() *drivers.ExitResult {
	h.stateMu.RLock()
	defer h.stateMu.RUnlock()
	if h.procState != drivers.TaskStateExited || h.exitResult == nil {
		return nil
	}
	return h.exitResult.Copy()
}

// chWaitError carries the stderr tail alongside the underlying exec error
// so the operator-facing log line shows both "exit status 1" and the last
// few hundred bytes of CH's complaint. Defined here (not in ch_client.go)
// because superviseCH is the sole producer and WaitTask is the sole
// consumer.
type chWaitError struct {
	base error
	tail []byte
}

func (e *chWaitError) Error() string {
	return e.base.Error() + ": " + string(e.tail)
}

func (e *chWaitError) Unwrap() error { return e.base }
