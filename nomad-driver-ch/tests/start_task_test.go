// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-1 sprint test surface: pins the CH config.json shape, the cold-boot
// validation guards, and the StartTask spawn argv via the processRunner
// seam in ch_client.go. Tests do NOT spawn a real cloud-hypervisor — the
// fake runner records argv and the test asserts on it.

package tests

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/drivers"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// validColdBootConfig returns a TaskConfig with every cold-boot required
// field set to a sane value. Helper used by all subtests so a per-test
// "this is what a good config looks like" doesn't drift between cases.
//
// C-2: the workspace/userhome image paths default to "" here so
// newDriversTaskConfig can rewrite them to per-test scratch paths under
// t.TempDir() before StartTask's pre-flight stat check runs. Pure
// BuildConfigJSON tests (which don't go through StartTask) set explicit
// paths after calling validColdBootConfig().
//
// Real production paths are under /var/lib/zsbx/img/... but the driver
// pre-flights against whatever the TaskConfig carries — the helper
// stages a stub file at each path before StartTask runs, and a
// per-test-tmpdir path keeps parallel test runs from racing.
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
//   - the spawned argv vector includes the long-argv flag set
//     (--api-socket, --kernel, --cmdline, --disk, --net, --memory,
//     --cpus, --console, --serial) — the C-1 fix shape, matching the
//     bash wrapper at nomad-vm-wrapper.sh:638-647
//   - the persisted TaskState carries the PID + APISocket + TapName the
//     fake runner reported
//   - config.json is still materialised under the task dir as a
//     debugging artifact (NOT passed to CH — see C-1 fix note)
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

	// argv assertions: full long-argv flag set (C-1 fix shape).
	argv := capturedRunner.argv
	if len(argv) == 0 || argv[0] != chBin {
		t.Errorf("argv[0] = %q, want %q", argv[0], chBin)
	}
	for _, want := range []string{"--api-socket", "--kernel", "--cmdline", "--disk", "--net", "--memory", "--cpus"} {
		if !containsAdjacent(argv, want) {
			t.Errorf("argv missing %s: %v", want, argv)
		}
	}
	// C-1 negative pin: --config must NOT appear in the spawn argv. CH
	// v51.1 rejects it with exit 2.
	if containsAdjacent(argv, "--config") {
		t.Errorf("argv unexpectedly contains --config (CH v51.1 rejects it): %v", argv)
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

	// config.json is still materialised under the task dir as a
	// debugging artifact (the JSON file is no longer fed to CH, but
	// it round-trips with the T-6 restore-path rewriter and is
	// operator-visible in the alloc dir). Use the api-socket path
	// to recover the run-dir.
	apiSocketPath := argvAfter(argv, "--api-socket")
	if apiSocketPath == "" {
		t.Fatal("--api-socket not found in argv")
	}
	runDir := filepath.Dir(apiSocketPath)
	configPath := filepath.Join(runDir, "config.json")
	configBytes, err := os.ReadFile(configPath)
	if err != nil {
		t.Fatalf("read %s (debug artifact): %v", configPath, err)
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

// TestStartTask_SpawnUsesApiSocketFlag pins the --api-socket flag
// position + value. The path must live under the per-task run dir
// (Nomad's NOMAD_TASK_DIR) and end in ch.sock so an operator looking
// at the alloc dir finds the CH control channel where they expect.
func TestStartTask_SpawnUsesApiSocketFlag(t *testing.T) {
	argv := captureColdBootArgv(t)
	got := argvAfter(argv, "--api-socket")
	if got == "" {
		t.Fatal("--api-socket missing from argv")
	}
	if !strings.HasSuffix(got, "ch.sock") {
		t.Errorf("--api-socket value = %q, want suffix ch.sock", got)
	}
}

// TestStartTask_SpawnPassesKernelPath pins --kernel <path>. The wrapper
// passes the kernel by path (typically /opt/zsbx/vmlinuz); the Go
// driver mirrors that 1:1 from TaskConfig.Kernel.
func TestStartTask_SpawnPassesKernelPath(t *testing.T) {
	argv := captureColdBootArgv(t)
	got := argvAfter(argv, "--kernel")
	if got != "/opt/zsbx/vmlinuz" {
		t.Errorf("--kernel value = %q, want /opt/zsbx/vmlinuz", got)
	}
}

// TestStartTask_SpawnPassesCmdlineWithSandboxId pins --cmdline. The
// kernel cmdline carries the agent identity tokens: SANDBOX_AGENT_SANDBOX_ID
// (R8-DEPLOY1) and zsbx_pubkey (controller signing key). Drift in the
// shape here breaks the guest's /sbin/init pubkey decode + the agent's
// OnceLock bind.
func TestStartTask_SpawnPassesCmdlineWithSandboxId(t *testing.T) {
	argv := captureColdBootArgv(t)
	got := argvAfter(argv, "--cmdline")
	if got == "" {
		t.Fatal("--cmdline missing from argv")
	}
	mustContain(t, "--cmdline value", got, "SANDBOX_AGENT_SANDBOX_ID=sbx_test")
	mustContain(t, "--cmdline value", got, "zsbx_pubkey=deadbeef")
	mustContain(t, "--cmdline value", got, "console=ttyS0")
	mustContain(t, "--cmdline value", got, "root=/dev/vda")
	mustContain(t, "--cmdline value", got, "ip=10.99.107.2::10.99.107.1:255.255.255.252::eth0:none")
}

// TestStartTask_SpawnPassesDisksWithVirtIOBlk pins the --disk flag
// shape. CH quirk (wrapper l.351-353): all disks share a SINGLE --disk
// flag with multiple `path=…` tokens as separate argv slots. The
// rootfs is the first disk; workspace + userhome follow.
func TestStartTask_SpawnPassesDisksWithVirtIOBlk(t *testing.T) {
	argv := captureColdBootArgv(t)

	// Find the --disk flag and the run of argv slots that follow before
	// the next `--flag` token. All of them belong to the single --disk.
	diskIdx := -1
	for i, a := range argv {
		if a == "--disk" {
			diskIdx = i
			break
		}
	}
	if diskIdx < 0 {
		t.Fatal("--disk missing from argv")
	}
	// Negative pin: a SECOND `--disk` flag would mean we mis-split
	// disks into multiple flags (CH's clap parser would only register
	// the LAST). Wrapper l.351-353 explicitly warns about this.
	for i := diskIdx + 1; i < len(argv); i++ {
		if argv[i] == "--disk" {
			t.Errorf("argv contains a SECOND --disk flag at %d; disks must share ONE --disk: %v", i, argv)
		}
	}

	var diskArgs []string
	for j := diskIdx + 1; j < len(argv); j++ {
		if strings.HasPrefix(argv[j], "--") {
			break
		}
		diskArgs = append(diskArgs, argv[j])
	}
	if len(diskArgs) != 3 {
		t.Fatalf("--disk arg count = %d (%v), want 3 (rootfs, workspace, userhome)", len(diskArgs), diskArgs)
	}
	// rootfs lives under the run dir (taskDir/rootfs.img).
	if !strings.Contains(diskArgs[0], "path=") || !strings.HasSuffix(strings.SplitN(diskArgs[0], ",", 2)[0], "rootfs.img") {
		t.Errorf("disk[0] = %q, want path=<run-dir>/rootfs.img,…", diskArgs[0])
	}
	mustContain(t, "disk[0]", diskArgs[0], "image_type=raw")
	mustContain(t, "disk[0]", diskArgs[0], "readonly=off")
	// workspace.img + userhome.img from TaskConfig.
	mustContain(t, "disk[1]", diskArgs[1], "path=/tmp/ws.img")
	mustContain(t, "disk[1]", diskArgs[1], "image_type=raw")
	mustContain(t, "disk[2]", diskArgs[2], "path=/tmp/uh.img")
	mustContain(t, "disk[2]", diskArgs[2], "image_type=raw")
}

// TestStartTask_SpawnPassesNetTapMac pins --net tap=…,mac=… . The tap
// name + MAC come from the resolved NetSpec (operator-supplied or
// auto-derived); we test the operator-supplied path here.
func TestStartTask_SpawnPassesNetTapMac(t *testing.T) {
	argv := captureColdBootArgv(t)
	got := argvAfter(argv, "--net")
	if got == "" {
		t.Fatal("--net missing from argv")
	}
	mustContain(t, "--net value", got, "tap=test-tap-7")
	mustContain(t, "--net value", got, "mac=12:34:56:78:9b:07")
}

// TestStartTask_SpawnPassesMemoryAndCpus pins --memory size=…,shared=on
// and --cpus boot=… . Shape comes from TaskConfig.MemoryMB / CPUs.
func TestStartTask_SpawnPassesMemoryAndCpus(t *testing.T) {
	argv := captureColdBootArgv(t)
	mem := argvAfter(argv, "--memory")
	if mem != "size=256M,shared=on" {
		t.Errorf("--memory value = %q, want size=256M,shared=on", mem)
	}
	cpus := argvAfter(argv, "--cpus")
	if cpus != "boot=2" {
		t.Errorf("--cpus value = %q, want boot=2", cpus)
	}
}

// TestStartTask_DoesNotPassConfigFlag is the C-1 negative pin: if a
// future refactor reintroduces `--config <path>`, this test fires
// before the cluster smoke does. CH v51.1 rejects --config with exit
// 2 ("error: unexpected argument '--config' found"), so this guards
// against a silent regression.
func TestStartTask_DoesNotPassConfigFlag(t *testing.T) {
	argv := captureColdBootArgv(t)
	if containsAdjacent(argv, "--config") {
		t.Errorf("argv unexpectedly contains --config (CH v51.1 has no such flag): %v", argv)
	}
}

// TestStartTask_DiskPreflight_FailsOnMissingPath is the C-2 negative
// pin: when a disk image referenced by TaskConfig doesn't exist on the
// host at spawn time, StartTask must surface a CLEAR error naming the
// offending path BEFORE invoking CH. Without this guard the failure
// manifests as a generic CH crash:
//
//	VmBoot(VmBoot(DeviceManager(Disk(Os{code:2, kind:NotFound, ...}))))
//
// — which leaves the operator hunting through CH's serial log for the
// path string. Closes C-2 from T-8b-smoke-retry-r3.
func TestStartTask_DiskPreflight_FailsOnMissingPath(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := validColdBootConfig()
	// Explicit operator-supplied Disks short-circuits the rootfs-stage
	// branch (so the rootfs failure mode doesn't mask the workspace
	// failure mode this test wants to exercise).
	cfg.Disks = []ch.DiskSpec{
		{Path: "/tmp/rootfs-this-test-staged.img", Readonly: false, Serial: "zsbx-root"},
		{Path: "/tmp/this-path-does-not-exist-pre-flight.img", Readonly: false, Serial: "zsbx-work"},
	}
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	// Stage ONLY the first disk; leave the second deliberately missing
	// so the pre-flight stat catches it.
	if err := os.WriteFile(cfg.Disks[0].Path, []byte("x"), 0o600); err != nil {
		t.Fatalf("pre-stage disk[0]: %v", err)
	}
	t.Cleanup(func() { _ = os.Remove(cfg.Disks[0].Path) })
	// Ensure the second path is truly absent before we run.
	_ = os.Remove(cfg.Disks[1].Path)

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected pre-flight error for missing disk, got nil")
	}
	if !strings.Contains(err.Error(), "does not exist") {
		t.Errorf("error %q must mention 'does not exist'", err.Error())
	}
	if !strings.Contains(err.Error(), cfg.Disks[1].Path) {
		t.Errorf("error %q must mention the offending path %q", err.Error(), cfg.Disks[1].Path)
	}
	// Operator-facing: prefix MUST be the driver tag so the Nomad task
	// log line is greppable.
	if !strings.Contains(err.Error(), "ch: StartTask") {
		t.Errorf("error %q must carry the driver prefix 'ch: StartTask'", err.Error())
	}
}

// TestStartTask_DiskPreflight_FailsOnEmptyFile is the related guard for
// the size>0 check: a disk image present but never written
// (controller staged the path with `truncate` but mkfs.ext4 failed
// silently) would have CH read 0 bytes and panic. Surface it here
// instead.
func TestStartTask_DiskPreflight_FailsOnEmptyFile(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	emptyDisk := filepath.Join(t.TempDir(), "empty.img")
	if err := os.WriteFile(emptyDisk, nil, 0o600); err != nil {
		t.Fatalf("create empty disk: %v", err)
	}

	cfg := validColdBootConfig()
	cfg.Disks = []ch.DiskSpec{
		{Path: emptyDisk, Readonly: false, Serial: "zsbx-root"},
	}
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected pre-flight error for empty disk, got nil")
	}
	if !strings.Contains(err.Error(), "empty") {
		t.Errorf("error %q must mention 'empty'", err.Error())
	}
}

