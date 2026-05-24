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
		if p.config != nil {
			p.chClient.SetBinaries(p.config.CloudHypervisorBin, p.config.VirtiofsdBin)
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
	snapshotConfigPath := filepath.Join(driverConfig.RestoreFrom, snapshotConfigFile)
	origConfig, err := os.ReadFile(snapshotConfigPath)
	if err != nil {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: read snapshot config %s: %w", snapshotConfigPath, err)
	}
	base := uint8(driverConfig.SubnetBaseOctet)
	if base == 0 {
		base = defaultSubnetBaseOctet
	}
	rewritten, err := rewriteConfigJSON(origConfig, runDir, driverConfig.VMIndex, base)
	if err != nil {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: rewrite config: %w", err)
	}
	if err := os.WriteFile(rewrittenConfigPath, rewritten, 0o600); err != nil {
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: write rewritten config %s: %w", rewrittenConfigPath, err)
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
	// CRITICAL: per CH docs, --restore is INCOMPATIBLE with --kernel
	// / --cmdline / --disk / --net / --memory / --cpus / --serial.
	// Those would conflict with the snapshot's embedded config. We
	// pass ONLY --api-socket + --restore.
	restoreURL := "source_url=file://" + driverConfig.RestoreFrom
	argv := []string{
		chBin,
		"--api-socket", apiSocket,
		"--restore", restoreURL,
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = runDir
	cmd.Stdout = nil

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
		return nil, nil, fmt.Errorf("ch: startTaskRestoreBranch: %w", err)
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
