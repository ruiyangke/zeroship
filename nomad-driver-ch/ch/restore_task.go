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
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
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
		return nil, nil, errors.New("ch: startTaskRestoreBranch: nil TaskConfig")
	}
	if driverConfig == nil {
		return nil, nil, errors.New("ch: startTaskRestoreBranch: nil driverConfig")
	}
	if driverConfig.RestoreFrom == "" {
		return nil, nil, errors.New("ch: startTaskRestoreBranch: RestoreFrom is empty")
	}

	// VMIndex is the one cold-boot validation that DOES apply on the
	// restore branch: the new tap name is derived from it. Other
	// cold-boot guards (sandbox_id, workspace_img, pubkey) are NOT
	// applied — the snapshot already carries that material in its
	// memory image, and the controller wakes a snapshot WITHOUT
	// re-supplying those fields.
	if driverConfig.VMIndex < 1 || driverConfig.VMIndex > 155 {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: vm_index %d out of range [1,155]", driverConfig.VMIndex)
	}
	if driverConfig.SubnetBaseOctet > 255 {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: subnet_base_octet %d out of u8 range", driverConfig.SubnetBaseOctet)
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
		return nil, nil, err
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
		return nil, nil, errors.New("ch: startTaskRestoreBranch: cloud-hypervisor binary not found (set ZSBX_CH_BIN or config.cloud_hypervisor_bin)")
	}

	runDir := taskRunDir(cfg, p.config)
	if err := os.MkdirAll(runDir, 0o755); err != nil {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: mkdir runDir %s: %w", runDir, err)
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
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: read snapshot config %s: %w", snapshotConfigPath, err)
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
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: rewrite config: %w", err)
	}
	if err := os.WriteFile(rewrittenConfigPath, rewritten, 0o600); err != nil {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: write rewritten config %s: %w", rewrittenConfigPath, err)
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
			return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: symlink %s -> %s: %w", src, dst, err)
		}
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
			return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: pre-create runtime file %s: %w", path, err)
		}
		if err := f.Close(); err != nil {
			return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: close runtime file %s: %w", path, err)
		}
	}

	// Step 3: tap setup. Same per-VM /30 plumbing as cold-boot. The
	// operator-supplied Net[] short-circuit is preserved so an
	// externally-managed tap still works on the restore branch.
	tapName, _ := resolveNet(driverConfig)
	if len(driverConfig.Net) > 0 {
		if err := ensureTapUp(tapName); err != nil {
			return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: tap %s not ready: %w", tapName, err)
		}
	} else {
		if _, err := setupTapForVM(driverConfig.VMIndex, base); err != nil {
			return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: setup tap for vm_index=%d: %w", driverConfig.VMIndex, err)
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
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: spawn cloud-hypervisor --restore: %w", err)
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
		if len(tail) > 0 {
			return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: %w; ch_stderr_tail=%q (path=%s)", err, string(tail), stderrLogPath)
		}
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: %w (no ch stderr captured; path=%s)", err, stderrLogPath)
	}

	// Step 6: ch-remote resume. THIS is what brings the VM back from
	// paused → running. Without it the guest's eth0 never replies to
	// ARP and the controller's /livez probe gets EHOSTUNREACH.
	if err := resumeFn(p.chClient, apiSocket); err != nil {
		_ = runner.Signal(os.Kill)
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: resume failed: %w", err)
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
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: persist TaskState: %w", err)
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
