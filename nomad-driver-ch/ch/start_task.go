// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go::StartTask) on
// 2026-05-25 for Cloud Hypervisor support. The libvirt CreateVM/cloud-init
// path is replaced by a direct CH spawn that mirrors the existing bash
// wrapper at crates/sandbox/scripts/nomad-vm-wrapper.sh (cold-boot branch,
// pre-restore).
//
// T-1 (this implementation) covers steps 1-4 of the wrapper: tap re-up
// (basic — T-3 owns the deep tap work), env validation, config.json
// materialisation for cloud-hypervisor, and the CH spawn itself with
// --api-socket / --config / --serial. The wait (cmd.Wait) is plumbed
// through to WaitTask via the processRunner seam introduced in ch_client.go.

package ch

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/hashicorp/nomad/plugins/drivers"
)

// chRootfsName is the file name the CH config refers to for the per-VM
// rootfs copy. The wrapper writes ${ZSBX_RUNTIME}/rootfs.img; we write
// ${cfg.TaskDir().Dir}/rootfs.img. Same convention so any operator
// looking at the alloc dir sees the same layout.
const chRootfsName = "rootfs.img"

// chAPISocketName / chSerialLogName / chConfigName are the conventional
// file names within the per-task run dir. Match the wrapper's names for
// operator familiarity.
const (
	chAPISocketName = "ch.sock"
	chSerialLogName = "serial.log"
	chConfigName    = "config.json"
)

