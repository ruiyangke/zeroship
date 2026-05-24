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
	"io"
	"os"
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
//
// C-7-LT-6 signature: adds sandboxID + contentAddressedRoots to drive
// the per-field disk allow-list. Pre-C-7-LT-6 callers pass "" / nil
// for both, which restricts disks[*].path to task_dir only (the prior
// strict invariant).
//
// C-7-LT-7 signature: adds userID to drive the per-user-home allow-
// list entry. Pass "" to disable that slot (mismatches a user-home
// path → rejected, just like pre-C-7-LT-7 behaviour).
//
// C-7-LT-9 signature: now returns a second value, runtimeFiles —
// the post-rewrite absolute paths of `serial.file` / `console.file`
// that the restore branch MUST pre-create before spawning CH.
// Tests that only care about the rewritten bytes can ignore the
// second return value; the new
// TestRewriteRestoreConfigPaths_RuntimeFilesCollected tests
// exercise the contract.
func RewriteConfigJSON(
	orig []byte,
	taskDir string,
	vmIndex uint16,
	subnetBaseOctet uint8,
	sandboxID, userID string,
	contentAddressedRoots []string,
) ([]byte, []string, error) {
	return rewriteConfigJSON(orig, taskDir, vmIndex, subnetBaseOctet, sandboxID, userID, contentAddressedRoots)
}

// PathFieldKind re-exports the field-kind enum so tests can exercise
// validatePathByKind directly. C-7-LT-6.
type PathFieldKindForTest = PathFieldKind

const (
	PathFieldRuntimeFileForTest = PathFieldRuntimeFile
	PathFieldDiskForTest        = PathFieldDisk
	PathFieldFsSocketForTest    = PathFieldFsSocket
)

// ValidatePathByKind is the test entry point for the per-field
// validator. C-7-LT-6 — exposed so tests can pin each allow-list
// branch without round-tripping the whole config.json.
//
// C-7-LT-7 signature: adds userID. Tests that don't care about the
// per-user-home slot should pass "".
func ValidatePathByKind(
	kind PathFieldKind,
	fieldName, value, taskDir, sandboxID, userID string,
	contentAddressedRoots []string,
) error {
	return validatePathByKind(kind, fieldName, value, value, taskDir, sandboxID, userID, contentAddressedRoots)
}

// SandboxPrefixForTest re-exports sandboxPrefix so tests can assert
// the per-sandbox layout convention without re-implementing the
// filepath join. C-7-LT-6.
func SandboxPrefixForTest(sandboxID string) string {
	return sandboxPrefix(sandboxID)
}

// UserHomePrefixForTest re-exports userHomePrefix so tests can assert
// the per-user-home layout convention without re-implementing the
// filepath join. C-7-LT-7.
func UserHomePrefixForTest(userID string) string {
	return userHomePrefix(userID)
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

// StageRootfsForRestore is the test entry point for the C-7-LT-12a
// restore-branch rootfs-stage helper. Tries `os.Link` first; falls
// back to a stdlib copy on EXDEV. Idempotent on re-attempt of a
// previously-staged dst (a re-invocation returns nil rather than
// EEXIST'ing on the copy fallback).
func StageRootfsForRestore(src, dst string) error {
	return stageRootfsForRestore(src, dst)
}

// CopyRootfsForRestoreTest unconditionally exercises the copy branch
// of stageRootfsForRestore. Used by tests that want to pin the
// copy-fallback semantics (distinct inode, byte-identical contents)
// without engineering a real EXDEV scenario in t.TempDir() (which
// would require a separate filesystem mount).
//
// Mirrors the inner copy logic in stageRootfsForRestore — kept in
// sync via a single helper extraction would be ideal, but the prod
// path's idempotency check + EXDEV detection live in the wrapper, so
// the simpler shape is a parallel test-only function.
func CopyRootfsForRestoreTest(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return err
	}
	if _, copyErr := io.Copy(out, in); copyErr != nil {
		_ = out.Close()
		_ = os.Remove(dst)
		return copyErr
	}
	return out.Close()
}

// SetHandleTapForTest mutates the in-memory taskHandle's `tap` field
// so tests can simulate the partial-init failure mode the driver v14
// defensive tap cleanup hook covers (StartTask reached SetDriverState
// but h.tap was either never assigned or cleared by a subsequent
// failure unwind). Returns an error if `taskID` isn't registered —
// signals a test setup bug rather than silently no-op'ing.
//
// Not for production use: the production lifecycle never mutates
// h.tap after the StartTask assignment line.
func SetHandleTapForTest(p *Plugin, taskID, tap string) error {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		return ErrTaskNotFound
	}
	h.tap = tap
	return nil
}

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
