// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::StartTask) on
// 2026-05-25 for Cloud Hypervisor support. The libvirt CreateVM/cloud-init
// path has been removed; the eventual replacement is a CH-spawn + virtiofsd
// + tap + ch-remote handshake sequence (proposal § 7 "StartTask"), stubbed
// here as T-1.

package ch

import (
	"errors"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// StartTask brings up a Cloud Hypervisor VM for the given task. Flow when
// implemented (T-1):
//
//  1. Decode driverConfig from cfg.DecodeDriverConfig.
//  2. Acquire the vm_index lock under cfg.config.VMIndexLockDir.
//  3. Set up the tap (rtnetlink or `ip(8)`) for driverConfig.Net[0].
//  4. Spawn virtiofsd × N (one per driverConfig.Fs entry); wait for sockets.
//  5. If RestoreFrom == "":
//       spawn CH with --kernel/--cmdline/--memory/--cpus/--disk*/--fs*/--net/
//       --api-socket/--serial/--console (proposal § 7);
//     else:
//       spawn CH with --restore source_url=file://<RestoreFrom> --api-socket,
//       then issue `ch-remote resume` once the API socket is ready.
//  6. Poll `ch-remote info` until vm_state ∈ {Running, Resumed} AND
//     cpu_state is past init (the B17 fix from the proposal).
//  7. Build a TaskState (CHPid/APISocket/VMIndex/Tap/Mode), persist it via
//     handle.SetDriverState.
//  8. Register the in-memory taskHandle in p.tasks and return.
//
// On any failure path, roll back in reverse order (kill CH, kill virtiofsd,
// tear tap, release vm_index lock).
func (p *Plugin) StartTask(cfg *drivers.TaskConfig) (*drivers.TaskHandle, *drivers.DriverNetwork, error) {
	if _, ok := p.tasks.Get(cfg.ID); ok {
		return nil, nil, ErrExistingTask
	}

	// Decoding the config is the one bit that's safe to do today — it
	// catches a swathe of HCL/schema errors early without requiring CH.
	// Future sprints reuse this block; we keep it here even though the rest
	// of the method short-circuits, because the test fixture exercises it.
	var driverConfig TaskConfig
	if err := cfg.DecodeDriverConfig(&driverConfig); err != nil {
		return nil, nil, errors.New("ch: T-1: failed to decode driver config: " + err.Error())
	}

	p.logger.Info("ch: StartTask stub invoked",
		"task_id", cfg.ID,
		"vm_index", driverConfig.VMIndex,
		"mode", modeOf(&driverConfig))

	return nil, nil, errors.New("ch: T-1: StartTask not implemented")
}

// modeOf returns "cold_boot" or "restore" based on whether RestoreFrom is
// set. Helper kept here because it's specific to StartTask's logging shape.
func modeOf(cfg *TaskConfig) string {
	if cfg.RestoreFrom == "" {
		return "cold_boot"
	}
	return "restore"
}
