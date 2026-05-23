// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (the libvirt provider that lived
// under providers/libvirt/) on 2026-05-25 for Cloud Hypervisor support. The
// CH-specific client speaks the cloud-hypervisor Unix-domain HTTP API
// (`vmm/src/api/openapi/cloud-hypervisor.yaml`); the wire idiom is documented
// in the volantvm spike § 3 ("vm_operations.go::httpRequest").

package ch

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"sync"
	"time"

	"github.com/hashicorp/go-hclog"
)

// Client is a thin wrapper around `cloud-hypervisor` (as a subprocess) and
// the `ch-remote` REST API exposed over the per-VM Unix socket. It does NOT
// hold per-task state — that lives on taskHandle. The single shared Client
// holds the HTTP transport, the resolved binary paths, the logger, and the
// processRunner factory (which tests swap out).
//
// The CH REST shape we target (per the volantvm spike and the CH OpenAPI
// spec at vmm/src/api/openapi/cloud-hypervisor.yaml):
//
//   PUT  /api/v1/vm.create   { payload: { kernel, cmdline, initramfs? },
//                              cpus, memory, disks, net, fs, ... }
//   PUT  /api/v1/vm.boot     {}
//   PUT  /api/v1/vm.resume   {}             // post-restore
//   PUT  /api/v1/vm.shutdown {}             // graceful stop
//   GET  /api/v1/vm.info                    // for TaskStats + InspectTask
//   PUT  /api/v1/vm.snapshot { destination_url: "file://<path>" }
//   PUT  /api/v1/vm.restore  { source_url:      "file://<path>" }
//
// Per-call socket dial is fine — the calls are not in any hot path; CH
// keeps the listener open for the life of the VM.
type Client struct {
	logger hclog.Logger

	// chBin is the absolute path to the `cloud-hypervisor` binary. Resolved
	// in NewClient from (in order) the ZSBX_CH_BIN env var, the configured
	// Config.CloudHypervisorBin, then PATH. Cached so StartTask's hot path
	// doesn't shell-out for the lookup on every spawn.
	chBin string

	// chRemoteBin is the absolute path to the `ch-remote` binary. Same
	// discovery order as chBin (env override → Config → PATH).
	chRemoteBin string

	// runnerFactory builds a processRunner for the given exec.Cmd. The
	// default builds a defaultRunner backed by os/exec; tests inject a
	// fakeRunner factory to assert argv without actually spawning CH.
	runnerFactory func(cmd *exec.Cmd) processRunner

	// muBin guards chBin / chRemoteBin updates from a late call to Probe.
	// Tests may swap binaries between scenarios.
	muBin sync.RWMutex
}

// NewClient constructs the shared Client. Looks up the cloud-hypervisor and
// ch-remote binaries on PATH (with env overrides) so a missing-binary error
// surfaces at plugin load instead of at first StartTask.
//
// A missing binary does NOT fail NewClient — the binary may not be present
// during early Nomad-client probing on a host that hasn't been provisioned
// yet. Callers that need a binary (StartTask) re-resolve via lookupBin and
// emit a structured error then.
func NewClient(logger hclog.Logger) *Client {
	c := &Client{
		logger:        logger,
		runnerFactory: defaultRunnerFactory,
	}
	// Best-effort discovery — failures are tolerated (StartTask retries).
	c.chBin, _ = lookupBin("ZSBX_CH_BIN", "", "cloud-hypervisor")
	c.chRemoteBin, _ = lookupBin("ZSBX_CH_REMOTE_BIN", "", "ch-remote")
	return c
}

// SetBinaries lets the driver-level config (via Plugin.SetConfig) override
// the discovered binary paths. Called after NewClient when SetConfig populates
// Config.CloudHypervisorBin and friends.
func (c *Client) SetBinaries(chBin, chRemoteBin string) {
	c.muBin.Lock()
	defer c.muBin.Unlock()
	if chBin != "" {
		// Reconfirm against PATH if absolute path missing.
		if resolved, err := lookupBin("ZSBX_CH_BIN", chBin, "cloud-hypervisor"); err == nil {
			c.chBin = resolved
		}
	}
	if chRemoteBin != "" {
		if resolved, err := lookupBin("ZSBX_CH_REMOTE_BIN", chRemoteBin, "ch-remote"); err == nil {
			c.chRemoteBin = resolved
		}
	}
}

