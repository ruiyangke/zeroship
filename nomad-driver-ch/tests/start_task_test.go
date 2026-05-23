// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-1 sprint test surface: pins the CH config.json shape, the cold-boot
// validation guards, and the StartTask spawn argv via the processRunner
// seam in ch_client.go. Tests do NOT spawn a real cloud-hypervisor — the
// fake runner records argv and the test asserts on it.

package tests

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/hashicorp/go-hclog"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// validColdBootConfig returns a TaskConfig with every cold-boot required
// field set to a sane value. Helper used by all subtests so a per-test
// "this is what a good config looks like" doesn't drift between cases.
func validColdBootConfig() ch.TaskConfig {
	return ch.TaskConfig{
		VMIndex:         7,
		Kernel:          "/opt/zsbx/vmlinuz",
		CPUs:            2,
		MemoryMB:        512,
		SandboxId:       "sbx_test123",
		WorkspaceImg:    "/var/lib/zsbx/img/workspace.img",
		UserHomeImg:     "/var/lib/zsbx/img/userhome.img",
		PubkeyHex:       "deadbeef",
		SubnetBaseOctet: 99,
	}
}

// TestStartTask_BuildsConfigJSON pins the JSON shape the CH `--config`
// flag will load. Asserts:
//   - cpus.boot_vcpus matches cfg.CPUs
//   - memory.size is MemoryMB × 1 MiB (bytes)
//   - payload.cmdline carries SANDBOX_AGENT_SANDBOX_ID and zsbx_pubkey
//   - disks include the rootfs / workspace / userhome triple
//   - net carries the synthesised tap + MAC
func TestStartTask_BuildsConfigJSON(t *testing.T) {
	cfg := validColdBootConfig()
	taskDir := "/opt/nomad/data/alloc/abc/task/local"

	raw, err := ch.BuildConfigJSON(cfg, taskDir)
	if err != nil {
		t.Fatalf("BuildConfigJSON: %v", err)
	}

	var doc map[string]any
	if err := json.Unmarshal(raw, &doc); err != nil {
		t.Fatalf("Unmarshal config.json: %v", err)
	}

	// payload.kernel + payload.cmdline
	payload, ok := doc["payload"].(map[string]any)
	if !ok {
		t.Fatalf("payload missing: %v", doc)
	}
	if payload["kernel"] != cfg.Kernel {
		t.Errorf("payload.kernel = %v, want %q", payload["kernel"], cfg.Kernel)
	}
	cmdline, _ := payload["cmdline"].(string)
	mustContain(t, "payload.cmdline", cmdline, "SANDBOX_AGENT_SANDBOX_ID=sbx_test123")
	mustContain(t, "payload.cmdline", cmdline, "zsbx_pubkey=deadbeef")
	mustContain(t, "payload.cmdline", cmdline, "ip=10.99.107.2::10.99.107.1:255.255.255.252::eth0:none")
	mustContain(t, "payload.cmdline", cmdline, "console=ttyS0")
	mustContain(t, "payload.cmdline", cmdline, "root=/dev/vda")

	// cpus.boot_vcpus + memory.size
	cpus, ok := doc["cpus"].(map[string]any)
	if !ok {
		t.Fatalf("cpus missing")
	}
	if int(cpus["boot_vcpus"].(float64)) != int(cfg.CPUs) {
		t.Errorf("cpus.boot_vcpus = %v, want %d", cpus["boot_vcpus"], cfg.CPUs)
	}
	mem, ok := doc["memory"].(map[string]any)
	if !ok {
		t.Fatalf("memory missing")
	}
	wantMem := float64(uint64(cfg.MemoryMB) * 1024 * 1024)
	if mem["size"].(float64) != wantMem {
		t.Errorf("memory.size = %v, want %v", mem["size"], wantMem)
	}
	if mem["shared"] != true {
		t.Errorf("memory.shared = %v, want true (virtio-fs friendliness)", mem["shared"])
	}

	// disks: rootfs (under taskDir) + workspace + userhome
	disks, ok := doc["disks"].([]any)
	if !ok {
		t.Fatalf("disks missing")
	}
	if len(disks) != 3 {
		t.Fatalf("disks len = %d, want 3 (rootfs, workspace, userhome)", len(disks))
	}
	d0 := disks[0].(map[string]any)
	if !strings.HasPrefix(d0["path"].(string), taskDir) {
		t.Errorf("disks[0].path = %v, want prefix %q", d0["path"], taskDir)
	}
	if d0["serial"] != "zsbx-root" {
		t.Errorf("disks[0].serial = %v, want zsbx-root", d0["serial"])
	}
	d1 := disks[1].(map[string]any)
	if d1["path"] != cfg.WorkspaceImg {
		t.Errorf("disks[1].path = %v, want %q", d1["path"], cfg.WorkspaceImg)
	}
	if d1["serial"] != "zsbx-work" {
		t.Errorf("disks[1].serial = %v, want zsbx-work", d1["serial"])
	}
	d2 := disks[2].(map[string]any)
	if d2["path"] != cfg.UserHomeImg {
		t.Errorf("disks[2].path = %v, want %q", d2["path"], cfg.UserHomeImg)
	}

	// net: one entry, tap name + MAC derived from VMIndex
	nets, ok := doc["net"].([]any)
	if !ok || len(nets) != 1 {
		t.Fatalf("net = %v, want 1 entry", doc["net"])
	}
	n0 := nets[0].(map[string]any)
	if n0["tap"] != "zsbx-nm-7" {
		t.Errorf("net[0].tap = %v, want zsbx-nm-7", n0["tap"])
	}
	if n0["mac"] != "12:34:56:78:9b:07" {
		t.Errorf("net[0].mac = %v, want 12:34:56:78:9b:07", n0["mac"])
	}

	// serial.file under taskDir
	serial, ok := doc["serial"].(map[string]any)
	if !ok {
		t.Fatalf("serial missing")
	}
	if serial["mode"] != "File" {
		t.Errorf("serial.mode = %v, want File", serial["mode"])
	}
	if !strings.HasPrefix(serial["file"].(string), taskDir) {
		t.Errorf("serial.file = %v, want prefix %q", serial["file"], taskDir)
	}
}

