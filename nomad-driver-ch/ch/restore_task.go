// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-6 sprint: wake-from-snapshot path. The Go port of the bash
// wrapper's restore branch (crates/sandbox-snapshot-restore/
// crates/sandbox/scripts/nomad-vm-wrapper.sh lines 366-419).
//
// Cold-boot lives in start_task.go; restore lives here so the two
// branches don't share a body of `if RestoreFrom == "" { … } else
// { … }` spaghetti.
//
// Wake-path shape (mirrors the wrapper line-for-line, except step 2
// is performed in-process via rewriteConfigJSON rather than shelling
// to python):
//
//   1. Validate the snapshot dir contains {state.json, config.json,
//      memory-ranges}.
//   2. Read + path-rewrite config.json (disks, serial, console, net
//      tap) → write to taskDir for diagnostic clarity.
//   3. Set up the per-VM /30 tap (same as cold-boot — restore needs
//      the host-side L2 plumbing exactly as a cold-boot does).
//   4. Spawn `cloud-hypervisor --api-socket <new-sock> --restore
//      source_url=file://<staged>` via processRunner seam.
//   5. Poll the API socket until ch-remote can talk to it (CH at
//      this point is PAUSED — --restore brings the VM back paused).
//   6. ch-remote resume — the B17 fix: brings vCPUs back to life;
//      virtio-net starts responding to ARP; tap transitions to
//      LOWER_UP.
//   7. Persist TaskState with new PID + sockets; register handle;
//      start supervisor goroutine.

package ch

import (
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
	"golang.org/x/sys/unix"
)

// Snapshot artifact file names (the controller stages these into
// $ZSBX_RESTORE_FROM before the driver runs StartTask on the restore
// branch). Pinning them as constants keeps the validate step's
// missing-file error message stable for operator-facing logs.
const (
	snapshotStateFile  = "state.json"
	snapshotConfigFile = "config.json"
	snapshotMemoryFile = "memory-ranges"
)

// chStderrLogName is the file the restore branch redirects CH's
// stderr to, under the per-alloc run dir. C-7-LT-3-PR2 (smoke-r14)
// added this so a CH that spawns and dies before its API socket
// comes up leaves a diagnostic trail an operator can read after the
// alloc's task-failure event. The file is opened with O_TRUNC on
// every spawn so a re-attempt doesn't accumulate stale lines.
//
// The cold-boot branch can adopt the same convention later; for now
// only restore writes here (cold-boot uses its --serial file to
// capture in-guest output, which is a different surface from CH's
// own stderr).
const chStderrLogName = "ch-stderr.log"

// chStderrTailBytes is the max bytes lifted from the on-disk
// stderr log into the socket-timeout error message. 4 KiB matches
// stderrCap (the in-memory tailBuffer) and is plenty to carry a
// CH panic / errno trace without bloating the Nomad event log.
const chStderrTailBytes = 4096

// Stage labels for startTaskRestoreBranch failure returns. Every
// error return in startTaskRestoreBranch tags itself with one of
// these constants — the tag surfaces in the operator-facing Nomad
// event message as `stage=<tag>` AND bumps the labelled
// `nomad_driver_ch_start_task_restore_failures_total{stage=<tag>}`
// counter. This closes the diagnostic gap T-8b-stress-r9-retry-4
// surfaced: 5 of 6 wakes failed with a generic
// `restore_backend_failed: ch: startTaskRestoreBran[truncated]`
// message that didn't say WHICH step inside the restore branch
// failed. Operators can now rate-graph failures by stage label.
//
// Naming convention: lowercase snake_case, matches the order the
// stages execute in startTaskRestoreBranch top-to-bottom. The list
// stays in sync with the error-return audit at v20 sprint planning:
// every `return …, err` inside startTaskRestoreBranch attributes to
// exactly one stage.
const (
	// stageValidateTaskConfig: nil / zero / out-of-range inputs to
	// startTaskRestoreBranch. Fires before any I/O — a spike here
	// signals a controller-side bug emitting a malformed
	// TaskConfig, NOT a worker-side failure.
	stageValidateTaskConfig = "validate_taskconfig"

	// stageValidateSnapshot: validateSnapshotDir failed (RestoreFrom
	// missing / not-a-dir / missing one of {state.json, config.json,
	// memory-ranges}). Signals the controller staged an incomplete
	// snapshot artifact dir — typically a staging-side bug (incomplete
	// rsync, GC race) rather than a CH-internal failure.
	stageValidateSnapshot = "validate_snapshot"

	// stageResolveBinary: the cloud-hypervisor binary path couldn't
	// be resolved (env var unset AND config field empty). Operator
	// misconfiguration; rare in practice once the driver plugin
	// config is wired.
	stageResolveBinary = "resolve_binary"

	// stageMkdirRundir: MkdirAll on the per-alloc run dir failed.
	// Disk-full / EACCES / EROFS on /opt/nomad. Adjacent disk
	// metrics (df, inodes) usually tell the story.
	stageMkdirRundir = "mkdir_rundir"

	// stageReadSnapshotConfig: ReadFile on <RestoreFrom>/config.json
	// failed. The validateSnapshot stage proved the file exists at
	// stat-time; a failure here means it disappeared mid-restore
	// (GC race on the staging dir, or a torn write the controller
	// hasn't fsynced).
	stageReadSnapshotConfig = "read_snapshot_config"

	// stageRewriteConfig: rewriteConfigJSON rejected the snapshot's
	// config.json (path outside allow-list, parse failure, missing
	// required field). C-7-LT-4..C-7-LT-7 invariants — operator-
	// visible because the rewriter's error names the offending
	// field + value verbatim.
	stageRewriteConfig = "rewrite_config"

	// stageWriteRewrittenConfig: WriteFile on the rewritten
	// config.json in runDir failed. Same disk-class causes as
	// stageMkdirRundir.
	stageWriteRewrittenConfig = "write_rewritten_config"

	// stageSymlinkSnapshotArtifact: os.Symlink failed when wiring
	// state.json / memory-ranges from RestoreFrom into runDir.
	// EPERM on a noexec mount, or a stale fs-cache. C-7-LT-10.
	stageSymlinkSnapshotArtifact = "symlink_snapshot_artifact"

	// stagePrewarmMemoryRanges: best-effort posix_fadvise(FADV_WILLNEED)
	// hint on the memory-ranges artifact after symlinks resolve but
	// before CH `--restore` spawns. Tells the kernel to populate the
	// page cache asynchronously so CH's mmap+page-fault-in sequence
	// (typically 35-43s cold) benefits from warm-cache reads. The
	// stage is non-blocking: fadvise returns immediately and the kernel
	// does the I/O in parallel with CH startup. Errors here do NOT fail
	// the wake — log WARN and continue. T-9-perf-prewarm.
	stagePrewarmMemoryRanges = "prewarm_memory_ranges"

	// stageRootfsSourceMissing: RootfsSource was empty OR the path
	// it pointed at couldn't be stat'd. The controller MUST emit a
	// valid Config.rootfs_source on every restore alloc — a failure
	// here is a controller contract bug, NOT a worker-side issue.
	// C-7-LT-12a.
	stageRootfsSourceMissing = "rootfs_source_missing"

	// stageStageRootfs: stageRootfsForRestore failed materialising
	// the rootfs into runDir. Reflink (FICLONE) + copy fallback both
	// errored — disk-full / permissions on the runtime_dir / a
	// genuinely unreadable src. v22 supersedes the v21 hardlink
	// shape: each wake now gets a UNIQUE inode (reflink CoW, or a
	// distinct-inode byte copy on non-reflink filesystems).
	stageStageRootfs = "stage_rootfs"

	// stageWakeRootfsLockWait: after the rootfs is materialised into
	// runDir but BEFORE CH `--restore` spawns, the driver polls
	// F_OFD_SETLK acquire on the rootfs path as defense-in-depth.
	//
	// Driver v22: stageRootfsForRestore now produces a UNIQUE inode
	// per wake alloc (reflink/FICLONE on ext4/xfs/btrfs, plain copy
	// on non-reflink filesystems), so the wake's rootfs.img no
	// longer shares inode-keyed OFD lock state with the source
	// template. Under normal operation this probe acquires the
	// lock immediately and is a no-op — its failure path now means
	// the dst filesystem is non-reflink-capable AND something
	// outside this driver is locking the dst inode (e.g. a
	// re-entrant restore attempt on the same runDir, an external
	// tool holding the file).
	//
	// Driver v21 origin (kept here for context): pre-v22 the wake
	// hardlinked `rootfs.img` from the source alloc's runDir.
	// Hardlinks share inode; the kernel's POSIX/OFD lock table is
	// keyed by inode. Under c>=2 wakes the second wake EAGAIN'd on
	// F_OFD_SETLK F_WRLCK because the first wake's CH still held
	// the lock for the VM lifetime. T-8b-stress-r9-retry-6 c=4
	// saw 8/8 CREATE + 8/8 SNAPSHOT but 0/8 WAKE on this exact
	// mechanism. v22's unique-inode-per-wake fixes the root cause;
	// this probe remains as a low-cost belt-and-braces check.
	//
	// Budget: 50 × 100ms = 5s wall. Same shape as r5-A's destroy-
	// side probe; finer cadence (100ms vs 200ms) because under v22
	// the acquire is expected to succeed on the first attempt.
	stageWakeRootfsLockWait = "wake_rootfs_lock_wait"

	// stagePrecreateRuntimeFile: pre-creation of serial.file /
	// console.file in runDir failed (CH `--restore` opens these
	// without O_CREAT; pre-creation is required, see C-7-LT-9).
	// Same disk-class causes as stageMkdirRundir.
	stagePrecreateRuntimeFile = "precreate_runtime_file"

	// stageTapSetup: ensureTapUp / setupTapForVM failed. Kernel-
	// side netdev wedge, or a previously-leaked tap on the same
	// VMIndex (the r24-A2-S2 wedge stress-r8 closed via netlink-
	// verified DestroyTask, but a residual race can still surface
	// here on a contended host).
	stageTapSetup = "tap_setup"

	// stageRestoreSpawn: exec.Cmd.Start() returned an error.
	// CH binary missing at exec time, ENOMEM, ulimit, AppArmor /
	// SELinux denial, or fork/exec races. CH stderr is meaningful
	// here ONLY if exec made it to the child process before
	// failing — most often the spawn errors before any stderr is
	// written.
	stageRestoreSpawn = "restore_spawn"

	// stageLivezProbe: the API-socket readiness probe
	// (waitForCHSocketReady wrapped by pollAPISocketFn) exhausted
	// its budget without observing CH bind the socket. Signals CH
	// is alive but slow to deserialise the memory image, or CH
	// crashed during deserialisation (in which case the captured
	// stderr tail carries the panic / errno trace). The labelling
	// here matches the smoke-r14 "ch-remote ping" diagnostic from
	// the pre-C-7-LT-3 wrapper era.
	stageLivezProbe = "livez_probe"

	// stageResume: ch-remote resume (vm.resume RPC) failed.
	// CH bound the socket but refused the resume — historical
	// shapes include HTTP 500 "VM is not running" (smoke-r21) and
	// HTTP 500 "VM Restore failed: DeviceManager(Disk(NotFound))"
	// (smoke-r22). C-7-LT-11 + C-7-LT-12a closed those specific
	// shapes; a fresh failure here points at a new CH-internal
	// surface visible only via the captured stderr tail.
	stageResume = "resume"

	// stagePersistState: handle.SetDriverState failed serialising
	// the new TaskState. nomad/plugins/drivers internal — extremely
	// rare; usually signals a schema-evolution bug.
	stagePersistState = "persist_state"
)

