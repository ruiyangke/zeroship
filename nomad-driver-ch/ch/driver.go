// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver.go) on 2026-05-25 for
// Cloud Hypervisor support. Libvirt-specific imports, providers, storage
// pools, and cloud-init coupling have been removed; CH-specific stubs are in
// place ready for sprint-by-sprint implementation.

// Package ch implements the Cloud Hypervisor Nomad task driver. It exposes a
// drivers.DriverPlugin that the Nomad client can talk to over gRPC (via the
// go-plugin handshake). The driver replaces the bash wrapper at
// crates/sandbox/scripts/nomad-vm-wrapper.sh — see docs/proposals/nomad-driver-ch.md
// for the full motivation and lifecycle mapping.
package ch

import (
	"context"
	"errors"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/drivers/shared/eventer"
	"github.com/hashicorp/nomad/plugins/base"
	"github.com/hashicorp/nomad/plugins/drivers"
	"github.com/hashicorp/nomad/plugins/drivers/fsisolation"
	"github.com/hashicorp/nomad/plugins/shared/hclspec"
	"github.com/hashicorp/nomad/plugins/shared/structs"
)

const (
	// PluginName is the public driver name; jobspecs reference this in
	// task.driver = "ch".
	PluginName = "ch"

	// PluginVersion identifies the driver build to Nomad. Bumped per-release;
	// surfaces in `nomad node status` and the plugin-load log line.
	PluginVersion = "0.0.1-scaffold"

	// TaskHandleVersion is the schema version of the TaskState struct we
	// persist into Nomad's drivers.TaskHandle. Bump if the struct changes
	// shape in a non-back-compat way; RecoverTask must then handle both.
	TaskHandleVersion = 1

	// FingerprintPeriod is how often we re-emit a Fingerprint to Nomad.
	// 30s matches the upstream virt driver and the Nomad-default cadence.
	FingerprintPeriod = 30 * time.Second
)

// PluginInfo is returned from the BasePlugin.PluginInfo() RPC, identifying
// this binary to the Nomad client during the plugin-load handshake.
var PluginInfo = &base.PluginInfoResponse{
	Type:              base.PluginTypeDriver,
	PluginApiVersions: []string{drivers.ApiVersion010},
	PluginVersion:     PluginVersion,
	Name:              PluginName,
}

// Capabilities is returned from the driver.Capabilities() RPC. Nomad uses it
// to decide what features it can ask of us. The flags below match the
// proposal (§ 3.4) and the volantvm spike (§ 3) — namely:
//
//   - SendSignals=true: we forward signals to the CH process (for dev only;
//     production code paths never signal a VM out of band).
//   - Exec=false: user code runs inside the VM via the sandbox-agent RPC
//     channel, not as a Nomad exec.
//   - DisableLogCollection=true: CH logs go to its own --log-file under our
//     run dir; we don't want Nomad slurping them into its log buffer.
//   - FSIsolation=Image: we own the rootfs (CH boots from a virtio-blk image).
//   - NetIsolationModes=[None]: we own the tap; Nomad must NOT try to put us
//     in a netns or a CNI sandbox.
//   - MustInitiateNetwork=false: Nomad doesn't expect a DriverNetwork from us.
//   - MountConfigs=None: tasks cannot declare host mounts (virtio-fs is set
//     by driver config, not by the operator-submitted jobspec).
var Capabilities = &drivers.Capabilities{
	SendSignals:          true,
	Exec:                 false,
	DisableLogCollection: true,
	FSIsolation:          fsisolation.Image,
	NetIsolationModes: []drivers.NetIsolationMode{
		drivers.NetIsolationModeNone,
	},
	MustInitiateNetwork: false,
	MountConfigs:        drivers.MountConfigSupportNone,
}

// Errors returned across multiple lifecycle methods.
var (
	ErrExistingTask  = errors.New("ch: task is already running")
	ErrTaskNotFound  = errors.New("ch: task not found")
	ErrNotImplemented = errors.New("ch: not implemented")
)