// TestStartTask_RejectsMissingEnv enumerates each cold-boot required
// field; mutates the valid config so the field is empty; asserts the
// error mentions the field name.
func TestStartTask_RejectsMissingEnv(t *testing.T) {
	cases := []struct {
		name      string
		mutate    func(*ch.TaskConfig)
		wantSubst string
	}{
		{"empty SandboxId", func(c *ch.TaskConfig) { c.SandboxId = "" }, "sandbox_id"},
		{"empty WorkspaceImg", func(c *ch.TaskConfig) { c.WorkspaceImg = "" }, "workspace_img"},
		{"empty UserHomeImg", func(c *ch.TaskConfig) { c.UserHomeImg = "" }, "user_home_img"},
		{"empty PubkeyHex", func(c *ch.TaskConfig) { c.PubkeyHex = "" }, "pubkey_hex"},
		{"empty Kernel", func(c *ch.TaskConfig) { c.Kernel = "" }, "kernel"},
		{"zero CPUs", func(c *ch.TaskConfig) { c.CPUs = 0 }, "cpus"},
		{"zero MemoryMB", func(c *ch.TaskConfig) { c.MemoryMB = 0 }, "memory_mb"},
		{"VMIndex 0", func(c *ch.TaskConfig) { c.VMIndex = 0 }, "vm_index"},
		{"VMIndex too large", func(c *ch.TaskConfig) { c.VMIndex = 200 }, "vm_index"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := validColdBootConfig()
			tc.mutate(&cfg)
			_, err := ch.BuildConfigJSON(cfg, "/tmp/task")
			if err == nil {
				t.Fatalf("expected error for case %s, got nil", tc.name)
			}
			if !strings.Contains(err.Error(), tc.wantSubst) {
				t.Errorf("error %q does not mention field %q", err.Error(), tc.wantSubst)
			}
		})
	}
}