// restoreErrorf wraps an error return for startTaskRestoreBranch
// with the stage label + bumps the per-stage failure counter. The
// returned error always carries `stage=<stage>` so the operator-
// facing Nomad event message (which the controller may truncate at
// the wake_jobs error_message column or a 180-char display budget)
// always names the failing stage near the start of the wrapped
// message.
//
// The wrapped error preserves `%w` semantics so errors.Is / errors.As
// still inspect the underlying cause.
//
// Naming note: detail goes BEFORE the optional stderr tail so a
// truncated display still surfaces the stage + cause; the tail is
// the tail of the message intentionally.
func restoreErrorf(stage string, format string, args ...any) error {
	incStartTaskRestoreFailures(stage)
	return fmt.Errorf("ch: startTaskRestoreBranch: stage=%s: "+format, append([]any{stage}, args...)...)
}

// restoreErrorWithStderr is the variant for stages where CH was
// already spawned and may have written to its stderr log. Appends
// `ch_stderr_tail=<tail>` after the cause + records the on-disk
// path so an operator can `cat` it for the full content if the
// 4 KiB tail wasn't enough.
//
// Behaves identically to restoreErrorf when tail is empty (drops
// the tail clause, keeps the path clause for triage continuity).
func restoreErrorWithStderr(stage string, stderrPath string, tail []byte, format string, args ...any) error {
	incStartTaskRestoreFailures(stage)
	if len(tail) > 0 {
		return fmt.Errorf(
			"ch: startTaskRestoreBranch: stage=%s: "+format+"; ch_stderr_tail=%q (path=%s)",
			append(append([]any{stage}, args...), string(tail), stderrPath)...,
		)
	}
	return fmt.Errorf(
		"ch: startTaskRestoreBranch: stage=%s: "+format+" (no ch stderr captured; path=%s)",
		append(append([]any{stage}, args...), stderrPath)...,
	)
}

// defaultAPISocketPollTimeout bounds the time we wait for CH's
// --api-socket to become responsive after --restore. C-7-LT-3
// (smoke-r14, 2026-05-25) widened this from 10s → 60s after a
// cluster wake observed the socket fail to accept within the prior
// 10s budget. The bash wrapper's 50 × 200ms = 10s budget assumed
// cold-boot timing; --restore's memory-image mmap + page-fault-in
// can take materially longer on a GCE n2-standard-4 host. The
// retrying connect loop (waitForCHSocketReady) cheaply tolerates
// the longer ceiling — first-success returns immediately, so the
// happy path is unchanged. Exposed as a var (not const) so tests
// can shorten it.
var defaultAPISocketPollTimeout = 60 * time.Second

// defaultAPISocketPollInterval is how often the readiness probe
// retries a Unix-socket connect. 100ms matches the host-fence probe
// rhythm (compio-side C-7-LT-2-PR1 in sandbox-snapshot-restore) so
// the two readiness shapes stay symmetric.
var defaultAPISocketPollInterval = 100 * time.Millisecond

// defaultAPISocketPollPerAttempt is the per-Dial timeout inside the
// probe loop. Short (200ms) so a hung Dial doesn't dominate the
// retry cadence; the readiness signal we want is "accept succeeds
// quickly" — a slow accept implies CH still booting and we'd
// rather retry than block.
var defaultAPISocketPollPerAttempt = 200 * time.Millisecond

// SetAPISocketPollForTest shortens both the poll timeout and the
// poll interval so the restore tests don't sleep real seconds.
// Returns the previous (timeout, interval) pair so the test can
// restore them on cleanup.
func SetAPISocketPollForTest(timeout, interval time.Duration) (time.Duration, time.Duration) {
	prevT := defaultAPISocketPollTimeout
	prevI := defaultAPISocketPollInterval
	if timeout > 0 {
		defaultAPISocketPollTimeout = timeout
	}
	if interval > 0 {
		defaultAPISocketPollInterval = interval
	}
	return prevT, prevI
}

