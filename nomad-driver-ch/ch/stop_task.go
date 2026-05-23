// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::StopTask &
// DestroyTask) on 2026-05-25 for Cloud Hypervisor support. The libvirt
// StopVM/DestroyVM path is replaced by the CH-specific ladder described in
// proposal § 7 ("StopTask" + "DestroyTask").

package ch

import (
	"errors"
	"time"
)

// StopTask gracefully stops the CH process backing the task. Flow when
// implemented (T-2):
//
//  1. Look up taskHandle by id; if absent, return nil (idempotent).
//  2. If signal != "" and signal != "SIGTERM": forward via os.Process.Signal
//     (used by `nomad alloc signal` for dev).
//  3. Otherwise: issue `ch-remote --api-socket <h.apiSocket> shutdown`.
//  4. Wait up to `timeout` for the CH PID to reap.
//  5. If still alive, send SIGKILL.
//  6. Cancel handle.ctx so the WaitTask monitor exits.
//
// StopTask must NOT clean up — DestroyTask does that. This matches the
// upstream virt driver and the proposal § 7.
func (p *Plugin) StopTask(taskID string, timeout time.Duration, signal string) error {
	if _, ok := p.tasks.Get(taskID); !ok {
		p.logger.Warn("ch: StopTask on unknown task; ignoring", "task_id", taskID)
		return nil
	}
	return errors.New("ch: T-2: StopTask not implemented")
}

// DestroyTask tears down all resources associated with a task. Flow when
// implemented (T-2):
//
//  1. Look up taskHandle; if absent, return nil (idempotent).
//  2. If still running and !force, return an error (Nomad will retry after
//     a StopTask).
//  3. SIGKILL CH if still alive.
//  4. Kill all virtiofsd children (recorded under the per-task run dir).
//  5. `ip tuntap del <handle.tap>`.
//  6. Release the vm_index lock under p.config.VMIndexLockDir.
//  7. Remove the per-task run dir (api socket, virtiofsd sockets, pid file).
//  8. Delete the in-memory handle from p.tasks.
//
// Idempotent — safe to call after RecoverTask discovered the CH was gone.
func (p *Plugin) DestroyTask(taskID string, force bool) error {
	if _, ok := p.tasks.Get(taskID); !ok {
		p.logger.Warn("ch: DestroyTask on unknown task; ignoring", "task_id", taskID)
		return nil
	}
	return errors.New("ch: T-2: DestroyTask not implemented")
}