// StartTask brings up a Cloud Hypervisor VM for the given task. Cold-boot
// flow (mirrors the bash wrapper):
//
//  1. Decode TaskConfig from cfg.DecodeDriverConfig.
//  2. Validate cold-boot env (SandboxId, WorkspaceImg, UserHomeImg,
//     PubkeyHex, Kernel) — fail loudly with field name.
//  3. Set up the tap (basic `ip link set <tap> up`; deeper provisioning
//     is T-3). The host operator is expected to have pre-created the
//     device.
//  4. Materialise the per-task run dir, write config.json the CH
//     `--config` flag will load.
//  5. Spawn cloud-hypervisor as a subprocess via the processRunner seam
//     (so tests can fake-spawn).
//  6. Persist TaskState (PID, APISocket, TapName, StartedAt, …) via
//     handle.SetDriverState — the persistence point RecoverTask (T-4)
//     reads from.
//  7. Register the in-memory taskHandle in p.tasks and return.
//
// Restore path (T-6) is NOT implemented here; an explicit guard short-
// circuits with a T-6 error if RestoreFrom is set so the cold-boot path
// stays unambiguous.
//
// On any failure path StartTask rolls back what it created (run dir,
// in-memory handle); the tap is left for T-3 / the host setup script to
// own.
func (p *Plugin) StartTask(cfg *drivers.TaskConfig) (*drivers.TaskHandle, *drivers.DriverNetwork, error) {
	if cfg == nil {
		return nil, nil, errors.New("ch: StartTask: nil TaskConfig")
	}
	if _, ok := p.tasks.Get(cfg.ID); ok {
		return nil, nil, ErrExistingTask
	}

	var driverConfig TaskConfig
	if err := cfg.DecodeDriverConfig(&driverConfig); err != nil {
		return nil, nil, fmt.Errorf("ch: StartTask: decode driver config: %w", err)
	}

	if driverConfig.RestoreFrom != "" {
		// Restore is T-6's sprint. Refuse explicitly with the matching
		// tag so the operator gets a clean "not yet implemented" rather
		// than a half-built cold-boot artifact.
		return nil, nil, errors.New("ch: T-6: restore path not implemented (RestoreFrom set)")
	}

	// Validate the cold-boot subset of fields. Each error names the field
	// so an operator looking at the Nomad task log knows what to fix.
	if err := validateColdBoot(&driverConfig); err != nil {
		return nil, nil, err
	}

	mode := modeOf(&driverConfig)
	p.logger.Info("ch: StartTask",
		"task_id", cfg.ID,
		"task_name", cfg.Name,
		"vm_index", driverConfig.VMIndex,
		"sandbox_id", driverConfig.SandboxId,
		"mode", mode)

	// Resolve cloud-hypervisor binary now (re-resolve in case Config was
	// applied after NewClient, or the host's PATH changed between probe
	// and now).
	chBin := p.chClient.CHBin()
	if chBin == "" {
		// Try once more with the configured Config path.
		if p.config != nil {
			p.chClient.SetBinaries(p.config.CloudHypervisorBin, p.config.VirtiofsdBin)
			chBin = p.chClient.CHBin()
		}
	}
	if chBin == "" {
		return nil, nil, errors.New("ch: StartTask: cloud-hypervisor binary not found (set ZSBX_CH_BIN or config.cloud_hypervisor_bin)")
	}

	// Per-task run dir under cfg.TaskDir().Dir (Nomad's NOMAD_TASK_DIR).
	// Falls back to p.config.RunDir if Nomad hasn't issued a task dir
	// (shouldn't happen in production but happens in tests).
	runDir := taskRunDir(cfg, p.config)
	if err := os.MkdirAll(runDir, 0o755); err != nil {
		return nil, nil, fmt.Errorf("ch: StartTask: mkdir runDir %s: %w", runDir, err)
	}

	apiSocket := filepath.Join(runDir, chAPISocketName)
	serialLog := filepath.Join(runDir, chSerialLogName)
	configPath := filepath.Join(runDir, chConfigName)
	rootfsPath := filepath.Join(runDir, chRootfsName)

	// Clear any stale API socket from a crashed prior run. Matches the
	// wrapper's `rm -f "$API_SOCK"`. Defensive only — Nomad gives a
	// fresh task dir per alloc.
	_ = os.Remove(apiSocket)

	// Resolve the tap + network shape for this VM. Mirrors the wrapper:
	//   TAP=zsbx-nm-${VMIndex}
	//   VM_IP=10.${BASE}.${100+VMIndex}.2
	//   HOST_IP=10.${BASE}.${100+VMIndex}.1
	//   MAC=12:34:56:78:9b:${VMIndex hex}
	tapName, netSpec := resolveNet(&driverConfig)

	// Tap setup (T-3). Replaces T-1's "expect the operator pre-created
	// the tap" stub with a full per-VM /30 host-side plumbing:
	//   ip tuntap add dev <tap> mode tap user nobody
	//   ip addr add  <host_ip>/30 dev <tap>
	//   ip link set  dev <tap> up
	// Idempotent on each step — a Nomad-client restart that left a tap
	// behind doesn't fail StartTask. Operator-supplied Net entries
	// short-circuit to the legacy "operator owns the tap, we just bring
	// it up" path so an externally-managed network config still works.
	if len(driverConfig.Net) > 0 {
		// Operator provided an explicit Net entry — trust them. Best-
		// effort up-the-link (the tap may already be up; ip link set is
		// idempotent). Failure is FATAL: an operator-pinned tap that
		// isn't routable is a configuration bug we want surfaced in the
		// task log, not silently smoothed over.
		if err := ensureTapUp(tapName); err != nil {
			return nil, nil, fmt.Errorf("ch: StartTask: tap %s not ready: %w", tapName, err)
		}
	} else {
		base := uint8(driverConfig.SubnetBaseOctet)
		if driverConfig.SubnetBaseOctet == 0 {
			base = defaultSubnetBaseOctet
		}
		if _, err := setupTapForVM(driverConfig.VMIndex, base); err != nil {
			return nil, nil, fmt.Errorf("ch: StartTask: setup tap for vm_index=%d: %w", driverConfig.VMIndex, err)
		}
	}

	// Build the kernel cmdline. If the operator already supplied one,
	// trust it verbatim; otherwise synthesise from the wrapper-equivalent
	// fields. The synthesis mirrors the wrapper line-for-line.
	cmdline := driverConfig.Cmdline
	if cmdline == "" {
		cmdline = synthesizeCmdline(&driverConfig, netSpec)
	}

	// Materialise the per-VM rootfs copy if it doesn't already exist.
	// The wrapper does `cp --reflink=auto $ARTIFACT_DIR/rootfs-slim.img
	// $DISK`. We don't have ARTIFACT_DIR here; the operator is expected
	// to either pre-stage the rootfs at runDir/rootfs.img OR include it
	// as the first entry of Disks. Honour Disks if non-empty; otherwise
	// synthesise from WorkspaceImg / UserHomeImg with a rootfs we expect
	// to already exist at rootfsPath.
	disks := driverConfig.Disks
	if len(disks) == 0 {
		disks = []DiskSpec{
			{Path: rootfsPath, Readonly: false, Serial: "zsbx-root"},
			{Path: driverConfig.WorkspaceImg, Readonly: false, Serial: "zsbx-work"},
			{Path: driverConfig.UserHomeImg, Readonly: false, Serial: "zsbx-home"},
		}
	}

	// Build the CH config.json. We use --config rather than the wrapper's
	// long argv list because it's trivially testable (one JSON document
	// pinned by tests) and lets us round-trip config through a snapshot
	// rewriter the same way T-6's restore branch will need to.
	chConfig := buildCHConfig(&driverConfig, cmdline, disks, []NetSpec{netSpec}, serialLog)
	configBytes, err := json.MarshalIndent(chConfig, "", "  ")
	if err != nil {
		return nil, nil, fmt.Errorf("ch: StartTask: marshal config: %w", err)
	}
	if err := os.WriteFile(configPath, configBytes, 0o600); err != nil {
		return nil, nil, fmt.Errorf("ch: StartTask: write config %s: %w", configPath, err)
	}

	// Spawn CH via the processRunner seam.
	argv := []string{
		chBin,
		"--api-socket", apiSocket,
		"--config", configPath,
	}
	cmd := exec.Command(argv[0], argv[1:]...)
	cmd.Dir = runDir
	// Stdout → CH's own log file is preferable; default exec.Cmd
	// inherits parent stdout/stderr which Nomad reads. Keep stdout as
	// inherited (CH is quiet on stdout); stderr goes through the
	// tailBuffer that the runner factory installs.
	cmd.Stdout = nil

	runner := p.chClient.RunnerFactory()(cmd)
	if err := runner.Start(); err != nil {
		// Roll back the run-dir artifacts we created. The tap is left
		// alone (it predates StartTask).
		_ = os.Remove(configPath)
		return nil, nil, fmt.Errorf("ch: StartTask: spawn cloud-hypervisor: %w", err)
	}

	startedAt := time.Now().UTC()
	pid := runner.Pid()

	// Persistence record. RecoverTask (T-4) reads this back; the field
	// set MUST be sufficient for RecoverTask to re-attach to the running
	// VM after a Nomad-client restart.
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
		// Best-effort kill the freshly-spawned CH so we don't orphan it.
		_ = runner.Signal(os.Kill)
		_ = os.Remove(configPath)
		return nil, nil, fmt.Errorf("ch: StartTask: persist TaskState: %w", err)
	}

	// In-memory handle, registered atomically. The exitDone channel is
	// closed by the supervisor goroutine after Wait returns; WaitTask
	// subscribers select on it (so N concurrent WaitTask calls all see
	// the same single Wait result without re-invoking cmd.Wait()).
	ctx, cancel := context.WithCancel(context.Background())
	h := &taskHandle{
		logger:       p.logger.With("task_id", cfg.ID, "vm_index", driverConfig.VMIndex),
		taskConfig:   cfg,
		driverConfig: &driverConfig,
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

	// Supervisor goroutine: blocks on runner.Wait, records the result on
	// the handle, then closes exitDone so any WaitTask call (now or
	// later) can pick it up. This ownership model (one Wait per handle,
	// many WaitTask subscribers) matches the upstream virt driver and is
	// the only model that works for Nomad's at-least-once WaitTask
	// contract.
	go p.superviseCH(h)

	p.logger.Info("ch: StartTask: spawned",
		"task_id", cfg.ID,
		"ch_pid", pid,
		"api_socket", apiSocket,
		"tap", tapName,
		"config", configPath)

	return handle, nil, nil
}