// Plugin implements drivers.DriverPlugin. It holds the in-memory task map,
// the Nomad-issued logger, and the configured CH client. All stubs in this
// file return errors tagged with "T-N" so we can drive the implementation
// sprint-by-sprint (see DESIGN.md).
type Plugin struct {
	// eventer fans TaskEvents() out to Nomad's plugin client.
	eventer *eventer.Eventer

	// config is the driver-level config from SetConfig (Nomad agent-side
	// `plugin "ch" { config { ... } }` block). Populated during plugin load.
	config *Config

	// nomadConfig is the agent's view of itself (data dir, client ID, etc.).
	// Stored so per-task code paths can read e.g. the worker's data dir.
	nomadConfig *base.ClientDriverConfig

	// tasks is the in-memory task registry. Survives only as long as this
	// process; RecoverTask reconstructs it from on-disk TaskState across
	// Nomad-client restarts (T-4).
	tasks *taskStore

	// chClient wraps `ch-remote` and our Unix-socket HTTP client. Shared
	// across all tasks; per-task state lives in taskHandle.
	chClient *Client

	// signalShutdown cancels every per-task context when the plugin process
	// is being torn down (SIGTERM from Nomad).
	signalShutdown context.CancelFunc

	logger hclog.Logger
}

// Config is the driver-level configuration block (set once at plugin load).
// Field names match the proposal's example Nomad agent stanza in § 8.
type Config struct {
	CloudHypervisorBin string `codec:"cloud_hypervisor_bin"`
	VirtiofsdBin       string `codec:"virtiofsd_bin"`
	VMIndexLockDir     string `codec:"vm_index_lockdir"`
	RunDir             string `codec:"run_dir"`
}

// configSpec is the HCL schema for the driver-level config block. Returned
// from ConfigSchema(); Nomad validates the operator's agent config against it.
var configSpec = hclspec.NewObject(map[string]*hclspec.Spec{
	"cloud_hypervisor_bin": hclspec.NewDefault(
		hclspec.NewAttr("cloud_hypervisor_bin", "string", false),
		hclspec.NewLiteral(`"/usr/local/bin/cloud-hypervisor"`),
	),
	"virtiofsd_bin": hclspec.NewDefault(
		hclspec.NewAttr("virtiofsd_bin", "string", false),
		hclspec.NewLiteral(`"/usr/local/bin/virtiofsd"`),
	),
	"vm_index_lockdir": hclspec.NewDefault(
		hclspec.NewAttr("vm_index_lockdir", "string", false),
		hclspec.NewLiteral(`"/var/lib/zsbx/vm-index"`),
	),
	"run_dir": hclspec.NewDefault(
		hclspec.NewAttr("run_dir", "string", false),
		hclspec.NewLiteral(`"/var/lib/zsbx/run"`),
	),
})

// NewPlugin returns a drivers.DriverPlugin ready to be served via
// plugins.Serve. Called once per plugin process by the factory in
// cmd/nomad-driver-ch/main.go.
func NewPlugin(logger hclog.Logger) drivers.DriverPlugin {
	ctx, cancel := context.WithCancel(context.Background())
	logger = logger.Named(PluginName)
	return &Plugin{
		eventer:        eventer.NewEventer(ctx, logger),
		config:         &Config{},
		tasks:          newTaskStore(),
		chClient:       NewClient(logger),
		signalShutdown: cancel,
		logger:         logger,
	}
}

// PluginInfo returns the version/identity tuple Nomad logs at load time.
func (p *Plugin) PluginInfo() (*base.PluginInfoResponse, error) {
	return PluginInfo, nil
}

// ConfigSchema is the HCL schema Nomad validates the agent-side
// `plugin "ch" { config { ... } }` block against.
func (p *Plugin) ConfigSchema() (*hclspec.Spec, error) {
	return configSpec, nil
}