// T-8b-stress-r9-retry-6 (driver v21): wake-side OFD-lock-probe
// budget for `stage_wake_rootfs_lock_wait`. After the rootfs is
// hardlinked into runDir but before CH `--restore` spawns, the driver
// must wait for the source alloc's CH process to release its OFD
// write lock on rootfs.img — otherwise CH errors at restore time
// with `Can't get Write lock ... as there is already a ExclusiveWrite
// lock` and the resume RPC returns HTTP 500.
//
// Budget: 50 × 100ms = 5s wall. Same total ceiling as the destroy-
// side r5-A probe (`destroyLockPollAttempts × destroyLockPollInterval`
// in stop_task.go), with a finer cadence (100ms vs 200ms) because
// the source-side `__fput` typically completes sub-100ms once the
// source destroy returns — finer cadence picks the lock up sooner
// without burning extra syscalls (each F_OFD_SETLK is ~10 µs).
//
// Var (not const) so SetWakeRootfsLockWaitForTest can shorten them
// for the test suite.
var (
	wakeRootfsLockWaitAttempts = 50
	wakeRootfsLockWaitInterval = 100 * time.Millisecond
)

// SetWakeRootfsLockWaitForTest overrides the wake-side OFD-lock-probe
// poll budget so tests don't sleep 5s. Returns the previous
// (attempts, interval) pair so the caller can restore them on
// cleanup. Mirrors SetDestroyLockPollForTest in stop_task.go.
func SetWakeRootfsLockWaitForTest(attempts int, interval time.Duration) (int, time.Duration) {
	prevA := wakeRootfsLockWaitAttempts
	prevI := wakeRootfsLockWaitInterval
	wakeRootfsLockWaitAttempts = attempts
	wakeRootfsLockWaitInterval = interval
	return prevA, prevI
}

// pollWaitForRootfsLockReleased polls F_OFD_SETLK acquire on the
// wake-path rootfs.img until the source alloc's CH process has
// released its OFD write lock, or the budget exhausts.
//
// Returns nil on observed acquire (or file-gone — same semantics as
// the destroy-side `pollAcquireOFDLock`: if the file isn't there,
// no lock is possible, treat as success). Returns a descriptive
// error on budget exhaustion or on an unexpected syscall error
// (anything other than EAGAIN/EACCES from F_OFD_SETLK).
//
// Reuses the package-shared `tryAcquireOFDLockFn` seam in
// stop_task.go so tests drive both paths with the same canned-
// outcome stub; only the budget (50 × 100ms vs the destroy-side's
// 25 × 200ms) and the sleep seam (sleepForWakeRootfsLockPoll, this
// file) differ. Per the v21 mandate's r5-A-pattern note: "check if
// it can be reused / generalized" — yes, the syscall-touching helper
// is shared; only the loop bound + counter differ.
//
// On budget exhaustion the caller (startTaskRestoreBranch) bumps
// `nomad_driver_ch_wake_rootfs_lock_held_total` AND
// `nomad_driver_ch_start_task_restore_failures_total{stage=
// "wake_rootfs_lock_wait"}` (the latter via the standard
// restoreErrorf path), then returns the error to Nomad. Unlike the
// destroy-side r5-A (which proceeds on budget exhaust to avoid a
// destroy-loop), the wake side MUST surface the error: spawning CH
// anyway would just hit the original `AlreadyLocked` failure with
// no diagnostic improvement. The 5s wait + clear stage-labelled
// error is strictly better than the v20 generic `stage=resume`
// failure.
func pollWaitForRootfsLockReleased(path string) error {
	if path == "" {
		return errors.New("ch: pollWaitForRootfsLockReleased: empty path")
	}
	attempts := wakeRootfsLockWaitAttempts
	interval := wakeRootfsLockWaitInterval
	for attempt := 0; attempt < attempts; attempt++ {
		result, err := tryAcquireOFDLockFn(path)
		switch result {
		case ofdLockProbeAcquired:
			// Source-side `__fput` ran (or never had to). We held the
			// lock briefly and released it — the next CH `--restore`
			// spawn will acquire it cleanly. If `err != nil` it's an
			// unlock-failed edge case the defer close already handles
			// (and the destroy-side pollAcquireOFDLock treats it as
			// success too — see realTryAcquireOFDLock comments).
			return nil
		case ofdLockProbeFileGone:
			// File doesn't exist — no lock possible. This should be
			// impossible on the wake path (stageRootfsForRestore JUST
			// created the file two lines above the caller) but we
			// treat it as success for symmetry with the destroy-side
			// helper: the lock state we cared about is moot.
			return nil
		case ofdLockProbeError:
			return fmt.Errorf("probe error on attempt %d: %w", attempt+1, err)
		case ofdLockProbeBusy:
			// Expected during the source-side deferred-__fput window.
			// Sleep + retry.
		}
		if attempt+1 < attempts {
			sleepForWakeRootfsLockPoll(interval)
		}
	}
	return fmt.Errorf("rootfs.img lock acquisition budget exhausted (%dx%v); source alloc's CH process has not released its OFD write lock",
		attempts, interval)
}

// sleepForWakeRootfsLockPoll is the package-level seam tests swap so
// the wake-path OFD-lock probe loop doesn't add real wall time.
// Default is time.Sleep — production callers block while the
// source-side kernel `__fput` workqueue catches up.
//
// Distinct from `sleepForOFDLockPoll` in stop_task.go so the two
// poll loops can be driven independently from tests (a single shared
// seam would bleed test state between the destroy-side and wake-side
// test suites). Mirrors the per-loop seam pattern v15 r3-B and r4-A
// already established.
var sleepForWakeRootfsLockPoll = func(d time.Duration) {
	time.Sleep(d)
}

// SetSleepForWakeRootfsLockPollForTest swaps the wake-path OFD-lock
// sleep seam. Returns the previous fn so the caller can restore it
// on cleanup. Tests typically install a no-op so the poll loop spins
// through its budget instantly rather than waiting real wall time.
func SetSleepForWakeRootfsLockPollForTest(fn func(d time.Duration)) func(d time.Duration) {
	prev := sleepForWakeRootfsLockPoll
	if fn != nil {
		sleepForWakeRootfsLockPoll = fn
	}
	return prev
}

// pollAPISocketFn is the seam tests swap to skip the real poll. The
// default impl waits for the socket file to appear + dials ch-remote
// ping (when ch-remote is resolvable), returning nil on success and
// a timeout error otherwise.
var pollAPISocketFn = pollAPISocketDefault

// SetPollAPISocketForTest replaces the poll-API-socket seam. Returns
// the previous fn so the caller can restore it on cleanup.
func SetPollAPISocketForTest(fn func(c *Client, socketPath string, timeout, interval time.Duration) error) func(*Client, string, time.Duration, time.Duration) error {
	prev := pollAPISocketFn
	if fn != nil {
		pollAPISocketFn = fn
	}
	return prev
}

// pollAPISocketDefault is the production poller. Delegates to
// waitForCHSocketReady, which probes the Unix socket with a
// retrying `net.DialTimeout("unix", …)` loop until either a connect
// succeeds (CH is ready) or the budget expires.
//
// C-7-LT-3 (smoke-r14, 2026-05-25) replaced the prior "stat the
// socket file + shell out to ch-remote ping" implementation. Two
// problems with the old shape:
//
//  1. Budget was 10s — too tight for --restore under prod load.
//     Widened to 60s here (defaultAPISocketPollTimeout) per the
//     review's recommendation; the retrying connect loop makes
//     the wider ceiling cheap because first-success returns
//     immediately.
//  2. ch-remote ping fork/execs on every retry attempt — 50
//     fork/execs in 10s is wasteful and would compound on a
//     contended host. A direct Unix-socket connect probes the
//     exact readiness signal we care about (CH bound + accepting)
//     with no per-attempt process spawn.
//
// The `c *Client` argument is retained for signature compatibility
// with the seam (tests swap pollAPISocketFn and need a stable
// shape); the new implementation doesn't consume it.
func pollAPISocketDefault(c *Client, socketPath string, timeout, interval time.Duration) error {
	if timeout <= 0 {
		timeout = defaultAPISocketPollTimeout
	}
	if interval <= 0 {
		interval = defaultAPISocketPollInterval
	}
	return waitForCHSocketReady(socketPath, timeout, defaultAPISocketPollPerAttempt, interval)
}

