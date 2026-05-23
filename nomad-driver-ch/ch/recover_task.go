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

	"github.com/hashicorp/nomad/plugins/drivers"
)

// RecoverTask rebuilds the in-memory taskHandle for a task that was running
// before this plugin process started. Called by Nomad at plugin load for
// every task it believes should still be running.
//
// Flow when implemented (T-4):
//
//  1. Decode TaskState from handle.GetDriverState.
//  2. If a handle with the same ID is already in p.tasks, return nil
//     (idempotent — Nomad may double-call).
//  3. Stat /proc/<state.CHPid>/comm; verify it's "cloud-hypervisor".
//  4. Open state.APISocket; issue `ch-remote info`. Confirm vm_state is
//     Running or Resumed.
//  5. If either check fails, return drivers.ErrTaskNotFound — Nomad will
//     mark the alloc lost and the controller's reconciler will recreate.
//  6. Otherwise, build a taskHandle, spawn the WaitTask supervisor on the
//     PID, register in p.tasks, return nil.
func (p *Plugin) RecoverTask(handle *drivers.TaskHandle) error {
	if handle == nil {
		return errors.New("ch: T-4: nil handle")
	}

	if _, ok := p.tasks.Get(handle.Config.ID); ok {
		return nil
	}

	var state TaskState
	if err := handle.GetDriverState(&state); err != nil {
		return errors.New("ch: T-4: failed to decode TaskState: " + err.Error())
	}

	p.logger.Info("ch: RecoverTask stub invoked",
		"task_id", handle.Config.ID,
		"ch_pid", state.CHPid,
		"api_socket", state.APISocket,
		"vm_index", state.VMIndex,
		"mode", state.Mode)

	// T-4: stub. The real implementation reattaches; for now, refuse so
	// Nomad treats it as lost (safe default — controller recreates).
	_ = context.TODO()
	return errors.New("ch: T-4: RecoverTask not implemented")
}