// TestStartTask_DiskPreflight_PureFn exercises PreflightDiskPaths in
// isolation — pure-function tests don't need a Plugin scaffolding and
// run instantly. Pins the failure shape: first offender wins, error
// names the path + the (driver-tagged) prefix.
func TestStartTask_DiskPreflight_PureFn(t *testing.T) {
	good := filepath.Join(t.TempDir(), "good.img")
	if err := os.WriteFile(good, []byte("x"), 0o600); err != nil {
		t.Fatalf("write good.img: %v", err)
	}
	missing := "/tmp/zsbx-test-missing-on-purpose.img"
	_ = os.Remove(missing)

	cases := []struct {
		name     string
		disks    []ch.DiskSpec
		wantSub  []string
		wantErr  bool
	}{
		{
			name:    "all good → no error",
			disks:   []ch.DiskSpec{{Path: good}},
			wantErr: false,
		},
		{
			name:    "empty path → error names index",
			disks:   []ch.DiskSpec{{Path: ""}},
			wantSub: []string{"empty path", "disk[0]"},
			wantErr: true,
		},
		{
			name:    "missing file → error names path + 'does not exist'",
			disks:   []ch.DiskSpec{{Path: good}, {Path: missing}},
			wantSub: []string{missing, "does not exist", "disk[1]"},
			wantErr: true,
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			err := ch.PreflightDiskPaths(tc.disks)
			if (err != nil) != tc.wantErr {
				t.Fatalf("err=%v wantErr=%v", err, tc.wantErr)
			}
			if err == nil {
				return
			}
			for _, sub := range tc.wantSub {
				if !strings.Contains(err.Error(), sub) {
					t.Errorf("error %q missing %q", err.Error(), sub)
				}
			}
		})
	}
}