// waitForCHSocketReady probes a Unix-domain socket path with a
// retrying `net.DialTimeout` loop and returns nil on first
// successful connect, or a timeout error including attempt count
// + lastErr if the total budget expires.
//
// Mirrors C-7-LT-2-PR1's compio-native pattern on the
// sandbox-snapshot-restore worktree (the controller's host-fence
// TCP probe).
//
// Arguments:
//   - sockPath:    absolute path to the Unix-domain socket file CH
//                  binds to via --api-socket.
//   - totalBudget: outer deadline for the whole loop. The fn
//                  returns no later than this duration after the
//                  first attempt unless a successful Dial returns
//                  earlier. Caller passes the wider 60s default
//                  (see defaultAPISocketPollTimeout) on the restore
//                  path; tests pass shorter to keep CI snappy.
//   - perAttempt:  per-Dial timeout. Short (200ms) so a hung Dial
//                  doesn't dominate the retry cadence.
//   - cadence:     sleep between attempts. 100ms matches the
//                  host-fence probe rhythm.
//
// Edge cases:
//   - Empty sockPath returns an immediate error (defensive — the
//     restore branch never passes an empty path, but the helper is
//     pure-fn callable from tests).
//   - perAttempt <= 0 → defaultAPISocketPollPerAttempt.
//   - cadence <= 0    → defaultAPISocketPollInterval.
//   - totalBudget <=0 → immediate timeout (no attempts).
//
// First-success: returns nil the moment any Dial succeeds; does
// NOT consume the remaining budget once readiness is observed.
func waitForCHSocketReady(sockPath string, totalBudget, perAttempt, cadence time.Duration) error {
	if sockPath == "" {
		return errors.New("ch: waitForCHSocketReady: empty socket path")
	}
	if perAttempt <= 0 {
		perAttempt = defaultAPISocketPollPerAttempt
	}
	if cadence <= 0 {
		cadence = defaultAPISocketPollInterval
	}
	deadline := time.Now().Add(totalBudget)
	var lastErr error
	attempts := 0
	for time.Now().Before(deadline) {
		attempts++
		conn, err := net.DialTimeout("unix", sockPath, perAttempt)
		if err == nil {
			_ = conn.Close()
			return nil
		}
		lastErr = err
		// Don't oversleep past the deadline — keeps the error path
		// reporting an attempts count that reflects what we
		// actually tried rather than padding with a wasted sleep.
		if time.Until(deadline) <= cadence {
			break
		}
		time.Sleep(cadence)
	}
	return fmt.Errorf("ch: api socket not responsive at %s within %v (attempts=%d, lastErr=%v)", sockPath, totalBudget, attempts, lastErr)
}

// resumeFn is the seam tests swap to drive the resume step's
// outcomes without spawning a real ch-remote. Default delegates to
// Client.Resume.
var resumeFn = func(c *Client, socketPath string) error {
	return c.Resume(socketPath)
}

// SetResumeForTest replaces the resume seam. Returns the previous fn
// so the caller can restore it on cleanup.
func SetResumeForTest(fn func(c *Client, socketPath string) error) func(*Client, string) error {
	prev := resumeFn
	if fn != nil {
		resumeFn = fn
	}
	return prev
}

// stageRootfsForRestore materialises the source rootfs at the
// destination path with a UNIQUE inode per wake alloc. Tries
// `ioctl(FICLONE)` first (reflink — O(1) block-level CoW on
// ext4/xfs/btrfs); falls back to a stdlib byte copy when the kernel
// returns EOPNOTSUPP / EXDEV / EINVAL (filesystem without reflink
// support, or src and dst on separate filesystems / different
// mounts).
//
// Why NOT hardlink (driver v22, supersedes v21's hardlink path):
//
// The previous shape used `os.Link(src, dst)` so the wake alloc's
// `rootfs.img` shared an inode with the source alloc's template
// `rootfs-slim.img`. Cloud-Hypervisor's `--restore` opens the disk
// with an OFD F_WRLCK (`fcntl(F_OFD_SETLK)`). The kernel's POSIX +
// OFD lock table is keyed by **inode**, not by file path or open
// fd — so under c>=2 concurrent wakes that all hardlink the same
// template inode, only ONE wake can acquire the exclusive write
// lock. Subsequent wakes get EAGAIN ("already locked") for the
// ENTIRE lifetime of the first VM, because CH holds the lock until
// shutdown. The v21 wake_rootfs_lock_wait probe is unable to
// resolve this — the lock never releases during the budget window —
// so the budget exhausts and the wake fails with stage=
// wake_rootfs_lock_wait. T-8b-stress-r9-retry-6 saw 0/8 WAKE at
// c=4 on this exact mechanism (architecture review r30-A2).
//
// Reflink (FICLONE) gives each wake a **distinct inode** that
// shares physical blocks via CoW — no shared lock state, no
// per-inode lock contention. The plain-copy fallback also produces
// a distinct inode (at higher wall-time cost, ~200ms per 200MB on
// SSD), so the unique-inode invariant holds even on non-reflink-
// capable filesystems (e.g. tmpfs in test). The v21 lock-wait
// probe stays as defense-in-depth: under normal operation with
// reflink/copy it observes the lock-acquirable state immediately
// and is a no-op; if it ever fires under v22, the filesystem is
// non-reflink-capable AND something outside this driver is locking
// the inode.
//
// Idempotent: if the destination already exists, returns nil — a
// re-attempt of a previously-failed restore should not error here.
// (The rewriter validates the destination's path; the file's mere
// presence is the invariant CH cares about.)
//
// Permissions: 0o600 (rw owner only), matching materializeRootfs
// on the cold-boot path. Reflink inherits the dst's mode (which
// we set via OpenFile); plain-copy uses the same explicit mode.
func stageRootfsForRestore(src, dst string) error {
	if src == "" {
		return errors.New("stageRootfsForRestore: empty src")
	}
	if dst == "" {
		return errors.New("stageRootfsForRestore: empty dst")
	}
	// Idempotency / symlink-replacement (v23 fix):
	//
	// A prior pipeline stage (symlink_snapshot_artifact) may have left a
	// SYMLINK at dst pointing at the shared host template
	// (/etc/zeroship/rootfs-slim.img).  os.Stat follows symlinks, so the
	// v22 check `if os.Stat(dst) == nil { return nil }` returned early and
	// the FICLONE+copy NEVER ran — all wake allocs dereferenced the same
	// template inode, causing OFD write-lock collisions (wake_rootfs_lock_held_total = 8).
	//
	// Fix: use os.Lstat (does NOT follow symlinks) to detect whatever is at
	// dst.  If anything exists — symlink, regular file, or other — remove it
	// first, then proceed to COW.  Exception: a regular file with nlink == 1
	// is a previously-staged unique-inode copy; return nil for true idempotency.
	if fi, err := os.Lstat(dst); err == nil {
		// dst exists.  Check shape for true idempotency.
		if fi.Mode().IsRegular() {
			if st, ok := fi.Sys().(*syscall.Stat_t); ok && st.Nlink == 1 {
				// Already a unique-inode regular file from a prior COW — idempotent.
				return nil
			}
		}
		// dst is a symlink (from symlink_snapshot_artifact), a hardlink
		// (nlink > 1 regular file sharing the template inode), or something
		// else unexpected.  Remove it so the FICLONE+copy path below creates
		// a fresh unique inode.
		if rmErr := os.Remove(dst); rmErr != nil {
			return fmt.Errorf("remove pre-existing dst %s (mode=%s): %w", dst, fi.Mode(), rmErr)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("lstat dst %s: %w", dst, err)
	}
	in, err := os.Open(src)
	if err != nil {
		return fmt.Errorf("open src %s: %w", src, err)
	}
	defer in.Close()
	// Create dst with O_EXCL — partner allocs racing the same wake
	// must not silently overwrite each other. The reflink ioctl
	// REPLACES dst's contents in place, so the dst file must exist
	// and be open for write before FICLONE.
	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return fmt.Errorf("create dst %s: %w", dst, err)
	}
	// Try reflink first. On success the dst inode is unique (a
	// fresh inode allocated by the O_EXCL open above) but shares
	// physical blocks with src via the filesystem's CoW machinery —
	// O(1) regardless of image size, no inode-keyed lock collision
	// with src or any other wake's reflink of the same template.
	if reflinkErr := reflinkFileFn(out, in); reflinkErr == nil {
		if closeErr := out.Close(); closeErr != nil {
			_ = os.Remove(dst)
			return fmt.Errorf("close dst %s (reflink): %w", dst, closeErr)
		}
		return nil
	}
	// Reflink failed. Fall back to a plain byte copy. The error
	// classes we recover from here are filesystem-capability gaps
	// (EOPNOTSUPP / ENOTSUP on tmpfs, ext2, vfat) and cross-device
	// (EXDEV when src and dst are on different mounts). We do NOT
	// distinguish — any reflink failure is treated as "fall back",
	// since the byte copy is the strictly safer shape and the
	// wall-time cost (~200ms per 200MB on SSD) is acceptable on
	// filesystems where reflink isn't available.
	if _, copyErr := io.Copy(out, in); copyErr != nil {
		_ = out.Close()
		_ = os.Remove(dst)
		return fmt.Errorf("copy %s -> %s (reflink fallback): %w", src, dst, copyErr)
	}
	if closeErr := out.Close(); closeErr != nil {
		_ = os.Remove(dst)
		return fmt.Errorf("close dst %s (copy): %w", dst, closeErr)
	}
	return nil
}

