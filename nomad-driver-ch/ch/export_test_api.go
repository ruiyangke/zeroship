// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Test-only exported names so the `tests/` package (which lives outside
// the `ch` package and therefore can't reach unexported symbols) can
// drive StartTask through the processRunner seam, exercise
// buildConfigJSON in isolation, and assert against the sentinel errors.
//
// File naming note: this is intentionally NOT `*_test.go` so the
// external test package can import these names. The compile-time
// guarantee that these are test-only is provided by the fact that they
// are documented as such and only used from the tests/ directory.

package ch

import (
	"context"
	"os/exec"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/drivers"
)

// ProcessRunnerSeam is the exported alias of processRunner, used by the
// tests/ package to define fake runners against the same contract
// StartTask consumes.
type ProcessRunnerSeam = processRunner

// BuildConfigJSON is the test entry point that exercises the cold-boot
// validation + config.json marshalling in isolation. Pure function; no
// side effects.
func BuildConfigJSON(cfg TaskConfig, taskDir string) ([]byte, error) {
	return buildConfigJSON(cfg, taskDir)
}

// BuildSpawnArgv is the test entry point for the C-1 long-argv spawn
// builder. Mirrors the wrapper's cold-boot CH spawn vector at
// nomad-vm-wrapper.sh:638-647. Pure function; no side effects.
func BuildSpawnArgv(
	chBin string,
	apiSocket string,
	cfg TaskConfig,
	cmdline string,
	disks []DiskSpec,
	net NetSpec,
	serialLog string,
) []string {
	return buildSpawnArgv(chBin, apiSocket, &cfg, cmdline, disks, net, serialLog)
}

// NewPluginForTest constructs a *Plugin with a caller-supplied runner
// factory. Mirrors NewPlugin but lets the test substitute the fake
// runner that records argv without spawning CH.
//
// Tests should still call SetConfig (or rely on the post-NewClient env
// var resolution) to wire up the binary paths.
func NewPluginForTest(logger hclog.Logger, factory func(cmd *exec.Cmd) ProcessRunnerSeam) *Plugin {
	p := NewPlugin(logger).(*Plugin)
	if factory != nil {
		p.chClient.SetRunnerFactory(func(cmd *exec.Cmd) processRunner {
			return factory(cmd)
		})
	}
	return p
}

// ErrExistingTaskErr is a re-export of ErrExistingTask under a name that
// reads naturally in errors.Is checks ("is err the existing-task err?").
// Tests can also compare against ch.ErrExistingTask directly; this is a
// belt-and-braces alias.
var ErrExistingTaskErr = ErrExistingTask

// SetEnsureTapUpForTest replaces the tap-up seam so tests can skip the
// `ip link set up` exec (which requires CAP_NET_ADMIN). Returns the
// previous fn so the test can restore it on cleanup.
func SetEnsureTapUpForTest(fn func(tapName string) error) func(string) error {
	prev := ensureTapUpFn
	if fn != nil {
		ensureTapUpFn = fn
	}
	return prev
}

// ComputeTapAddresses is the test entry point for the per-VM /30 subnet
// arithmetic in net.go. Pure function; tests can pin the layout without
// invoking StartTask or shelling to `ip`.
//
// Returns (tapName, hostIP, guestIP, subnet, err). See computeTapAddresses
// for the contract.
func ComputeTapAddresses(idx uint16, subnetBaseOctet uint8) (string, string, string, string, error) {
	return computeTapAddresses(idx, subnetBaseOctet)
}

// SetRunIPForTest swaps the `ip` exec seam in net.go so tests can drive
// realSetupTap / realTeardownTap with fake stderr shapes — exercising the
// idempotency branches without root. Returns the previous fn.
//
// The fn receives the argv (minus the leading "ip") and must return
// (combined-output, exec-error). To simulate `ip` exiting non-zero,
// return a non-nil error; the stderr-pattern matcher then inspects the
// returned bytes.
func SetRunIPForTest(fn func(args ...string) ([]byte, error)) func(...string) ([]byte, error) {
	prev := runIP
	if fn != nil {
		runIP = fn
	}
	return prev
}