// TestStartTask_RootfsStaged_FromArtifactDir is the happy-path C-2 test:
// asserts that StartTask copies $ZSBX_ARTIFACT_DIR/rootfs-slim.img into
// the run dir's rootfs.img before CH spawn, mirroring the wrapper at
// nomad-vm-wrapper.sh:300-305. The argv-shape tests already pin the
// disk[0] path; this test pins the on-disk effect.
func TestStartTask_RootfsStaged_FromArtifactDir(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	var captured *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		captured = newFakeRunner(cmd)
		return captured
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if captured == nil {
		t.Fatal("runner factory not invoked")
	}
	t.Cleanup(func() { close(captured.waitCh) })

	// Recover the run dir from the --api-socket argv slot.
	apiSocket := argvAfter(captured.argv, "--api-socket")
	if apiSocket == "" {
		t.Fatal("--api-socket missing from argv")
	}
	rootfsPath := filepath.Join(filepath.Dir(apiSocket), "rootfs.img")
	info, err := os.Stat(rootfsPath)
	if err != nil {
		t.Fatalf("rootfs.img not staged at %s: %v", rootfsPath, err)
	}
	if info.Size() == 0 {
		t.Errorf("rootfs.img exists but is empty — copy step likely no-op'd")
	}
	// The stub content the test helper writes is "stub-rootfs" (11
	// bytes). If the size differs the copy reached into a wrong
	// source — pin the exact length.
	if info.Size() != int64(len("stub-rootfs")) {
		t.Errorf("rootfs.img size = %d, want %d (content-matched copy from artifact dir)",
			info.Size(), len("stub-rootfs"))
	}
}