// reflinkFileFn is the seam tests swap to drive the FICLONE
// failure path without engineering a non-reflink filesystem in
// t.TempDir(). Default impl wraps unix.IoctlFileClone (FICLONE) —
// the Linux ioctl for block-level copy-on-write between two open
// file descriptors. Returns nil on success, the kernel errno (as
// returned by ioctl) on failure.
//
// Tests install a stub returning a sentinel error to force the
// copy-fallback branch.
var reflinkFileFn = realReflinkFile

// realReflinkFile is the production reflink implementation.
// Delegates to unix.IoctlFileClone(destFd, srcFd), which performs
// the FICLONE ioctl on Linux 4.5+. On filesystems without reflink
// support (tmpfs, ext2, vfat), the kernel returns EOPNOTSUPP /
// ENOTSUP and stageRootfsForRestore falls back to a byte copy.
func realReflinkFile(dst, src *os.File) error {
	return unix.IoctlFileClone(int(dst.Fd()), int(src.Fd()))
}

// SetReflinkFileForTest swaps the FICLONE seam. Returns the
// previous fn so callers can restore it on cleanup. Tests
// typically install a stub that returns syscall.EOPNOTSUPP to
// pin the copy-fallback contract regardless of the underlying
// filesystem's reflink capability.
func SetReflinkFileForTest(fn func(dst, src *os.File) error) func(dst, src *os.File) error {
	prev := reflinkFileFn
	if fn != nil {
		reflinkFileFn = fn
	}
	return prev
}

// prewarmFileFn is the package-level seam tests swap to drive prewarm
// outcomes without touching real files or syscalls. The default
// implementation calls prewarmFile. Tests install a stub to exercise
// the "missing file → WARN + continue" and "success → counter
// increments" paths. T-9-perf-prewarm.
var prewarmFileFn = prewarmFile

// SetPrewarmFileFnForTest swaps the prewarm seam. Returns the previous
// fn so the caller can restore it on cleanup.
func SetPrewarmFileFnForTest(fn func(path string) error) func(string) error {
	prev := prewarmFileFn
	if fn != nil {
		prewarmFileFn = fn
	}
	return prev
}

// prewarmFile issues posix_fadvise(2) POSIX_FADV_WILLNEED on the
// named file, hinting the kernel to populate the page cache
// asynchronously. This is non-blocking: the syscall returns
// immediately and the kernel performs readahead in the background
// while CH spawns. On kernels or filesystems that ignore
// FADV_WILLNEED the syscall still succeeds (POSIX: "advisory only").
//
// Errors are expected to be transient (ENOENT if the symlink target
// disappeared between the symlink stage and here, EPERM on exotic
// seccomp profiles). Callers should WARN-log and continue — prewarm
// is an optimisation, not a correctness requirement.
func prewarmFile(path string) error {
	f, err := os.Open(path)
	if err != nil {
		return err
	}
	defer f.Close()
	fi, err := f.Stat()
	if err != nil {
		return err
	}
	size := fi.Size()
	if size <= 0 {
		return nil
	}
	if err := unix.Fadvise(int(f.Fd()), 0, size, unix.FADV_WILLNEED); err != nil {
		return err
	}
	incPrewarmMemoryRangesBytes(size)
	return nil
}

