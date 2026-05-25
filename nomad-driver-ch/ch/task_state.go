// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::TaskState and
// plugin/handle.go::taskHandle) on 2026-05-25 for Cloud Hypervisor support.
// The libvirt-specific NetTeardown field is replaced by CH-specific recovery
// fields (CHPid, APISocket, Tap, VMIndex).

package ch

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/drivers"
)

// TaskState is the on-disk handle persisted via drivers.TaskHandle.SetDriverState.
// Nomad round-trips this through its client state store across restarts; on
// recovery we read it back via drivers.TaskHandle.GetDriverState and use it to
// re-attach to the running CH process.
//
// This is the orphan-CH-safety field set the volantvm spike identified as
// missing (cf. docs/reviews/nomad-driver-ch-volantvm-spike.md § 2 row 5).
// Every field here is required for RecoverTask to function (T-4).
//
// Wire-format invariant: bumping TaskHandleVersion is required when this
// struct's shape changes in a non-back-compat way.
type TaskState struct {
	// TaskConfig is the original drivers.TaskConfig as Nomad passed it to
	// StartTask. Stored verbatim so RecoverTask can rebuild the handle with
	// identical metadata.
	TaskConfig *drivers.TaskConfig

	// StartedAt is the wall-clock time StartTask returned successfully.
	StartedAt time.Time

	// CHPid is the PID of the CH process. Used by RecoverTask to verify the
	// process is still alive (`/proc/<pid>/comm == "cloud-hypervisor"`).
	CHPid int

	// APISocket is the absolute path to CH's --api-socket. Used by RecoverTask
	// to issue `ch-remote info` and confirm the VM is still responsive.
	APISocket string

	// VMIndex is the per-host VM index. Used to release the on-disk lock and
	// reconstruct the tap name for cleanup in DestroyTask.
	VMIndex uint16

	// Tap is the host-side tap device name. Captured so DestroyTask can
	// `ip tuntap del <Tap>` even after a recovery where TaskConfig may be
	// truncated.
	Tap string

	// Mode is "cold_boot" or "restore". Drives the controller-side
	// post-Running hooks (e.g. clock-resync runs only on restore).
	Mode string

	// SandboxId is the typed-id of the sandbox this VM belongs to. Captured
	// for RecoverTask + observability (so a recovered handle can re-emit
	// the sandbox-id-tagged events even when the operator's TaskConfig was
	// truncated mid-restart).
	SandboxId string

	// NomadTaskName is cfg.Name from the original TaskConfig — kept for
	// RecoverTask context (StartTask records it; RecoverTask uses it to
	// re-tag log lines so a recovered task is grepable in the same way it
	// was at first boot).
	NomadTaskName string
}

// Validate sanity-checks an unmarshalled TaskState before RecoverTask
// attempts to re-attach. Returns an error naming the first invalid field
// so a Nomad-client restart confronted with a corrupt handle surfaces
// a clear cause in the task log.
//
// The invariants enforced match the StartTask persistence contract
// (start_task.go records all of these unconditionally on the cold-boot
// path; restore-mode bookkeeping is T-6's sprint).
func (s *TaskState) Validate() error {
	if s == nil {
		return errors.New("ch: TaskState: nil state")
	}
	if s.CHPid <= 0 {
		return fmt.Errorf("ch: TaskState: CHPid must be > 0, got %d", s.CHPid)
	}
	if s.APISocket == "" {
		return errors.New("ch: TaskState: APISocket is empty")
	}
	// Tap is required: even RecoverTask handles need to know the tap so
	// DestroyTask can `ip link delete` it on cleanup. Format matches the
	// `zsbx-nm-<idx>` convention (or an operator-supplied name); we only
	// enforce non-empty here — the deeper typed-id-shape check is the
	// VMIndex range guard below.
	if s.Tap == "" {
		return errors.New("ch: TaskState: Tap is empty")
	}
	// VMIndex range matches StartTask's validateColdBoot (1..155).
	// 0 is reserved; >155 wouldn't fit the third octet (100+idx).
	if s.VMIndex < 1 || s.VMIndex > 155 {
		return fmt.Errorf("ch: TaskState: VMIndex %d out of range [1,155]", s.VMIndex)
	}
	return nil
}