// TestStartTask_RootfsStaged_FailsWhenArtifactDirEnvMissing covers the
// failure path: the controller MUST emit ZSBX_ARTIFACT_DIR on every
// job (nomad_ch.rs:2339 + l.2495). If it doesn't, StartTask should
// surface a clear error naming the missing env var.
func TestStartTask_RootfsStaged_FailsWhenArtifactDirEnvMissing(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})

	// Strip the artifact-dir env var the helper installed.
	delete(taskCfg.Env, ch.ChArtifactDirEnvVar)

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected error when ZSBX_ARTIFACT_DIR is missing, got nil")
	}
	if !strings.Contains(err.Error(), ch.ChArtifactDirEnvVar) {
		t.Errorf("error %q must name the missing env var %q", err.Error(), ch.ChArtifactDirEnvVar)
	}
}

// TestStartTask_RootfsStaged_Idempotent: a second StartTask attempt that
// finds the per-task rootfs already in place must NOT re-copy. Mirrors
// the wrapper's `[ ! -f "$DISK" ]` guard at nomad-vm-wrapper.sh:300.
// Idempotency matters under Nomad-client restart — RecoverTask can
// resurface a task whose run dir survived the restart.
func TestStartTask_RootfsStaged_Idempotent(t *testing.T) {
	// Pure unit on materializeRootfs — happy path is "src exists, dst
	// exists, dst is left untouched".
	artifact := t.TempDir()
	src := filepath.Join(artifact, ch.ChRootfsSourceName)
	if err := os.WriteFile(src, []byte("fresh"), 0o600); err != nil {
		t.Fatalf("write src: %v", err)
	}
	dst := filepath.Join(t.TempDir(), "rootfs.img")
	if err := os.WriteFile(dst, []byte("preexisting"), 0o600); err != nil {
		t.Fatalf("write dst: %v", err)
	}
	if err := ch.MaterializeRootfs(artifact, dst); err != nil {
		t.Fatalf("MaterializeRootfs idempotent path: %v", err)
	}
	got, err := os.ReadFile(dst)
	if err != nil {
		t.Fatalf("read dst: %v", err)
	}
	if string(got) != "preexisting" {
		t.Errorf("dst content = %q, want unchanged 'preexisting' (idempotent)", got)
	}
}