// TestStartTask_RejectsBadPubkeyHex covers the wrapper's hex format check:
// non-hex chars or odd length must fail before CH spawn.
func TestStartTask_RejectsBadPubkeyHex(t *testing.T) {
	cases := []struct {
		name     string
		pubkey   string
		wantWord string
	}{
		{"odd length", "abc", "odd length"},
		{"non-hex chars", "deadXYef", "non-hex"},
		{"unicode", "déadbeef", "non-hex"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := validColdBootConfig()
			cfg.PubkeyHex = tc.pubkey
			_, err := ch.BuildConfigJSON(cfg, "/tmp/task")
			if err == nil {
				t.Fatalf("expected error for pubkey=%q", tc.pubkey)
			}
			if !strings.Contains(err.Error(), tc.wantWord) {
				t.Errorf("error %q missing word %q", err.Error(), tc.wantWord)
			}
		})
	}
}

// TestStartTask_RejectsBadSandboxId covers the typed-id format check
// (alnum + underscore). A space-bearing sandbox_id would inject extra
// kernel cmdline tokens; we fail before spawn.
func TestStartTask_RejectsBadSandboxId(t *testing.T) {
	cases := []string{
		"sbx with space",
		"sbx;injected",
		"sbx=other",
		"sbx-dashed", // dashes are NOT in [0-9a-zA-Z_]
		"",           // empty
	}
	for _, bad := range cases {
		t.Run(bad, func(t *testing.T) {
			cfg := validColdBootConfig()
			cfg.SandboxId = bad
			_, err := ch.BuildConfigJSON(cfg, "/tmp/task")
			if err == nil {
				t.Fatalf("expected error for sandbox_id=%q", bad)
			}
			if !strings.Contains(err.Error(), "sandbox_id") {
				t.Errorf("error %q does not mention sandbox_id", err.Error())
			}
		})
	}
}

// fakeRunner records the argv it was constructed with, lets the test
// drive Wait()/exit through channels, and implements the processRunner
// contract enough for StartTask + WaitTask to exercise.
type fakeRunner struct {
	argv       []string
	dir        string
	pid        int
	mu         sync.Mutex
	startErr   error
	waitErr    error
	waitCh     chan struct{} // closed when Wait should unblock
	exitCode   int
	stderrBuf  []byte
	signalsRcv []os.Signal
	started    bool
	waited     bool
}

func newFakeRunner(cmd *exec.Cmd) *fakeRunner {
	return &fakeRunner{
		argv:   append([]string{cmd.Path}, cmd.Args[1:]...),
		dir:    cmd.Dir,
		pid:    424242, // deterministic, recognisable in failure output
		waitCh: make(chan struct{}),
	}
}

func (r *fakeRunner) Start() error {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.started = true
	return r.startErr
}

func (r *fakeRunner) Wait() error {
	<-r.waitCh
	r.mu.Lock()
	defer r.mu.Unlock()
	r.waited = true
	return r.waitErr
}

func (r *fakeRunner) Pid() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	if !r.started {
		return 0
	}
	return r.pid
}

func (r *fakeRunner) Signal(sig os.Signal) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.signalsRcv = append(r.signalsRcv, sig)
	return nil
}

func (r *fakeRunner) ExitCode() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.exitCode
}

func (r *fakeRunner) StderrTail(_ int) []byte {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.stderrBuf
}