// taskHandle is the in-memory runtime view of a running task. It is created
// in StartTask, looked up by every other lifecycle method, and torn down by
// DestroyTask. Unlike TaskState, it never touches disk.
type taskHandle struct {
	// stateMu syncs access to procState/exitResult/completedAt — the fields
	// that change as the task transitions running -> exited.
	stateMu sync.RWMutex

	logger hclog.Logger

	// taskConfig is Nomad's view of the task (alloc/task IDs, env, resources).
	taskConfig *drivers.TaskConfig

	// driverConfig is our decoded TaskConfig (vm_index, kernel, cmdline, ...).
	driverConfig *TaskConfig

	procState drivers.TaskState

	startedAt   time.Time
	completedAt time.Time
	exitResult  *drivers.ExitResult

	// chPid is the CH process PID. Owned for lifetime supervision in WaitTask.
	chPid int

	// apiSocket is CH's --api-socket path. Used by InspectTask, TaskStats,
	// and the graceful-stop ladder in StopTask.
	apiSocket string

	// vmIndex and tap are echoed from TaskConfig for fast cleanup access.
	vmIndex uint16
	tap     string
	mode    string

	// runner is the live processRunner StartTask attached. WaitTask blocks
	// on runner.Wait(); StopTask signals through runner.Signal(); the
	// SignalTask RPC calls runner.Signal(); StderrTail pulls from runner.
	// nil for handles produced by RecoverTask before T-4 wires up a
	// re-attached process supervisor.
	runner processRunner

	// exitDone is closed by the supervisor goroutine (superviseCH) once
	// runner.Wait returns. WaitTask subscribers select on it instead of
	// calling cmd.Wait directly (which can only fire once per Cmd).
	// nil for handles produced by RecoverTask before T-4.
	exitDone chan struct{}

	// ctx/cancelFn bound the per-task supervision goroutines (WaitTask
	// monitor, TaskStats poller). Cancelled in StopTask/DestroyTask.
	ctx      context.Context
	cancelFn context.CancelFunc
}

// TaskStatus returns the drivers.TaskStatus payload InspectTask hands to
// Nomad. Cheap; reads only in-memory state.
func (h *taskHandle) TaskStatus() *drivers.TaskStatus {
	h.stateMu.RLock()
	defer h.stateMu.RUnlock()

	return &drivers.TaskStatus{
		ID:          h.taskConfig.ID,
		Name:        h.taskConfig.Name,
		State:       h.procState,
		StartedAt:   h.startedAt,
		CompletedAt: h.completedAt,
		ExitResult:  h.exitResult.Copy(),
		DriverAttributes: map[string]string{
			"ch_pid":     itoa(h.chPid),
			"api_socket": h.apiSocket,
			"vm_index":   itoa(int(h.vmIndex)),
			"tap":        h.tap,
			"mode":       h.mode,
		},
	}
}

// IsRunning is a cheap goroutine-safe check for the running state.
func (h *taskHandle) IsRunning() bool {
	h.stateMu.RLock()
	defer h.stateMu.RUnlock()
	return h.procState == drivers.TaskStateRunning
}

// itoa is a tiny strconv-free integer formatter; kept here to avoid pulling
// strconv into the hot path of TaskStatus. Returns "0" for 0 and "-N" for
// negatives.
func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	neg := false
	if n < 0 {
		neg = true
		n = -n
	}
	var buf [20]byte
	i := len(buf)
	for n > 0 {
		i--
		buf[i] = byte('0' + n%10)
		n /= 10
	}
	if neg {
		i--
		buf[i] = '-'
	}
	return string(buf[i:])
}