// TestStartTask_RootfsStaged_FailsWhenSourceMissing: the artifact dir
// exists but rootfs-slim.img inside it does not. The driver must
// surface a clear ENOENT-with-path error before invoking CH.
func TestStartTask_RootfsStaged_FailsWhenSourceMissing(t *testing.T) {
	artifact := t.TempDir()
	// Deliberately do NOT write rootfs-slim.img.
	dst := filepath.Join(t.TempDir(), "rootfs.img")
	err := ch.MaterializeRootfs(artifact, dst)
	if err == nil {
		t.Fatal("expected error for missing src rootfs, got nil")
	}
	if !strings.Contains(err.Error(), ch.ChRootfsSourceName) {
		t.Errorf("error %q must name the missing file %q", err.Error(), ch.ChRootfsSourceName)
	}
}

// TestStartTask_SpawnArgvShape_PureFn exercises BuildSpawnArgv as a
// pure function — no plugin / runner / taskdir plumbing. Pins the
// argv shape against the wrapper's cold-boot block. Independent of
// the end-to-end StartTask test so a regression in the helper surfaces
// directly.
func TestStartTask_SpawnArgvShape_PureFn(t *testing.T) {
	cfg := validColdBootConfig()
	disks := []ch.DiskSpec{
		{Path: "/run/zsbx/rootfs.img", Readonly: false, Serial: "zsbx-root"},
		{Path: "/var/lib/zsbx/img/workspace.img", Readonly: false, Serial: "zsbx-work"},
		{Path: "/var/lib/zsbx/img/userhome.img", Readonly: false, Serial: "zsbx-home"},
	}
	net := ch.NetSpec{Tap: "zsbx-nm-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}
	argv := ch.BuildSpawnArgv(
		"/usr/bin/cloud-hypervisor",
		"/run/zsbx/ch.sock",
		cfg,
		"console=ttyS0 root=/dev/vda zsbx_pubkey=deadbeef SANDBOX_AGENT_SANDBOX_ID=sbx_test123",
		disks,
		net,
		"/run/zsbx/serial.log",
	)

	// argv[0] is the binary; the rest must include the wrapper's flag
	// set in order. We don't pin exact positions (allows reordering
	// adjacent flags) but we do pin: (a) each flag appears, (b) --disk
	// is followed by 3 contiguous disk args, (c) --config never appears.
	if argv[0] != "/usr/bin/cloud-hypervisor" {
		t.Errorf("argv[0] = %q, want /usr/bin/cloud-hypervisor", argv[0])
	}
	wantFlags := []string{"--api-socket", "--kernel", "--cmdline", "--disk", "--net", "--memory", "--cpus", "--console", "--serial"}
	for _, f := range wantFlags {
		if !containsAdjacent(argv, f) {
			t.Errorf("argv missing %s: %v", f, argv)
		}
	}
	if containsAdjacent(argv, "--config") {
		t.Errorf("BuildSpawnArgv must NOT emit --config: %v", argv)
	}
	if got := argvAfter(argv, "--memory"); got != "size=512M,shared=on" {
		t.Errorf("--memory = %q, want size=512M,shared=on", got)
	}
	if got := argvAfter(argv, "--cpus"); got != "boot=2" {
		t.Errorf("--cpus = %q, want boot=2", got)
	}
	if got := argvAfter(argv, "--console"); got != "off" {
		t.Errorf("--console = %q, want off", got)
	}
	if got := argvAfter(argv, "--serial"); got != "file=/run/zsbx/serial.log" {
		t.Errorf("--serial = %q, want file=/run/zsbx/serial.log", got)
	}
}

// captureColdBootArgv is the shared setup for all SpawnPasses* sub-tests:
// stand up a plugin with a fake runner, drive StartTask once with a
// canonical valid TaskConfig, return the recorded argv. The fake runner
// is left blocked in Wait — captureColdBootArgv schedules a t.Cleanup
// that unblocks it so the supervisor goroutine reaps.
func captureColdBootArgv(t *testing.T) []string {
	t.Helper()
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := ch.TaskConfig{
		VMIndex:      7,
		Kernel:       "/opt/zsbx/vmlinuz",
		CPUs:         2,
		MemoryMB:     256,
		SandboxId:    "sbx_test",
		WorkspaceImg: "/tmp/ws.img",
		UserHomeImg:  "/tmp/uh.img",
		PubkeyHex:    "deadbeef",
		Net:          []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}},
	}

	var capturedRunner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		r := newFakeRunner(cmd)
		capturedRunner = r
		return r
	}
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)
	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if capturedRunner == nil {
		t.Fatal("runner factory not invoked")
	}
	t.Cleanup(func() {
		if capturedRunner != nil {
			capturedRunner.mu.Lock()
			done := false
			select {
			case <-capturedRunner.waitCh:
				done = true
			default:
			}
			capturedRunner.mu.Unlock()
			if !done {
				close(capturedRunner.waitCh)
			}
		}
	})
	return capturedRunner.argv
}

