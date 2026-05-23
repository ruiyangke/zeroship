// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::WaitTask) on
// 2026-05-25 for Cloud Hypervisor support. The libvirt domStats poller is
// replaced by direct supervision of the CH process PID (proposal § 7
// "WaitTask").

package ch

import (
	"context"
	"errors"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// WaitTask returns a channel that fires exactly once when the CH process
// exits (or when ctx is cancelled). Flow when implemented (T-2):
//
//  1. Look up the taskHandle; if absent, return drivers.ErrTaskNotFound.
//  2. If the task already exited, send the cached ExitResult and close.
//  3. Otherwise, spawn a goroutine that:
//     - awaits on the CH PID (syscall.Wait4 or a tick-and-/proc-check loop;
//       upstream uses a ticker, we should do the same for portability);
//     - on exit, fills h.exitResult with the exit code + signal + oom flag
//       (cf. proposal § 7: scrape memory.events for oom_kill > 0);
//     - sends the result on the channel and closes.
//
// A WaitTask call after StopTask is valid (Nomad relies on this); a WaitTask
// after DestroyTask must return drivers.ErrTaskNotFound.
func (p *Plugin) WaitTask(ctx context.Context, taskID string) (<-chan *drivers.ExitResult, error) {
	if _, ok := p.tasks.Get(taskID); !ok {
		return nil, drivers.ErrTaskNotFound
	}

	exitCh := make(chan *drivers.ExitResult, 1)
	go func() {
		defer close(exitCh)
		// T-2: stub. Emit a fake non-zero result so any code path that
		// blocks on WaitTask doesn't hang forever during scaffold testing.
		exitCh <- &drivers.ExitResult{
			ExitCode: 1,
			Err:      errors.New("ch: T-2: WaitTask not implemented"),
		}
	}()
	return exitCh, nil
}