// TestStartTask_SpawnInvokesCH end-to-end: builds a plugin with a fake
// runner factory, drives StartTask with a valid TaskConfig, asserts:
//   - the spawned argv vector includes --api-socket, --config, the
//     resolved CH binary
//   - the persisted TaskState carries the PID + APISocket + TapName the
//     fake runner reported
//
// The test sets ZSBX_CH_BIN to a synthetic stub so binary discovery
// resolves without requiring cloud-hypervisor on the test host.
func TestStartTask_SpawnInvokesCH(t *testing.T) {
	// Synthetic CH binary (just a stat target — the fake runner is what
	// "spawns" it).
	chBin := writeStubBinary(t, "cloud-hypervisor")
	chRemote := writeStubBinary(t, "ch-remote")
	t.Setenv("ZSBX_CH_BIN", chBin)
	t.Setenv("ZSBX_CH_REMOTE_BIN", chRemote)

	// Skip the real `ip link set up` (needs CAP_NET_ADMIN). The test
	// only asserts argv + persistence; T-3 owns tap correctness.
	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	// A fake "tap" name (the seam above no-ops it).
	tap := "test-tap-7"

	taskDir := t.TempDir()

	cfg := ch.TaskConfig{
		VMIndex:      7,
		Kernel:       "/opt/zsbx/vmlinuz",
		CPUs:         2,
		MemoryMB:     256,
		SandboxId:    "sbx_test",
		WorkspaceImg: "/tmp/ws.img",
		UserHomeImg:  "/tmp/uh.img",
		PubkeyHex:    "deadbeef",
		// Override the synthesised net entry so we point at "lo" (which
		// /sys/class/net/lo definitely exists). The wrapper auto-derives
		// for the production path; this is a test seam.
		Net: []ch.NetSpec{{Tap: tap, MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}},
	}

	// Capture the fake runner the factory creates so we can assert on it
	// post-StartTask.
	var capturedRunner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		r := newFakeRunner(cmd)
		capturedRunner = r
		return r
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)

	handle, _, err := p.StartTask(taskCfg)
	if err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if handle == nil {
		t.Fatal("StartTask returned nil handle")
	}
	if capturedRunner == nil {
		t.Fatal("runner factory not invoked")
	}

	// argv assertions.
	argv := capturedRunner.argv
	if len(argv) == 0 || argv[0] != chBin {
		t.Errorf("argv[0] = %q, want %q", argv[0], chBin)
	}
	if !containsAdjacent(argv, "--api-socket") {
		t.Errorf("argv missing --api-socket: %v", argv)
	}
	if !containsAdjacent(argv, "--config") {
		t.Errorf("argv missing --config: %v", argv)
	}

	// State assertions: the persisted TaskState should carry PID +
	// APISocket + TapName.
	var state ch.TaskState
	if err := handle.GetDriverState(&state); err != nil {
		t.Fatalf("GetDriverState: %v", err)
	}
	if state.CHPid != 424242 {
		t.Errorf("TaskState.CHPid = %d, want 424242", state.CHPid)
	}
	if !strings.HasSuffix(state.APISocket, "ch.sock") {
		t.Errorf("TaskState.APISocket = %q, want suffix ch.sock", state.APISocket)
	}
	if state.Tap != tap {
		t.Errorf("TaskState.Tap = %q, want %q", state.Tap, tap)
	}
	if state.Mode != "cold_boot" {
		t.Errorf("TaskState.Mode = %q, want cold_boot", state.Mode)
	}
	if state.SandboxId != cfg.SandboxId {
		t.Errorf("TaskState.SandboxId = %q, want %q", state.SandboxId, cfg.SandboxId)
	}
	if state.StartedAt.IsZero() {
		t.Error("TaskState.StartedAt is zero")
	}

	// Extract the --config path from the argv and read it back to
	// verify materialisation. Avoids hard-coding the run-dir layout
	// (driver-internal).
	configPath := argvAfter(argv, "--config")
	if configPath == "" {
		t.Fatal("--config not found in argv")
	}
	configBytes, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatalf("read %s: %v", configPath, err)
	}
	mustContain(t, "config.json contents", string(configBytes), "boot_vcpus")
	mustContain(t, "config.json contents", string(configBytes), "SANDBOX_AGENT_SANDBOX_ID=sbx_test")

	// Drive the fake runner to "exit" so WaitTask doesn't hang in a
	// follow-up test run.
	capturedRunner.mu.Lock()
	capturedRunner.exitCode = 0
	capturedRunner.waitErr = nil
	capturedRunner.mu.Unlock()
	close(capturedRunner.waitCh)
}

// TestStartTask_RejectsRestoreFrom asserts the T-6 guard: cold-boot path
// refuses if RestoreFrom is set, with the matching tag.
func TestStartTask_RejectsRestoreFrom(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	cfg := validColdBootConfig()
	cfg.RestoreFrom = "/some/snapshot/dir"

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected error for RestoreFrom set, got nil")
	}
	if !strings.Contains(err.Error(), "T-6") {
		t.Errorf("error %q missing T-6 tag", err.Error())
	}
}