// TestStartTask_RestoreFromRoutesToRestoreBranch asserts that
// setting RestoreFrom flips StartTask to the restore branch (T-6).
// With the snapshot dir missing, the restore branch surfaces a
// snapshot-validation error — that's the routing signal.
func TestStartTask_RestoreFromRoutesToRestoreBranch(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	cfg := validColdBootConfig()
	cfg.RestoreFrom = "/some/snapshot/dir/that/does/not/exist"

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		return newFakeRunner(cmd)
	})

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected error for missing snapshot dir, got nil")
	}
	if !strings.Contains(err.Error(), "restore") {
		t.Errorf("error %q does not mention restore branch", err.Error())
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

// TestStartTask_RollsBackTapOnSpawnFailure pins the G4 fix: when CH
// spawn fails (fakeRunner.Start returns an error) AFTER setupTapForVM
// has created the tap, StartTask must call teardownTap exactly once
// with the same tap name, AND must not register the task in p.tasks
// (otherwise DestroyTask would try to tear it down a second time).
//
// Without the deferred rollback the tap would leak: the task is never
// persisted to p.tasks → DestroyTask never fires → the tap survives
// until the next cold-boot at the same vm_index re-creates it
// (which is idempotent today, but a defense-in-depth hole nonetheless).
func TestStartTask_RollsBackTapOnSpawnFailure(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	// Fake the tap setup so we don't need CAP_NET_ADMIN AND so we can
	// observe whether teardown is called against the tap setup returned.
	var (
		setupCalls    atomic.Int32
		teardownCalls atomic.Int32
		setupTap      string
		teardownTap   string
		mu            sync.Mutex
	)
	prevSetup := ch.SetSetupTapForTest(func(idx uint16, base uint8) (string, error) {
		setupCalls.Add(1)
		name := fmt.Sprintf("zsbx-nm-%d", idx)
		mu.Lock()
		setupTap = name
		mu.Unlock()
		return name, nil
	})
	t.Cleanup(func() { ch.SetSetupTapForTest(prevSetup) })

	prevTeardown := ch.SetTeardownTapForTest(func(name string) error {
		teardownCalls.Add(1)
		mu.Lock()
		teardownTap = name
		mu.Unlock()
		return nil
	})
	t.Cleanup(func() { ch.SetTeardownTapForTest(prevTeardown) })

	cfg := validColdBootConfig()
	cfg.VMIndex = 7
	// CRUCIAL: leave cfg.Net unset so StartTask routes through
	// setupTapForVM (the rollback-armed branch).

	// Factory returns a fake runner whose Start() fails — this is the
	// most likely real-world trigger for the leak (CH binary present
	// but execve fails for env reasons, ENOMEM, EACCES on a referenced
	// file, etc).
	spawnErr := errors.New("synthetic spawn failure")
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		r := newFakeRunner(cmd)
		r.startErr = spawnErr
		return r
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected StartTask to fail when runner.Start errors, got nil")
	}
	if !errors.Is(err, spawnErr) {
		t.Errorf("err = %v, want wrap of spawnErr", err)
	}

	if got := setupCalls.Load(); got != 1 {
		t.Errorf("setupTap call count = %d, want 1", got)
	}
	if got := teardownCalls.Load(); got != 1 {
		t.Errorf("teardownTap call count = %d, want 1 (rollback should fire on spawn failure)", got)
	}
	mu.Lock()
	if teardownTap != setupTap {
		t.Errorf("teardown called with %q, want %q (the tap setupTapForVM returned)", teardownTap, setupTap)
	}
	mu.Unlock()

	// Task must NOT be registered (StartTask failed). InspectTask
	// surfaces drivers.ErrTaskNotFound in that case.
	if _, err := p.InspectTask(taskCfg.ID); !errors.Is(err, drivers.ErrTaskNotFound) {
		t.Errorf("InspectTask after failed StartTask = %v, want ErrTaskNotFound", err)
	}
}