// CHBin returns the resolved absolute path to cloud-hypervisor, or "" if it
// couldn't be located. Callers that need it should treat "" as an error.
func (c *Client) CHBin() string {
	c.muBin.RLock()
	defer c.muBin.RUnlock()
	return c.chBin
}

// CHRemoteBin returns the resolved absolute path to ch-remote.
func (c *Client) CHRemoteBin() string {
	c.muBin.RLock()
	defer c.muBin.RUnlock()
	return c.chRemoteBin
}

// RunnerFactory returns the current process-runner factory. StartTask uses
// this to spawn the CH subprocess; tests override it.
func (c *Client) RunnerFactory() func(cmd *exec.Cmd) processRunner {
	return c.runnerFactory
}

// SetRunnerFactory replaces the process-runner factory. ONLY for tests.
func (c *Client) SetRunnerFactory(f func(cmd *exec.Cmd) processRunner) {
	c.runnerFactory = f
}

// lookupBin resolves a binary path with the precedence:
//
//  1. env var (envName), if non-empty
//  2. cfgPath (from the driver Config block), if non-empty
//  3. exec.LookPath(name)
//
// Returns ("", err) on miss. The returned path is always absolute on
// success (LookPath does that; cfgPath / env are trusted verbatim).
func lookupBin(envName, cfgPath, name string) (string, error) {
	if envName != "" {
		if v := os.Getenv(envName); v != "" {
			if _, err := os.Stat(v); err == nil {
				return v, nil
			}
		}
	}
	if cfgPath != "" {
		if _, err := os.Stat(cfgPath); err == nil {
			return cfgPath, nil
		}
	}
	resolved, err := exec.LookPath(name)
	if err != nil {
		return "", fmt.Errorf("ch: %s not found on PATH: %w", name, err)
	}
	return resolved, nil
}

// httpClientForSocket returns an http.Client whose Transport dials the given
// Unix-domain socket path on every request. Hostname in the URL is ignored
// by the dialer; convention is "unix" (matches the upstream virt + volantvm
// idiom).
//
// The 5-second timeout is the volantvm value and works fine for our
// cold-boot envelope.
func httpClientForSocket(socketPath string) *http.Client {
	return &http.Client{
		Timeout: 5 * time.Second,
		Transport: &http.Transport{
			DialContext: func(_ context.Context, _, _ string) (net.Conn, error) {
				return net.Dial("unix", socketPath)
			},
		},
	}
}

// Info returns the parsed `vm.info` payload for the VM at socketPath.
//
// Two paths are kept: the structured JSON path (preferred, matches T-5's
// TaskStats consumer) and a fallback that shells out to `ch-remote info`
// (used when the caller wants the same diagnostic shape as the bash
// wrapper). Today we use the HTTP path; ch-remote is reserved for
// administrative commands (shutdown, resume) where ch-remote's CLI
// validates argument shape better than a hand-rolled PUT.
func (c *Client) Info(socketPath string) (*VMInfo, error) {
	if socketPath == "" {
		return nil, errors.New("ch: Info: empty socket path")
	}
	hc := httpClientForSocket(socketPath)
	req, err := http.NewRequest("GET", "http://unix/api/v1/vm.info", nil)
	if err != nil {
		return nil, fmt.Errorf("ch: Info: build request: %w", err)
	}
	resp, err := hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("ch: Info: dial %s: %w", socketPath, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(resp.Body)
		return nil, fmt.Errorf("ch: Info: status=%d body=%q", resp.StatusCode, string(body))
	}
	var info VMInfo
	if err := json.NewDecoder(resp.Body).Decode(&info); err != nil {
		return nil, fmt.Errorf("ch: Info: decode: %w", err)
	}
	return &info, nil
}