// SetConfig is invoked once at plugin load with the operator's driver-level
// config (msgpack-encoded against ConfigSchema). Stash it on the receiver
// and propagate the configured binary paths into the shared CH client so
// StartTask doesn't re-resolve them on every spawn.
func (p *Plugin) SetConfig(cfg *base.Config) error {
	var config Config
	if len(cfg.PluginConfig) != 0 {
		if err := base.MsgPackDecode(cfg.PluginConfig, &config); err != nil {
			return err
		}
	}
	p.config = &config
	if cfg.AgentConfig != nil {
		p.nomadConfig = cfg.AgentConfig.Driver
	}
	// Best-effort binary discovery using the new config. NewClient also
	// did this; we redo it here in case Config arrives after construction.
	if p.chClient != nil {
		p.chClient.SetBinaries(p.config.CloudHypervisorBin, p.config.VirtiofsdBin)
	}
	return nil
}

// TaskConfigSchema returns the HCL schema for the per-task `config { ... }`
// block. Defined in task_config.go to keep the schema and the decoded struct
// next to each other.
func (p *Plugin) TaskConfigSchema() (*hclspec.Spec, error) {
	return taskConfigSpec, nil
}

// Capabilities returns the static capability vector; cached by Nomad.
func (p *Plugin) Capabilities() (*drivers.Capabilities, error) {
	return Capabilities, nil
}

// Fingerprint streams a health/attributes snapshot every FingerprintPeriod.
//
// T-0 (scaffold): emit Undetected once, then close. Once StartTask et al are
// wired up, this should report CH/virtiofsd/kvm presence and the vm_index
// pool size (see proposal § 7 "Fingerprint").
func (p *Plugin) Fingerprint(ctx context.Context) (<-chan *drivers.Fingerprint, error) {
	ch := make(chan *drivers.Fingerprint)
	go p.handleFingerprint(ctx, ch)
	return ch, nil
}

func (p *Plugin) handleFingerprint(ctx context.Context, ch chan<- *drivers.Fingerprint) {
	defer close(ch)

	ch <- p.buildFingerprint()

	ticker := time.NewTicker(FingerprintPeriod)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			ch <- p.buildFingerprint()
		}
	}
}

func (p *Plugin) buildFingerprint() *drivers.Fingerprint {
	// T-1: report the discovered CH binary so `nomad node status` shows
	// whether the host is ready. /dev/kvm + virtiofsd are still TODO
	// (T-3 covers the deeper probe — tap availability, kvm capability).
	attrs := map[string]*structs.Attribute{}
	chBin := ""
	if p.chClient != nil {
		chBin = p.chClient.CHBin()
	}
	if chBin == "" {
		return &drivers.Fingerprint{
			Attributes:        attrs,
			Health:            drivers.HealthStateUndetected,
			HealthDescription: "ch driver: cloud-hypervisor binary not found on PATH",
		}
	}
	attrs["driver.ch.cloud_hypervisor_bin"] = structs.NewStringAttribute(chBin)
	if chRemote := p.chClient.CHRemoteBin(); chRemote != "" {
		attrs["driver.ch.ch_remote_bin"] = structs.NewStringAttribute(chRemote)
	}
	return &drivers.Fingerprint{
		Attributes:        attrs,
		Health:            drivers.HealthStateHealthy,
		HealthDescription: "ready",
	}
}

// TaskEvents proxies the eventer's channel; structured events are emitted
// from per-task code paths via p.eventer.EmitEvent.
func (p *Plugin) TaskEvents(ctx context.Context) (<-chan *drivers.TaskEvent, error) {
	return p.eventer.TaskEvents(ctx)
}

// ExecTask is intentionally unsupported (capability claims Exec=false).
// User code runs inside the VM via the existing sandbox-agent channel.
func (p *Plugin) ExecTask(taskID string, cmd []string, timeout time.Duration) (*drivers.ExecTaskResult, error) {
	return nil, errors.New("ch: ExecTask is not supported (capability Exec=false)")
}

