// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::TaskStats and
// plugin/handle.go::fillStats) on 2026-05-25 for Cloud Hypervisor support.
// The libvirt domStats poller is replaced by `ch-remote info` polling
// (proposal § 7 "TaskStats").

package ch

import (
	"context"
	"errors"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// TaskStats streams a TaskResourceUsage every `interval` (Nomad default 5s)
// until ctx is cancelled. Flow when implemented (T-5):
//
//  1. Look up the taskHandle.
//  2. Spawn a goroutine that ticks at `interval`:
//     - GET /api/v1/vm.info over handle.apiSocket;
//     - Map memory.actual_size and per-vCPU stats into TaskResourceUsage;
//     - Send on the channel.
//  3. On three consecutive ch-remote failures, mark the task lost and exit.
//
// T-5 is the nice-to-have; the cluster can run without per-VM telemetry
// (the bash wrapper has none today either). Implement after T-1..T-4.
func (p *Plugin) TaskStats(ctx context.Context, taskID string, interval time.Duration) (<-chan *drivers.TaskResourceUsage, error) {
	if _, ok := p.tasks.Get(taskID); !ok {
		return nil, drivers.ErrTaskNotFound
	}

	ch := make(chan *drivers.TaskResourceUsage)
	go func() {
		defer close(ch)
		// T-5: stub. Don't emit anything; consumer will see an empty stream
		// and ctx.Done will eventually unblock them.
		<-ctx.Done()
		_ = errors.New("ch: T-5: TaskStats not implemented")
	}()
	return ch, nil
}