// Shutdown asks the running CH instance at socketPath to perform a graceful
// VMM shutdown by invoking `ch-remote --api-socket <socketPath> shutdown-vmm`.
//
// We shell out to ch-remote (rather than issue the HTTP PUT directly the way
// Info does) because:
//
//  1. ch-remote's CLI validates argument shape and emits the exact error text
//     the bash wrapper consumes today; staying on the same wire keeps cluster
//     behaviour identical (the wrapper's cleanup trap uses kill, but the
//     controller path that calls /shutdown ultimately reaches the same CH
//     control channel).
//  2. The shutdown-vmm semantic (terminate the VMM process gracefully) is
//     subtly different from vm.shutdown (power-off the guest, leave the VMM
//     running) and shutdown-vmm is the operation the proposal § 7 calls for
//     in the StopTask ladder.
//
// Behaviour:
//   - Returns nil if ch-remote exits 0.
//   - Returns an error (containing the combined output) on non-zero exit, on
//     missing ch-remote binary, or on empty socketPath.
//
// Note: ch-remote returns success the moment CH acknowledges the request;
// the actual process exit is observed by StopTask waiting on runner.Wait().
func (c *Client) Shutdown(socketPath string) error {
	if socketPath == "" {
		return errors.New("ch: Shutdown: empty socket path")
	}
	chRemote := c.CHRemoteBin()
	if chRemote == "" {
		return errors.New("ch: Shutdown: ch-remote binary not resolved")
	}
	cmd := exec.Command(chRemote, "--api-socket", socketPath, "shutdown-vmm")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("ch: Shutdown: ch-remote shutdown-vmm: %w (output=%q)", err, string(out))
	}
	return nil
}

// Resume sends `PUT /api/v1/vm.resume` to the VM at socketPath.
// Stubbed until T-6 implements the restore path.
func (c *Client) Resume(socketPath string) error {
	return errors.New("ch: T-6: Client.Resume not implemented")
}

// shutdownFn is the package-level seam that tests swap to fake ch-remote
// without spawning a real binary. Default delegates to Client.Shutdown.
// Mirrors the ensureTapUpFn pattern from start_task.go.
var shutdownFn = func(c *Client, socketPath string) error {
	return c.Shutdown(socketPath)
}

// SetShutdownForTest replaces the ch-remote shutdown seam. Returns the
// previous fn so the caller can restore it on cleanup.
func SetShutdownForTest(fn func(c *Client, socketPath string) error) func(*Client, string) error {
	prev := shutdownFn
	if fn != nil {
		shutdownFn = fn
	}
	return prev
}

// removeTapFn is the package-level seam for tap teardown. Retained as a
// thin alias over teardownTapFn (T-3) so the existing stop_task.go call
// site and the SetRemoveTapForTest test API don't churn. T-3 owns the
// underlying impl in net.go.
//
// Reads and writes here MUST funnel through teardownTapFn under the hood
// so a test that sets one seam doesn't silently bypass the other.
var removeTapFn = func(tapName string) error {
	return teardownTap(tapName)
}

// SetRemoveTapForTest replaces the tap-down seam. Returns the previous fn.
// Today this is the SAME seam as SetTeardownTapForTest, just under the
// historical name the T-2 tests use. New tests should prefer
// SetTeardownTapForTest; this one is preserved verbatim so the T-2 suite
// still drives the same code path.
func SetRemoveTapForTest(fn func(tapName string) error) func(string) error {
	prev := removeTapFn
	if fn != nil {
		removeTapFn = fn
	}
	return prev
}

// VMInfo is the decoded shape of CH's `GET /api/v1/vm.info` payload. Only
// the fields we consume are listed; CH's actual response is larger but
// stable across v48..v51 (per the volantvm spike § 2 row 9).
type VMInfo struct {
	State  string `json:"state"` // "Created"|"Running"|"Shutdown"|...
	Memory struct {
		ActualSize uint64 `json:"actual_size"` // bytes
	} `json:"memory"`
	CPU struct {
		Utilisation uint64 `json:"utilisation"`
	} `json:"cpu"`
}

// ------------------------------------------------------------------
// processRunner: a small seam over os/exec so tests can assert argv
// without spawning a real cloud-hypervisor.
//
// The default runner wraps *exec.Cmd. The fake runner used in tests
// records the argv vector and lets the test drive Wait()/Pid()
// deterministically.
// ------------------------------------------------------------------