// TestStartTask_WaitTaskSurfacesExitCode end-to-ends StartTask →
// fakeRunner.exit → WaitTask, asserting the exit code + stderr tail
// flow through the supervisor goroutine to the WaitTask subscriber.
func TestStartTask_WaitTaskSurfacesExitCode(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	var runner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		runner = newFakeRunner(cmd)
		runner.exitCode = 42
		runner.waitErr = errors.New("synthetic CH crash")
		runner.stderrBuf = []byte("Error opening block device file: No such file or directory\n")
		return runner
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)
	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}

	// Trigger the fake runner's Wait to return — supervisor will record
	// the exit + close exitDone.
	close(runner.waitCh)

	exitCh, err := p.WaitTask(context.Background(), taskCfg.ID)
	if err != nil {
		t.Fatalf("WaitTask: %v", err)
	}
	select {
	case result := <-exitCh:
		if result.ExitCode != 42 {
			t.Errorf("ExitCode = %d, want 42", result.ExitCode)
		}
		if result.Err == nil {
			t.Error("Err is nil; expected wrapped synthetic error")
		}
		if !strings.Contains(result.Err.Error(), "Error opening block device") {
			t.Errorf("Err missing stderr tail: %v", result.Err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("WaitTask did not return within 2s")
	}
}

// TestStartTask_DuplicateID asserts that a second StartTask with the same
// task ID returns ErrExistingTask without spawning a second VM.
func TestStartTask_DuplicateID(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	var runner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		runner = newFakeRunner(cmd)
		return runner
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)
	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("first StartTask: %v", err)
	}
	defer func() {
		if runner != nil {
			close(runner.waitCh)
		}
	}()

	if _, _, err := p.StartTask(taskCfg); !errors.Is(err, ch.ErrExistingTaskErr) {
		// Direct equality against the exported sentinel works too;
		// errors.Is is the future-safe path.
		if err == nil || !strings.Contains(err.Error(), "already running") {
			t.Errorf("second StartTask: got %v, want ErrExistingTask", err)
		}
	}
}

// -- helpers ---------------------------------------------------------

func mustContain(t *testing.T, label, haystack, needle string) {
	t.Helper()
	if !strings.Contains(haystack, needle) {
		t.Errorf("%s: missing %q in %q", label, needle, haystack)
	}
}

func containsAdjacent(argv []string, want string) bool {
	for _, a := range argv {
		if a == want {
			return true
		}
	}
	return false
}

// argvAfter returns the argument following `flag` in argv, or "" if not
// found. Used to extract values like `--config /tmp/x` → "/tmp/x".
func argvAfter(argv []string, flag string) string {
	for i, a := range argv {
		if a == flag && i+1 < len(argv) {
			return argv[i+1]
		}
	}
	return ""
}

// writeStubBinary creates an empty file in t.TempDir() so binary
// discovery (os.Stat) succeeds. We do NOT chmod +x — that's irrelevant
// because the fake runner never actually spawns it.
func writeStubBinary(t *testing.T, name string) string {
	t.Helper()
	dir := t.TempDir()
	path := dir + "/" + name
	if err := os.WriteFile(path, []byte("#!/bin/false\n"), 0o755); err != nil {
		t.Fatalf("write stub %s: %v", path, err)
	}
	return path
}

// newTestPluginWithFactory builds a *ch.Plugin and a matching
// drivers.TaskConfig such that StartTask can run against the fake
// runner. The taskDir is wired into the TaskConfig so config.json /
// rootfs.img / ch.sock all land under t.TempDir().
func newTestPluginWithFactory(
	t *testing.T,
	driverCfg *ch.TaskConfig,
	taskDir string,
	factory func(cmd *exec.Cmd) ch.ProcessRunnerSeam,
) (*ch.Plugin, *driversTaskConfig) {
	t.Helper()
	plugin := ch.NewPluginForTest(hclog.NewNullLogger(), factory)

	taskCfg := newDriversTaskConfig(t, driverCfg, taskDir)
	return plugin, taskCfg
}