// superviseCH owns the single runner.Wait() call for the handle and
// records the exit result. Closes h.exitDone so any number of WaitTask
// subscribers can see the result without racing on cmd.Wait() (which
// must be called exactly once per *exec.Cmd).
//
// This runs in its own goroutine started from StartTask. Cancellation
// is via h.ctx (StopTask/DestroyTask cancel it; the supervisor then
// signals the process via h.runner.Signal — see T-2 for the full stop
// ladder).
func (p *Plugin) superviseCH(h *taskHandle) {
	waitErr := h.runner.Wait()

	result := &drivers.ExitResult{
		ExitCode: h.runner.ExitCode(),
	}
	if waitErr != nil {
		result.Err = waitErr
		tail := h.runner.StderrTail(stderrTailLimit)
		if len(tail) > 0 {
			result.Err = &chWaitError{base: waitErr, tail: tail}
		}
	}

	h.stateMu.Lock()
	h.procState = drivers.TaskStateExited
	h.completedAt = time.Now().UTC()
	h.exitResult = result
	h.stateMu.Unlock()

	close(h.exitDone)

	if p != nil && p.logger != nil {
		p.logger.Info("ch: VM exited",
			"task_id", h.taskConfig.ID,
			"ch_pid", h.chPid,
			"exit_code", result.ExitCode)
	}
}