// TestStartTask_DoesNotRollBackOnSuccess is the negative half of the
// rollback contract: on a happy-path StartTask, teardownTap must NOT
// be called. The deferred rollback fires only when tapRollback is
// still true at function return — once handle.SetDriverState succeeds
// and the task is registered, the toggle flips to false.
func TestStartTask_DoesNotRollBackOnSuccess(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	var (
		setupCalls    atomic.Int32
		teardownCalls atomic.Int32
	)
	prevSetup := ch.SetSetupTapForTest(func(idx uint16, _ uint8) (string, error) {
		setupCalls.Add(1)
		return fmt.Sprintf("zsbx-nm-%d", idx), nil
	})
	t.Cleanup(func() { ch.SetSetupTapForTest(prevSetup) })

	prevTeardown := ch.SetTeardownTapForTest(func(string) error {
		teardownCalls.Add(1)
		return nil
	})
	t.Cleanup(func() { ch.SetTeardownTapForTest(prevTeardown) })

	cfg := validColdBootConfig()
	cfg.VMIndex = 7
	// Leave Net unset → routes through setupTapForVM.

	var runner *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		runner = newFakeRunner(cmd)
		return runner
	}

	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	handle, _, err := p.StartTask(taskCfg)
	if err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if handle == nil {
		t.Fatal("nil handle on happy path")
	}

	if got := setupCalls.Load(); got != 1 {
		t.Errorf("setupTap call count = %d, want 1", got)
	}
	if got := teardownCalls.Load(); got != 0 {
		t.Errorf("teardownTap call count = %d, want 0 on happy path (rollback must NOT fire)", got)
	}

	// Sanity: the task IS registered.
	if _, err := p.InspectTask(taskCfg.ID); err != nil {
		t.Errorf("InspectTask after successful StartTask: %v", err)
	}

	// Unblock the fake runner's Wait so the supervisor goroutine
	// doesn't hang the test process.
	if runner != nil {
		close(runner.waitCh)
	}
}