// processRunner is the abstract handle StartTask uses to manage the spawned
// CH process. The minimal surface is intentional: Start (spawn), Wait
// (block on exit and return ExitError-like info), Pid (for state
// persistence), Signal (for StopTask), and Stderr (last bytes for error
// surface).
//
// All methods are goroutine-safe with respect to themselves; Wait must be
// callable exactly once (matches os/exec.Cmd.Wait's contract).
type processRunner interface {
	// Start spawns the underlying process. Must return promptly.
	Start() error
	// Wait blocks until the process exits. Returns nil on clean exit
	// (exit code 0), an error otherwise. Callers consult ExitCode and
	// StderrTail on a non-nil error.
	Wait() error
	// Pid returns the PID of the spawned process. Valid only after Start.
	Pid() int
	// Signal forwards a signal to the process. Used by StopTask (T-2)
	// and by SignalTask.
	Signal(sig os.Signal) error
	// ExitCode returns the exit code from the last Wait. Valid only
	// after Wait returns. -1 on signal-terminated.
	ExitCode() int
	// StderrTail returns the last min(N, len(stderr)) bytes of the
	// process's stderr. Used by WaitTask to populate ExitResult.Err
	// with the failure message.
	StderrTail(n int) []byte
}

// defaultRunner wraps *exec.Cmd. The stderr buffer is a ring-buffer-ish
// io.Writer that keeps the last `stderrCap` bytes; CH's stderr is mostly
// quiet (it logs to --log-file) so a 4 KiB cap is plenty.
const stderrCap = 4096

type defaultRunner struct {
	cmd     *exec.Cmd
	stderr  *tailBuffer
	mu      sync.Mutex
	exited  bool
	exitErr error
	code    int
}

// defaultRunnerFactory is the production runner factory. Wraps cmd's stderr
// in a tailBuffer so WaitTask can surface the last bytes on failure.
func defaultRunnerFactory(cmd *exec.Cmd) processRunner {
	tb := newTailBuffer(stderrCap)
	cmd.Stderr = tb
	return &defaultRunner{cmd: cmd, stderr: tb}
}

func (r *defaultRunner) Start() error {
	return r.cmd.Start()
}

func (r *defaultRunner) Wait() error {
	err := r.cmd.Wait()
	r.mu.Lock()
	r.exited = true
	r.exitErr = err
	if ee, ok := err.(*exec.ExitError); ok {
		r.code = ee.ExitCode()
	} else if err == nil {
		r.code = 0
	} else {
		r.code = -1
	}
	r.mu.Unlock()
	return err
}

func (r *defaultRunner) Pid() int {
	if r.cmd.Process == nil {
		return 0
	}
	return r.cmd.Process.Pid
}

func (r *defaultRunner) Signal(sig os.Signal) error {
	if r.cmd.Process == nil {
		return errors.New("ch: Signal: process not started")
	}
	return r.cmd.Process.Signal(sig)
}

func (r *defaultRunner) ExitCode() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.code
}

func (r *defaultRunner) StderrTail(n int) []byte {
	if r.stderr == nil {
		return nil
	}
	return r.stderr.Tail(n)
}

// tailBuffer is a fixed-cap io.Writer that keeps the last `cap` bytes
// written. Used to capture the tail of CH stderr without unbounded growth.
type tailBuffer struct {
	mu   sync.Mutex
	buf  []byte
	cap  int
	full bool
	pos  int
}

func newTailBuffer(cap int) *tailBuffer {
	return &tailBuffer{buf: make([]byte, 0, cap), cap: cap}
}

func (t *tailBuffer) Write(p []byte) (int, error) {
	t.mu.Lock()
	defer t.mu.Unlock()
	for _, b := range p {
		if !t.full {
			t.buf = append(t.buf, b)
			if len(t.buf) == t.cap {
				t.full = true
				t.pos = 0
			}
			continue
		}
		t.buf[t.pos] = b
		t.pos = (t.pos + 1) % t.cap
	}
	return len(p), nil
}

// Tail returns the most-recent min(n, cap, written) bytes in write order.
func (t *tailBuffer) Tail(n int) []byte {
	t.mu.Lock()
	defer t.mu.Unlock()
	if !t.full {
		if n >= len(t.buf) {
			out := make([]byte, len(t.buf))
			copy(out, t.buf)
			return out
		}
		out := make([]byte, n)
		copy(out, t.buf[len(t.buf)-n:])
		return out
	}
	// Ring buffer is full; layout is buf[pos:] + buf[:pos].
	ordered := make([]byte, t.cap)
	copy(ordered, t.buf[t.pos:])
	copy(ordered[t.cap-t.pos:], t.buf[:t.pos])
	if n >= t.cap {
		return ordered
	}
	return ordered[t.cap-n:]
}