// modeOf returns "cold_boot" or "restore" based on whether RestoreFrom is
// set. Helper kept here because it's specific to StartTask's logging shape.
func modeOf(cfg *TaskConfig) string {
	if cfg.RestoreFrom == "" {
		return "cold_boot"
	}
	return "restore"
}

// validateColdBoot enforces the wrapper's cold-boot env preconditions on
// the decoded TaskConfig. Each error names the field; an operator hitting
// this knows exactly which task_config attribute is missing.
//
// Mirrors the wrapper's `: "${ZSBX_*:?missing ZSBX_*}"` guards and the
// pubkey/sandbox_id format checks.
func validateColdBoot(cfg *TaskConfig) error {
	if cfg.SandboxId == "" {
		return errors.New("ch: StartTask: sandbox_id is required (cold-boot)")
	}
	// typed_id format: [a-zA-Z0-9_] only. Anything else corrupts the
	// kernel cmdline at the whitespace tokeniser.
	if !isTypedID(cfg.SandboxId) {
		return fmt.Errorf("ch: StartTask: sandbox_id %q contains characters outside [0-9a-zA-Z_] (would corrupt kernel cmdline)", cfg.SandboxId)
	}
	if cfg.WorkspaceImg == "" {
		return errors.New("ch: StartTask: workspace_img is required (cold-boot)")
	}
	if cfg.UserHomeImg == "" {
		return errors.New("ch: StartTask: user_home_img is required (cold-boot)")
	}
	if cfg.PubkeyHex == "" {
		return errors.New("ch: StartTask: pubkey_hex is required (cold-boot)")
	}
	if !isLowerHex(cfg.PubkeyHex) {
		return fmt.Errorf("ch: StartTask: pubkey_hex %q contains non-hex characters", cfg.PubkeyHex)
	}
	if len(cfg.PubkeyHex)%2 != 0 {
		return fmt.Errorf("ch: StartTask: pubkey_hex has odd length %d", len(cfg.PubkeyHex))
	}
	if cfg.Kernel == "" {
		return errors.New("ch: StartTask: kernel is required")
	}
	if cfg.CPUs == 0 {
		return errors.New("ch: StartTask: cpus must be > 0")
	}
	if cfg.MemoryMB == 0 {
		return errors.New("ch: StartTask: memory_mb must be > 0")
	}
	if cfg.VMIndex < 1 || cfg.VMIndex > 155 {
		return fmt.Errorf("ch: StartTask: vm_index %d out of range [1,155]", cfg.VMIndex)
	}
	if cfg.SubnetBaseOctet > 255 {
		return fmt.Errorf("ch: StartTask: subnet_base_octet %d out of u8 range", cfg.SubnetBaseOctet)
	}
	return nil
}