// TestStartTask_EmitsTracePoints (T-10) pins the three CREATE-path
// trace points the controller-side r32-T1 review identified as the
// missing attribution surface for the ~700ms "client+driver dispatch"
// segment between Nomad's alloc_first_seen and alloc-running.
//
// The trace points are:
//   - "start_task: entry"          — emitted at the top of StartTask
//   - "start_task: ch_spawned"     — emitted right after runner.Start()
//     returns successfully
//   - "start_task: handle_returned" — emitted right before the happy-path
//     return of TaskHandle to Nomad
//
// All three MUST appear in the captured logger output, in order. If a
// future refactor drops or reorders any of them the cluster review's
// attribution math breaks silently, so we pin the contract here.
func TestStartTask_EmitsTracePoints(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	prev := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prev) })

	cfg := validColdBootConfig()
	cfg.Net = []ch.NetSpec{{Tap: "test-tap-7", MAC: "12:34:56:78:9b:07", IP: "10.99.107.2", Mask: "255.255.255.252"}}

	var captured *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		captured = newFakeRunner(cmd)
		return captured
	}

	// hclog with a *bytes.Buffer sink so we can assert on raw log lines.
	// Info level matches the production emit level on the trace points.
	var buf bytes.Buffer
	logger := hclog.New(&hclog.LoggerOptions{
		Name:   "ch-test",
		Level:  hclog.Info,
		Output: &buf,
	})
	plugin := ch.NewPluginForTest(logger, factory)

	taskCfg := newDriversTaskConfig(t, &cfg, t.TempDir())

	if _, _, err := plugin.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask: %v", err)
	}
	if captured == nil {
		t.Fatal("runner factory not invoked")
	}
	t.Cleanup(func() { close(captured.waitCh) })

	out := buf.Bytes()

	// All three trace points must appear in the captured output.
	for _, want := range [][]byte{
		[]byte("start_task: entry"),
		[]byte("start_task: ch_spawned"),
		[]byte("start_task: handle_returned"),
	} {
		if !bytes.Contains(out, want) {
			t.Errorf("missing trace point %q in log output:\n%s", want, out)
		}
	}

	// And they must appear in the order above — the attribution math
	// (pre-spawn driver work vs. post-spawn handle persist) only works
	// if the emit order matches the code-path order.
	entryIdx := bytes.Index(out, []byte("start_task: entry"))
	spawnedIdx := bytes.Index(out, []byte("start_task: ch_spawned"))
	returnedIdx := bytes.Index(out, []byte("start_task: handle_returned"))
	if !(entryIdx < spawnedIdx && spawnedIdx < returnedIdx) {
		t.Errorf("trace points out of order: entry=%d ch_spawned=%d handle_returned=%d\noutput:\n%s",
			entryIdx, spawnedIdx, returnedIdx, out)
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
