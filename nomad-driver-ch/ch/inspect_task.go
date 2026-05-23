// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::InspectTask) on
// 2026-05-25 for Cloud Hypervisor support.

package ch

import (
	"github.com/hashicorp/nomad/plugins/drivers"
)

// InspectTask returns the current TaskStatus for the given task. Cheap;
// reads only in-memory state from taskHandle. Stub is functionally complete
// for the scaffold — once taskHandle is populated by StartTask (T-1), this
// becomes the right answer automatically.
func (p *Plugin) InspectTask(taskID string) (*drivers.TaskStatus, error) {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		return nil, drivers.ErrTaskNotFound
	}
	return h.TaskStatus(), nil
}