// isTypedID asserts the wrapper's typed-id contract: a non-empty sequence
// of [0-9a-zA-Z_]. Kept inline (no regexp) so the hot path is allocation-
// free.
func isTypedID(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c >= '0' && c <= '9':
		case c >= 'a' && c <= 'z':
		case c >= 'A' && c <= 'Z':
		case c == '_':
		default:
			return false
		}
	}
	return true
}

// isLowerHex asserts the wrapper's pubkey contract: hex characters only.
// Tolerates upper-case A-F to match the wrapper's case-insensitive regex.
func isLowerHex(s string) bool {
	if s == "" {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		switch {
		case c >= '0' && c <= '9':
		case c >= 'a' && c <= 'f':
		case c >= 'A' && c <= 'F':
		default:
			return false
		}
	}
	return true
}

// taskRunDir returns the absolute path of the per-task run dir. Preference
// is Nomad's per-task local dir (cfg.TaskDir().LocalDir); fall back to
// cfg.TaskDir().Dir, then config.RunDir/<task-id> for environments where
// Nomad hasn't provided a task dir (unit tests).
func taskRunDir(cfg *drivers.TaskConfig, dcfg *Config) string {
	if cfg != nil {
		td := cfg.TaskDir()
		if td != nil {
			if td.LocalDir != "" {
				return td.LocalDir
			}
			if td.Dir != "" {
				return td.Dir
			}
		}
	}
	if dcfg != nil && dcfg.RunDir != "" {
		if cfg != nil && cfg.ID != "" {
			return filepath.Join(dcfg.RunDir, cfg.ID)
		}
		return dcfg.RunDir
	}
	if cfg != nil && cfg.AllocDir != "" {
		return filepath.Join(cfg.AllocDir, "ch")
	}
	return filepath.Join(os.TempDir(), "ch-run")
}

// resolveNet returns the tap name + a NetSpec derived from VMIndex /
// SubnetBaseOctet, falling back to driverConfig.Net[0] if the operator
// provided one explicitly. Mirrors the wrapper:
//
//	TAP=zsbx-nm-${IDX}
//	VM_IP=10.${BASE}.${100+IDX}.2
//	HOST_IP=10.${BASE}.${100+IDX}.1
//	MAC=12:34:56:78:9b:${IDX hex}
func resolveNet(cfg *TaskConfig) (string, NetSpec) {
	if len(cfg.Net) > 0 {
		return cfg.Net[0].Tap, cfg.Net[0]
	}
	base := cfg.SubnetBaseOctet
	if base == 0 {
		base = 99 // wrapper default
	}
	idx := cfg.VMIndex
	tap := fmt.Sprintf("zsbx-nm-%d", idx)
	return tap, NetSpec{
		Tap:  tap,
		MAC:  fmt.Sprintf("12:34:56:78:9b:%02x", idx),
		IP:   fmt.Sprintf("10.%d.%d.2", base, 100+int(idx)),
		Mask: "255.255.255.252",
	}
}