// SignalTask forwards a POSIX signal (named per Nomad's `nomad alloc signal`
// contract — "SIGUSR1", "SIGHUP", …) to the CH process backing the task.
//
// Resolution:
//
//  1. Parse the signal name via signalLookup (case-insensitive; tolerates
//     both "SIGUSR1" and "USR1"). Unknown names default to syscall.SIGINT
//     with a warning, matching raw_exec's behaviour.
//  2. Prefer handle.runner.Signal (live in T-1's processRunner seam, used
//     by fake runners in tests). Fall back to os.FindProcess+Signal when
//     no runner is attached (RecoverTask handles in T-4).
//
// Capability claim SendSignals=true (see Capabilities) is the wire promise
// behind this method.
func (p *Plugin) SignalTask(taskID string, signal string) error {
	h, ok := p.tasks.Get(taskID)
	if !ok {
		return drivers.ErrTaskNotFound
	}
	sig, ok := signalLookup(signal)
	if !ok {
		p.logger.Warn("ch: SignalTask: unknown signal name; defaulting to SIGINT",
			"task_id", taskID, "signal", signal)
		sig = syscall.SIGINT
	}
	return p.signalHandle(h, sig)
}

// signalLookup resolves a Nomad-style signal name ("SIGUSR1" or "USR1") to
// the matching syscall.Signal. Returns (sig, true) on hit; (0, false) on
// miss so the caller can decide the fallback policy.
//
// The table covers the subset Nomad clients actually emit (`nomad alloc
// signal`'s -s flag accepts these by name); host-only signals (SIGCHLD,
// SIGURG) are intentionally omitted.
func signalLookup(name string) (syscall.Signal, bool) {
	n := strings.ToUpper(strings.TrimSpace(name))
	n = strings.TrimPrefix(n, "SIG")
	switch n {
	case "HUP":
		return syscall.SIGHUP, true
	case "INT":
		return syscall.SIGINT, true
	case "QUIT":
		return syscall.SIGQUIT, true
	case "ILL":
		return syscall.SIGILL, true
	case "TRAP":
		return syscall.SIGTRAP, true
	case "ABRT", "IOT":
		return syscall.SIGABRT, true
	case "BUS":
		return syscall.SIGBUS, true
	case "FPE":
		return syscall.SIGFPE, true
	case "KILL":
		return syscall.SIGKILL, true
	case "USR1":
		return syscall.SIGUSR1, true
	case "SEGV":
		return syscall.SIGSEGV, true
	case "USR2":
		return syscall.SIGUSR2, true
	case "PIPE":
		return syscall.SIGPIPE, true
	case "ALRM":
		return syscall.SIGALRM, true
	case "TERM":
		return syscall.SIGTERM, true
	case "STOP":
		return syscall.SIGSTOP, true
	case "TSTP":
		return syscall.SIGTSTP, true
	case "CONT":
		return syscall.SIGCONT, true
	case "WINCH":
		return syscall.SIGWINCH, true
	case "IO":
		return syscall.SIGIO, true
	case "PWR":
		return syscall.SIGPWR, true
	case "SYS":
		return syscall.SIGSYS, true
	}
	return 0, false
}

// taskStore is the in-process registry of running tasks. Methods are
// goroutine-safe; the store does NOT persist to disk — TaskState in the
// Nomad-issued drivers.TaskHandle is the source of truth across restarts.
type taskStore struct {
	mu    sync.RWMutex
	store map[string]*taskHandle
}

func newTaskStore() *taskStore {
	return &taskStore{store: map[string]*taskHandle{}}
}

func (ts *taskStore) Set(id string, h *taskHandle) {
	ts.mu.Lock()
	defer ts.mu.Unlock()
	ts.store[id] = h
}

func (ts *taskStore) Get(id string) (*taskHandle, bool) {
	ts.mu.RLock()
	defer ts.mu.RUnlock()
	h, ok := ts.store[id]
	return h, ok
}

func (ts *taskStore) Delete(id string) {
	ts.mu.Lock()
	defer ts.mu.Unlock()
	delete(ts.store, id)
}