// startTaskRestoreBranch is the wake-from-snapshot StartTask flow.
// Invoked from StartTask when cfg.RestoreFrom != "" (see start_task.go).
// Mirrors the bash wrapper's restore branch line-for-line; see
// file-level comment for the step list.
//
// Returns the same (TaskHandle, DriverNetwork, error) tuple StartTask
// itself returns so the dispatch site is a simple delegation.
//
// On any failure path the partially-spawned CH is best-effort killed
// to avoid orphaning a paused VM; the run dir + on-disk state are
// left for DestroyTask to scrub (this matches the cold-boot
// roll-back model).
func (p *Plugin) startTaskRestoreBranch(cfg *drivers.TaskConfig, driverConfig *TaskConfig) (*drivers.TaskHandle, *drivers.DriverNetwork, error) {
	if cfg == nil {
		return nil, nil, restoreErrorf(stageValidateTaskConfig, "nil TaskConfig")
	}
	if driverConfig == nil {
		return nil, nil, restoreErrorf(stageValidateTaskConfig, "nil driverConfig")
	}
	if driverConfig.RestoreFrom == "" {
		return nil, nil, restoreErrorf(stageValidateTaskConfig, "RestoreFrom is empty")
	}

	// VMIndex is the one cold-boot validation that DOES apply on the
	// restore branch: the new tap name is derived from it. Other
	// cold-boot guards (sandbox_id, workspace_img, pubkey) are NOT
	// applied — the snapshot already carries that material in its
	// memory image, and the controller wakes a snapshot WITHOUT
	// re-supplying those fields.
	if driverConfig.VMIndex < 1 || driverConfig.VMIndex > 155 {
		return nil, nil, restoreErrorf(stageValidateTaskConfig, "vm_index %d out of range [1,155]", driverConfig.VMIndex)
	}
	if driverConfig.SubnetBaseOctet > 255 {
		return nil, nil, restoreErrorf(stageValidateTaskConfig, "subnet_base_octet %d out of u8 range", driverConfig.SubnetBaseOctet)
	}

	mode := "restore"
	p.logger.Info("ch: StartTask (restore branch)",
		"task_id", cfg.ID,
		"task_name", cfg.Name,
		"vm_index", driverConfig.VMIndex,
		"restore_from", driverConfig.RestoreFrom,
		"mode", mode)

	// Step 1: validate the staged snapshot dir.
	if err := validateSnapshotDir(driverConfig.RestoreFrom); err != nil {
		return nil, nil, restoreErrorf(stageValidateSnapshot, "%v (restore_from=%s)", err, driverConfig.RestoreFrom)
	}

	chBin := p.chClient.CHBin()
	if chBin == "" {
		// C-7-LT-5: pass ChRemoteBin (not VirtiofsdBin) — see Config
		// struct doc. Pre-fix this silently aliased c.chRemoteBin to
		// virtiofsd, breaking the StopTask shutdown step.
		if p.config != nil {
			p.chClient.SetBinaries(p.config.CloudHypervisorBin, p.config.ChRemoteBin)
			chBin = p.chClient.CHBin()
		}
	}
	if chBin == "" {
		return nil, nil, restoreErrorf(stageResolveBinary, "cloud-hypervisor binary not found (set ZSBX_CH_BIN or config.cloud_hypervisor_bin)")
	}

	runDir := taskRunDir(cfg, p.config)
	if err := os.MkdirAll(runDir, 0o755); err != nil {
		return nil, nil, restoreErrorf(stageMkdirRundir, "mkdir runDir %s: %v", runDir, err)
	}

	apiSocket := filepath.Join(runDir, chAPISocketName)
	rewrittenConfigPath := filepath.Join(runDir, chConfigName)
	// Stale socket cleanup, matching the wrapper's `rm -f "$API_SOCK"`.
	_ = os.Remove(apiSocket)

	// Step 2: read + path-rewrite config.json. Materialise the
	// rewritten copy in the run dir (operator-facing trail of "what
	// did the restore actually feed CH"). The bash wrapper rewrites
	// in-place in $ZSBX_RESTORE_FROM/config.json; we DO NOT do that
	// because (a) the source dir is potentially read-only and (b)
	// re-wakes of the same snapshot should each see a pristine
	// source — the rewrite is idempotent across attempts but
	// touching the staged dir is a smell.
	//
	// C-7-LT-10 (smoke-r20): runDir is now the source-of-truth dir we
	// hand CH via --restore source_url=file://<runDir>. The rewritten
	// config.json lives in runDir; state.json and memory-ranges
	// (immutable artifacts CH consumes verbatim) are symlinked from
	// <RestoreFrom> into runDir below so CH sees all three files
	// under one directory. The read-only source dir is referenced
	// only through the symlinks — we still never write into it.
	// Pre-fix the rewritten config never reached CH because the
	// invocation pointed at <RestoreFrom> where the un-rewritten
	// config.json still lived — three smoke cycles (r15/r19/r20) all
	// failed at CreateConsoleDevice ENOENT before this was caught.
	snapshotConfigPath := filepath.Join(driverConfig.RestoreFrom, snapshotConfigFile)
	origConfig, err := os.ReadFile(snapshotConfigPath)
	if err != nil {
		return nil, nil, restoreErrorf(stageReadSnapshotConfig, "read snapshot config %s: %v", snapshotConfigPath, err)
	}
	base := uint8(driverConfig.SubnetBaseOctet)
	if base == 0 {
		base = defaultSubnetBaseOctet
	}
	// C-7-LT-6: per-field path allow-list. The rewriter MUST know
	// the current sandbox_id so disks[*].path under
	// /var/zeroship/ch/<sbx>/ (the per-sandbox persistent workspace)
	// is accepted; and it MAY accept disks under any operator-
	// configured content-addressed root (e.g. read-only base
	// rootfs.img). Both come from the driver Config.
	//
	// C-7-LT-7 (smoke-r17): the rewriter ALSO needs the current
	// user_id so disks[*].path under /var/zeroship/ch/users/<usr>/
	// (the per-user persistent home image, shared across every
	// sandbox a user owns) is accepted. Cross-tenant isolation is
	// preserved via strict user_id equality in the prefix check.
	var contentRoots []string
	if p.config != nil {
		contentRoots = p.config.ContentAddressedRootfsRoots
	}
	rewritten, runtimeFiles, err := rewriteConfigJSON(origConfig, runDir, driverConfig.VMIndex, base, driverConfig.SandboxId, driverConfig.UserId, contentRoots)
	if err != nil {
		return nil, nil, restoreErrorf(stageRewriteConfig, "rewrite config (source=%s, runDir=%s): %v", snapshotConfigPath, runDir, err)
	}
	if err := os.WriteFile(rewrittenConfigPath, rewritten, 0o600); err != nil {
		return nil, nil, restoreErrorf(stageWriteRewrittenConfig, "write rewritten config %s: %v", rewrittenConfigPath, err)
	}

	// C-7-LT-10 (smoke-r20): symlink the immutable snapshot artifacts
	// (state.json, memory-ranges) from <RestoreFrom> into <runDir> so
	// CH `--restore source_url=file://<runDir>` sees them next to the
	// rewritten config.json. Without this hop CH consults whichever
	// directory the `source_url` names — pre-fix that was the source
	// dir whose config.json is still the un-rewritten copy pointing
	// at the OLD alloc's task_dir. The fix routes CH through runDir
	// for ALL three files: config.json (rewritten, written above)
	// plus state.json and memory-ranges (symlinks into the read-only
	// source dir, so the immutable artifacts stay untouched).
	//
	// Mode: an `os.Symlink` here creates the link with default
	// (mode-irrelevant) permissions; CH opens the target via the
	// symlink and inherits the source's permission bits, which is
	// what we want.
	//
	// Idempotency: a re-attempt of a previously-failed restore will
	// find the symlinks already in place. `fs.ErrExist` is the
	// expected outcome of the second call and is tolerated; any
	// other error surfaces (e.g. EPERM on a noexec mount, target
	// missing — though the earlier validateSnapshotDir step already
	// asserted those exist).
	for _, name := range []string{snapshotStateFile, snapshotMemoryFile} {
		src := filepath.Join(driverConfig.RestoreFrom, name)
		dst := filepath.Join(runDir, name)
		if err := os.Symlink(src, dst); err != nil && !errors.Is(err, fs.ErrExist) {
			return nil, nil, restoreErrorf(stageSymlinkSnapshotArtifact, "symlink %s -> %s: %v", src, dst, err)
		}
	}

	// T-9-perf-prewarm: hint the kernel to populate the page cache for
	// memory-ranges before CH `--restore` spawns. fadvise is non-
	// blocking: the syscall returns immediately and the kernel does the
	// readahead asynchronously in parallel with CH startup, so the
	// 35-43s cold mmap+page-fault-in cost is partially hidden behind
	// CH's own startup sequence. Best-effort: any error (e.g. ENOENT
	// if the source dir was GC'd between validateSnapshotDir and here,
	// EPERM on exotic seccomp) is WARN-logged and the wake continues —
	// a failed prewarm is a missed optimisation, not a correctness
	// failure. The symlinks above ensure the real file is reachable at
	// the runDir path passed here.
	memRangesPath := filepath.Join(driverConfig.RestoreFrom, snapshotMemoryFile)
	if err := prewarmFileFn(memRangesPath); err != nil {
		p.logger.Warn("ch: startTaskRestoreBranch: prewarm memory-ranges failed; continuing",
			"stage", stagePrewarmMemoryRanges,
			"path", memRangesPath,
			"err", err)
	}

	// C-7-LT-12a (smoke-r22): stage rootfs.img into runDir so the
	// rewriter's retarget of disks[0].path → <runDir>/rootfs.img
	// resolves to a real file. Pre-fix the rewriter retargeted the
	// path and validation passed (task_dir allow-list, C-7-LT-6),
	// but nothing materialised the bytes at the new destination —
	// CH then aborted at `VM Restore failed: DeviceManager(Disk(
	// NotFound))` (smoke-r22 verbatim stderr).
	//
	// Why rootfs needs explicit staging but workspace.img / home.img
	// don't: the latter two live at stable persistent paths
	// (`/var/zeroship/ch/<sbx>/workspace.img`,
	// `/var/zeroship/ch/users/<usr>/home.img`) outside the alloc
	// dir, so they survive the source-alloc GC. rootfs.img is
	// alloc-scoped on the source side (the cold-boot's
	// materializeRootfs copied it INTO the alloc task_dir) and gets
	// GC'd alongside the source alloc — its bytes have nowhere to
	// live across the snapshot/restore boundary unless the driver
	// re-stages them from the source artifact.
	//
	// Source: the controller emits `rootfs_source = runtime_dir/
	// rootfs-slim.img` in the ChPlugin restore-path Config — the
	// same path the cold-boot's materializeRootfs copies from. The
	// driver hardlinks first (O_1 regardless of image size) and
	// falls back to a stdlib copy on EXDEV (cross-device — runtime
	// dir on a separate filesystem from the alloc dir).
	if driverConfig.RootfsSource == "" {
		return nil, nil, restoreErrorf(stageRootfsSourceMissing, "rootfs_source is empty; controller must emit ChPlugin Config.rootfs_source on the restore branch (C-7-LT-12a)")
	}
	if _, err := os.Stat(driverConfig.RootfsSource); err != nil {
		return nil, nil, restoreErrorf(stageRootfsSourceMissing, "rootfs_source %s not readable: %v", driverConfig.RootfsSource, err)
	}
	rootfsDst := filepath.Join(runDir, chRootfsName)
	if err := stageRootfsForRestore(driverConfig.RootfsSource, rootfsDst); err != nil {
		return nil, nil, restoreErrorf(stageStageRootfs, "stage rootfs %s -> %s: %v", driverConfig.RootfsSource, rootfsDst, err)
	}

	// T-8b-stress-r9-retry-6 (driver v21): wake-side OFD-lock-wait
	// gate. The hardlink stageRootfsForRestore just created shares
	// the inode + kernel-attributed OFD locks with the source alloc's
	// rootfs.img. The source alloc's CH process may not have released
	// its exclusive write lock yet (kernel-deferred `__fput` after
	// the last close lags reap by tens to hundreds of ms on a busy
	// host). If we spawn CH `--restore` now, CH errors with
	// `Can't get Write lock for .../rootfs.img as there is already a
	// ExclusiveWrite lock` and the resume RPC returns HTTP 500 — the
	// exact wedge stress-r9-retry-6 caught at c=4 (0/8 WAKE).
	//
	// Mirrors the destroy-side r5-A pattern (stop_task.go's
	// pollAcquireOFDLock + waitForOFDLockRelease): try to acquire
	// F_OFD_SETLK F_WRLCK on the rootfs path; success means the
	// source-side struct file has been closed and `__fput` ran. On
	// budget exhaustion (5s wall), bump the labelled counter +
	// surface stage=wake_rootfs_lock_wait to Nomad — spawning CH
	// anyway would just hit the same AlreadyLocked with no
	// diagnostic improvement.
	if err := pollWaitForRootfsLockReleased(rootfsDst); err != nil {
		incWakeRootfsLockHeld()
		return nil, nil, restoreErrorf(stageWakeRootfsLockWait,
			"%v (path=%s, wake_rootfs_lock_held_total=%d)",
			err, rootfsDst, WakeRootfsLockHeldTotal())
	}

	// C-7-LT-9 (smoke-r19): pre-create each runtime file the rewriter
	// flagged. CH `--restore` opens `serial.file` / `console.file`
	// without `O_CREAT`; on a NEW alloc those paths point at the
	// freshly-created NEW task_dir where the file does not yet exist
	// (the prior alloc's serial.log lived at the OLD task_dir, which
	// is unreachable). Without this step CH aborts at
	// CreateConsoleDevices(... NotFound ...) before VmBoot — the
	// smoke-r19 stderr the diagnostic loop captured.
	//
	// Mode 0o640: owner read/write, group read, world none. Matches
	// the bash wrapper's umask defaults (its `--serial file=...`
	// argument creates the same mode through CH on cold-boot). The
	// file ownership is whatever uid/gid the driver runs as
	// (typically root in production); the in-guest serial sink
	// inherits that on open.
	//
	// Best-effort fsync NOT required: the file just needs to exist
	// at CH's open() call; durability across host crash mid-restore
	// is irrelevant (restore re-runs from the snapshot artifact).
	//
	// O_TRUNC included so a re-attempt of a previously-failed restore
	// starts with an empty log rather than appending to whatever
	// half-written content a prior failed spawn dribbled in.
	for _, path := range runtimeFiles {
		f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o640)
		if err != nil {
			return nil, nil, restoreErrorf(stagePrecreateRuntimeFile, "open runtime file %s: %v", path, err)
		}
		if err := f.Close(); err != nil {
			return nil, nil, restoreErrorf(stagePrecreateRuntimeFile, "close runtime file %s: %v", path, err)
		}
	}

	// Step 3: tap setup. Same per-VM /30 plumbing as cold-boot. The
	// operator-supplied Net[] short-circuit is preserved so an
	// externally-managed tap still works on the restore branch.
	tapName, _ := resolveNet(driverConfig)
	if len(driverConfig.Net) > 0 {
		if err := ensureTapUp(tapName); err != nil {
			return nil, nil, restoreErrorf(stageTapSetup, "tap %s not ready: %v", tapName, err)
		}
	} else {
		if _, err := setupTapForVM(driverConfig.VMIndex, base); err != nil {
			return nil, nil, restoreErrorf(stageTapSetup, "setup tap for vm_index=%d: %v", driverConfig.VMIndex, err)
		}
	}

	// Step 4: spawn CH with --restore. The URL syntax is CH-specific:
	// `source_url=file://<dir>` (a tagged key=value pair, NOT a bare
	// URL). The trailing dir is the staged snapshot dir; CH reads
	// state.json + config.json + memory-ranges from that location.
	//
	// C-7-LT-10 (smoke-r20): point CH at runDir, NOT RestoreFrom.
	// runDir now holds (a) the rewritten config.json written above
	// and (b) symlinks to state.json + memory-ranges in the read-only
	// source dir. Pre-fix this said `RestoreFrom` and CH read the
	// un-rewritten config from the snapshot dir; three smoke cycles
	// failed at CreateConsoleDevice ENOENT before the writer/reader
	// asymmetry was caught.
	//
	// CRITICAL: per CH docs, --restore is INCOMPATIBLE with --kernel
	// / --cmdline / --disk / --net / --memory / --cpus / --serial.
	// Those would conflict with the snapshot's embedded config. We
	// pass ONLY --api-socket + --restore.
	restoreURL := "source_url=file://" + runDir
	argv := []string{
		chBin,
		"--api-socket", apiSocket,
		"--restore", restoreURL,
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = runDir
	cmd.Stdout = nil

	// C-7-LT-3-PR2 (smoke-r14): redirect CH stderr to a per-alloc
	// file under the run dir so a CH that spawns and dies before
	// its API socket comes up leaves a diagnostic trail. The
	// defaultRunnerFactory tees through io.MultiWriter so the
	// existing WaitTask exit-tail behaviour is preserved. O_TRUNC
	// so a re-attempt doesn't accumulate stale output.
	//
	// Best-effort: a failure to open the file does NOT abort the
	// spawn (an unwritable run dir would surface as a clearer
	// downstream error from CH itself). The file path is recorded
	// for the socket-timeout diagnostic below.
	stderrLogPath := filepath.Join(runDir, chStderrLogName)
	stderrFile, stderrErr := os.OpenFile(stderrLogPath, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if stderrErr == nil {
		cmd.Stderr = stderrFile
		// Close our handle once the runner takes ownership of the
		// fd; the subprocess inherits via exec and keeps it open.
		// Deferred until after Start so the subprocess inherits a
		// valid descriptor; the explicit Close is run at function
		// exit (any branch) to avoid leaking the host-side fd.
		defer func() { _ = stderrFile.Close() }()
	} else {
		p.logger.Warn("ch: startTaskRestoreBranch: stderr-log open failed; continuing without per-alloc stderr capture",
			"path", stderrLogPath, "err", stderrErr)
	}

	runner := p.chClient.RunnerFactory()(cmd)
	if err := runner.Start(); err != nil {
		// Spawn errors typically fire before exec() reaches the child
		// (ENOENT on the binary path, EACCES on the runtime_dir,
		// ulimit / cgroup denial). Best-effort capture the stderr
		// tail anyway — if the runner factory's tee opened the file
		// the child MAY have written diagnostics before dying.
		tail := readStderrTail(stderrLogPath, chStderrTailBytes)
		return nil, nil, restoreErrorWithStderr(stageRestoreSpawn, stderrLogPath, tail,
			"spawn cloud-hypervisor --restore (chBin=%s, api_socket=%s, run_dir=%s): %v",
			chBin, apiSocket, runDir, err)
	}

	startedAt := time.Now().UTC()
	pid := runner.Pid()

	// Step 5: poll the API socket. CH may take up to ~1s to bind it
	// after mmap'ing the snapshot memory image; under load the
	// restore path has been observed past 10s (smoke-r14,
	// C-7-LT-3) — the default budget is now 60s.
	if err := pollAPISocketFn(p.chClient, apiSocket, defaultAPISocketPollTimeout, defaultAPISocketPollInterval); err != nil {
		_ = runner.Signal(os.Kill)
		// C-7-LT-3-PR2: embed the CH stderr tail so the next
		// cluster smoke has visibility into whether CH crashed or
		// was just slow. Best-effort: a missing/unreadable file
		// produces an empty tail and the original error remains
		// useful on its own. We prefer the on-disk file (persists
		// past task-failure) but fall back to the runner's
		// in-memory tail buffer if the file capture path didn't
		// initialise (open error path above).
		tail := readStderrTail(stderrLogPath, chStderrTailBytes)
		if len(tail) == 0 {
			tail = runner.StderrTail(chStderrTailBytes)
		}
		return nil, nil, restoreErrorWithStderr(stageLivezProbe, stderrLogPath, tail,
			"api socket readiness probe failed (api_socket=%s): %v", apiSocket, err)
	}

	// Step 6: ch-remote resume. THIS is what brings the VM back from
	// paused → running. Without it the guest's eth0 never replies to
	// ARP and the controller's /livez probe gets EHOSTUNREACH.
	if err := resumeFn(p.chClient, apiSocket); err != nil {
		_ = runner.Signal(os.Kill)
		// C-7-LT-11: mirror the socket-poll-timeout stderr capture so
		// smoke-r22 has CH diagnostic output when ch-remote resume
		// returns an error (e.g. HTTP 500 "VM is not running").
		tail := readStderrTail(stderrLogPath, chStderrTailBytes)
		if len(tail) == 0 {
			tail = runner.StderrTail(chStderrTailBytes)
		}
		return nil, nil, restoreErrorWithStderr(stageResume, stderrLogPath, tail,
			"resume failed (api_socket=%s): %v", apiSocket, err)
	}

	// Step 7: persist TaskState + register handle + start supervisor.
	state := &TaskState{
		TaskConfig:    cfg,
		StartedAt:     startedAt,
		CHPid:         pid,
		APISocket:     apiSocket,
		VMIndex:       driverConfig.VMIndex,
		Tap:           tapName,
		Mode:          mode,
		SandboxId:     driverConfig.SandboxId,
		NomadTaskName: cfg.Name,
	}
	handle := drivers.NewTaskHandle(TaskHandleVersion)
	handle.Config = cfg
	if err := handle.SetDriverState(state); err != nil {
		_ = runner.Signal(os.Kill)
		return nil, nil, restoreErrorf(stagePersistState, "persist TaskState (task_id=%s): %v", cfg.ID, err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	h := &taskHandle{
		logger:       p.logger.With("task_id", cfg.ID, "vm_index", driverConfig.VMIndex),
		taskConfig:   cfg,
		driverConfig: driverConfig,
		procState:    drivers.TaskStateRunning,
		startedAt:    startedAt,
		exitResult:   &drivers.ExitResult{},
		chPid:        pid,
		apiSocket:    apiSocket,
		vmIndex:      driverConfig.VMIndex,
		tap:          tapName,
		mode:         mode,
		runner:       runner,
		exitDone:     make(chan struct{}),
		ctx:          ctx,
		cancelFn:     cancel,
	}
	p.tasks.Set(cfg.ID, h)
	go p.superviseCH(h)

	p.logger.Info("ch: StartTask (restore branch): resumed",
		"task_id", cfg.ID,
		"ch_pid", pid,
		"api_socket", apiSocket,
		"tap", tapName,
		"restore_from", driverConfig.RestoreFrom)

	return handle, nil, nil
}

// readStderrTail returns the last up-to-`max` bytes of the file at
// `path`. Used by the socket-timeout error path on the restore
// branch to embed CH stderr in the operator-facing Nomad event
// message. Best-effort: returns nil on any open/seek/read error so
// the caller can fall back to a different source (in-memory tail
// buffer) without erroring on the diagnostic itself.
//
// Implementation notes:
//   - Seeks from the end so a multi-GiB log doesn't pull the whole
//     file into memory; we only need the tail.
//   - On read partial-success we return what we got rather than
//     erroring — a torn tail of CH stderr is still more useful
//     than nothing.
func readStderrTail(path string, max int) []byte {
	if path == "" || max <= 0 {
		return nil
	}
	f, err := os.Open(path)
	if err != nil {
		return nil
	}
	defer f.Close()
	stat, err := f.Stat()
	if err != nil {
		return nil
	}
	size := stat.Size()
	if size == 0 {
		return nil
	}
	readN := int64(max)
	if size < readN {
		readN = size
	}
	if _, err := f.Seek(-readN, io.SeekEnd); err != nil {
		return nil
	}
	buf := make([]byte, readN)
	n, err := io.ReadFull(f, buf)
	if err != nil && err != io.ErrUnexpectedEOF {
		// EOF/UnexpectedEOF tolerated — return whatever we got.
		if n == 0 {
			return nil
		}
	}
	return buf[:n]
}

// validateSnapshotDir confirms the snapshot artifact dir exists and
// carries the three CH-restore-required files. Returns a precise
// "missing X" error so an operator log shows what the controller
// failed to stage.
//
// Mirrors the wrapper's pre-spawn `[ -f $RESTORE_FROM/X ]` triple.
func validateSnapshotDir(dir string) error {
	if dir == "" {
		return errors.New("ch: restore: RestoreFrom is empty")
	}
	st, err := os.Stat(dir)
	if err != nil {
		return fmt.Errorf("ch: restore: stat %s: %w", dir, err)
	}
	if !st.IsDir() {
		return fmt.Errorf("ch: restore: %s is not a directory", dir)
	}
	for _, name := range []string{snapshotStateFile, snapshotConfigFile, snapshotMemoryFile} {
		path := filepath.Join(dir, name)
		if _, err := os.Stat(path); err != nil {
			return fmt.Errorf("ch: restore: snapshot dir %s missing %s: %w", dir, name, err)
		}
	}
	return nil
}