// synthesizeCmdline builds the kernel cmdline from the wrapper-equivalent
// fields. Verbatim shape from the wrapper's cold-boot branch:
//
//	console=ttyS0 root=/dev/vda rw init=/sbin/init reboot=t panic=1
//	ip=${VM_IP}::${HOST_IP}:255.255.255.252::eth0:none
//	zsbx_pubkey=${PUBKEY_HEX}
//	SANDBOX_AGENT_SANDBOX_ID=${SANDBOX_ID}
func synthesizeCmdline(cfg *TaskConfig, net NetSpec) string {
	hostIP := deriveHostIP(net.IP)
	mask := net.Mask
	if mask == "" {
		mask = "255.255.255.252"
	}
	tokens := []string{
		"console=ttyS0",
		"root=/dev/vda",
		"rw",
		"init=/sbin/init",
		"reboot=t",
		"panic=1",
		fmt.Sprintf("ip=%s::%s:%s::eth0:none", net.IP, hostIP, mask),
		fmt.Sprintf("zsbx_pubkey=%s", cfg.PubkeyHex),
		fmt.Sprintf("SANDBOX_AGENT_SANDBOX_ID=%s", cfg.SandboxId),
	}
	return strings.Join(tokens, " ")
}

// deriveHostIP turns 10.B.X.2 into 10.B.X.1 (the host side of the /30).
// Defensive: if vm_ip doesn't end in .2, return it unchanged — the
// operator-supplied cmdline path won't take this branch anyway.
func deriveHostIP(vmIP string) string {
	if !strings.HasSuffix(vmIP, ".2") {
		return vmIP
	}
	return vmIP[:len(vmIP)-1] + "1"
}

// ensureTapUpFn is the package-level seam tests swap out. Default impl
// (ensureTapUpDefault) shells to `ip link set <tap> up`; tests override
// to a no-op so they don't need CAP_NET_ADMIN. T-3 will replace the
// default with rtnetlink + optional auto-creation.
var ensureTapUpFn = ensureTapUpDefault

// ensureTapUpDefault is the production implementation. Failure to find
// the device is FATAL (matches the wrapper's "host setup script did not
// pre-create it" branch). Failure to set it up is also FATAL, with the
// cause surfaced so the operator can tell a permission error
// (need root + CAP_NET_ADMIN) from a missing-device error.
func ensureTapUpDefault(tapName string) error {
	if tapName == "" {
		return errors.New("empty tap name")
	}
	if _, err := os.Stat("/sys/class/net/" + tapName); err != nil {
		return fmt.Errorf("tap %s missing: %w", tapName, err)
	}
	cmd := exec.Command("ip", "link", "set", "dev", tapName, "up")
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("ip link set %s up: %w (output=%q)", tapName, err, string(out))
	}
	return nil
}

// ensureTapUp dispatches through the swappable seam.
func ensureTapUp(tapName string) error {
	return ensureTapUpFn(tapName)
}

// ------------------------------------------------------------------
// CH config.json shape
//
// Subset of the cloud-hypervisor REST API's `vm.create` payload (which is
// what --config consumes). We pin only the fields we set; CH tolerates
// extra fields gracefully but rejects unknown ones with HTTP 400, so the
// JSON marshaller's `omitempty` on optional slices is important.
// ------------------------------------------------------------------

// chConfigDoc is the top-level shape of the JSON CH `--config` loads.
type chConfigDoc struct {
	CPUs    chCPUs        `json:"cpus"`
	Memory  chMemory      `json:"memory"`
	Payload chPayload     `json:"payload"`
	Disks   []chDisk      `json:"disks,omitempty"`
	Net     []chNet       `json:"net,omitempty"`
	Serial  *chConsoleSer `json:"serial,omitempty"`
	Console *chConsoleSer `json:"console,omitempty"`
}

type chCPUs struct {
	BootVcpus int `json:"boot_vcpus"`
	MaxVcpus  int `json:"max_vcpus"`
}