// CallRealSetupTap drives the production setup path with the seam'd `ip`
// command for testing. Bypasses setupTapFn so tests can exercise
// realSetupTap's branching directly without re-implementing it.
func CallRealSetupTap(idx uint16, subnetBaseOctet uint8) (string, error) {
	return realSetupTap(idx, subnetBaseOctet)
}

// CallRealTeardownTap is the test entry point for realTeardownTap. See
// CallRealSetupTap.
func CallRealTeardownTap(tapName string) error {
	return realTeardownTap(tapName)
}

// RewriteConfigJSON is the test entry point for the T-6 restore-path
// config.json rewriter. Pure function; no side effects.
func RewriteConfigJSON(orig []byte, taskDir string, vmIndex uint16, subnetBaseOctet uint8) ([]byte, error) {
	return rewriteConfigJSON(orig, taskDir, vmIndex, subnetBaseOctet)
}

// WaitForCHSocketReady is the test entry point for the C-7-LT-3-PR1
// retrying Unix-socket readiness probe. Pure-ish: the only side
// effect is the connect attempts (no global state mutated).
//
// Tests use this in two shapes:
//
//	(1) happy path — a goroutine bind()s the socket mid-loop and
//	    the helper returns nil with attempts >= 1;
//	(2) timeout path — no listener exists and the helper returns
//	    a "not responsive within %v" error after the budget.
func WaitForCHSocketReady(sockPath string, totalBudget, perAttempt, cadence time.Duration) error {
	return waitForCHSocketReady(sockPath, totalBudget, perAttempt, cadence)
}

// ChStderrLogName re-exports the constant for the per-alloc CH
// stderr capture file. Pinned for the test suite so a future rename
// surfaces at compile time rather than at the next cluster smoke.
const ChStderrLogName = chStderrLogName

// PreflightDiskPaths is the test entry point for the C-2 pre-flight stat
// check. Pure function; no side effects. Mirrors the wrapper's existence
// guards at nomad-vm-wrapper.sh:270-277.
func PreflightDiskPaths(disks []DiskSpec) error {
	return preflightDiskPaths(disks)
}

// MaterializeRootfs is the test entry point for the C-2 rootfs-stage
// helper that mirrors the wrapper's `cp $ARTIFACT_DIR/rootfs-slim.img
// $RUNTIME/rootfs.img` at nomad-vm-wrapper.sh:300-305. Tests assert the
// happy-path copy, the idempotent re-spawn no-op, and the failure modes
// (missing source, unwritable dest).
func MaterializeRootfs(artifactDir, dstRootfs string) error {
	return materializeRootfs(artifactDir, dstRootfs)
}

// ChRootfsSourceName re-exports the wire-level constant for the source
// rootfs file name. Tests pin this so a future rename to e.g.
// "rootfs.img.zst" surfaces here at compile time rather than at the next
// cluster smoke.
const ChRootfsSourceName = chRootfsSourceName

// ChArtifactDirEnvVar re-exports the wire-level constant for the env
// var key the controller emits. Pinned for the same reason as
// ChRootfsSourceName above.
const ChArtifactDirEnvVar = chArtifactDirEnvVar

// InstallFakeRunningTaskForStats registers a synthetic taskHandle in the
// plugin's task store so a TaskStats caller can find it without needing
// to spawn a real CH process. Mirrors the minimal shape RecoverTask
// builds — exitDone is created (so the supervisor-style exit gate works)
// but no supervisor goroutine is started.
//
// Returns the synthetic exitDone channel so the test can `close(exitDone)`
// to drive the "task exited" branch of the collector loop.
func InstallFakeRunningTaskForStats(p *Plugin, taskID string, chPid int) chan struct{} {
	exitDone := make(chan struct{})
	ctx, cancel := context.WithCancel(context.Background())
	h := &taskHandle{
		taskConfig: &drivers.TaskConfig{ID: taskID, Name: taskID},
		procState:  drivers.TaskStateRunning,
		chPid:      chPid,
		exitDone:   exitDone,
		ctx:        ctx,
		cancelFn:   cancel,
	}
	p.tasks.Set(taskID, h)
	return exitDone
}