type chMemory struct {
	// Size in bytes. CH consumes bytes here despite the wrapper's --memory
	// arg accepting suffixes (M/G); the JSON shape is bytes-only.
	Size   uint64 `json:"size"`
	Shared bool   `json:"shared"`
}

type chPayload struct {
	Kernel  string `json:"kernel"`
	Cmdline string `json:"cmdline,omitempty"`
}

type chDisk struct {
	Path      string `json:"path"`
	Readonly  bool   `json:"readonly,omitempty"`
	Direct    bool   `json:"direct,omitempty"`
	Serial    string `json:"serial,omitempty"`
	ImageType string `json:"image_type,omitempty"`
}

type chNet struct {
	Tap string `json:"tap"`
	Mac string `json:"mac,omitempty"`
}

type chConsoleSer struct {
	Mode string `json:"mode"`           // "Off"|"File"|"Tty"|"Null"|"Socket"
	File string `json:"file,omitempty"` // when Mode=="File"
}

// buildCHConfig assembles a chConfigDoc from the decoded TaskConfig + the
// synthesised cmdline / disks / net entries. Pure function — easy to unit
// test (which is what TestStartTask_BuildsConfigJSON exercises).
func buildCHConfig(cfg *TaskConfig, cmdline string, disks []DiskSpec, nets []NetSpec, serialLog string) chConfigDoc {
	doc := chConfigDoc{
		CPUs: chCPUs{
			BootVcpus: int(cfg.CPUs),
			MaxVcpus:  int(cfg.CPUs),
		},
		Memory: chMemory{
			Size:   uint64(cfg.MemoryMB) * 1024 * 1024,
			Shared: true,
		},
		Payload: chPayload{
			Kernel:  cfg.Kernel,
			Cmdline: cmdline,
		},
	}
	for _, d := range disks {
		doc.Disks = append(doc.Disks, chDisk{
			Path:      d.Path,
			Readonly:  d.Readonly,
			Direct:    false,
			Serial:    d.Serial,
			ImageType: "raw",
		})
	}
	for _, n := range nets {
		doc.Net = append(doc.Net, chNet{
			Tap: n.Tap,
			Mac: n.MAC,
		})
	}
	if serialLog != "" {
		doc.Serial = &chConsoleSer{Mode: "File", File: serialLog}
	}
	doc.Console = &chConsoleSer{Mode: "Off"}
	return doc
}

// buildConfigJSON is the test entry point — round-trips a TaskConfig
// through validation, network resolution, cmdline synthesis, and
// chConfigDoc marshalling so tests can pin the JSON shape without
// reaching for a real Nomad TaskConfig.
//
// taskDir is unused today (the JSON doesn't bake in alloc paths), but is
// accepted so the signature matches what T-6's restore-aware variant will
// need (rewriting the snapshot's disks[].path into the current alloc's
// dir).
func buildConfigJSON(cfg TaskConfig, taskDir string) ([]byte, error) {
	if err := validateColdBoot(&cfg); err != nil {
		return nil, err
	}
	_, netSpec := resolveNet(&cfg)
	cmdline := cfg.Cmdline
	if cmdline == "" {
		cmdline = synthesizeCmdline(&cfg, netSpec)
	}
	disks := cfg.Disks
	if len(disks) == 0 {
		rootfs := filepath.Join(taskDir, chRootfsName)
		disks = []DiskSpec{
			{Path: rootfs, Readonly: false, Serial: "zsbx-root"},
			{Path: cfg.WorkspaceImg, Readonly: false, Serial: "zsbx-work"},
			{Path: cfg.UserHomeImg, Readonly: false, Serial: "zsbx-home"},
		}
	}
	serialLog := ""
	if taskDir != "" {
		serialLog = filepath.Join(taskDir, chSerialLogName)
	}
	doc := buildCHConfig(&cfg, cmdline, disks, []NetSpec{netSpec}, serialLog)
	return json.MarshalIndent(doc, "", "  ")
}
