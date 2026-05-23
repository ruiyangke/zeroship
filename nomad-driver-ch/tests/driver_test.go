// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (plugin/driver_test.go shape) on
// 2026-05-25 for Cloud Hypervisor support. The libvirt-mock-backed tests are
// gone; what remains are interface-level checks that the scaffold compiles
// and that PluginInfo/Capabilities have the values we promise.

package tests

import (
	"testing"

	"github.com/hashicorp/go-hclog"
	"github.com/hashicorp/nomad/plugins/base"
	"github.com/hashicorp/nomad/plugins/drivers"
	"github.com/hashicorp/nomad/plugins/drivers/fsisolation"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// TestPluginInfo asserts the version/identity tuple we hand to Nomad.
func TestPluginInfo(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	info, err := p.PluginInfo()
	if err != nil {
		t.Fatalf("PluginInfo: %v", err)
	}
	if info.Name != ch.PluginName {
		t.Errorf("Name = %q, want %q", info.Name, ch.PluginName)
	}
	if info.Type != base.PluginTypeDriver {
		t.Errorf("Type = %q, want %q", info.Type, base.PluginTypeDriver)
	}
	if len(info.PluginApiVersions) == 0 || info.PluginApiVersions[0] != drivers.ApiVersion010 {
		t.Errorf("PluginApiVersions = %v, want [%q]", info.PluginApiVersions, drivers.ApiVersion010)
	}
}

// TestCapabilities asserts the capability flags from the proposal § 3.4.
// These are wire-format invariants: changing them affects Nomad scheduling
// for every existing job.
func TestCapabilities(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	caps, err := p.Capabilities()
	if err != nil {
		t.Fatalf("Capabilities: %v", err)
	}
	if !caps.SendSignals {
		t.Error("SendSignals should be true (dev `nomad alloc signal`)")
	}
	if caps.Exec {
		t.Error("Exec should be false (user code runs inside the VM)")
	}
	if !caps.DisableLogCollection {
		t.Error("DisableLogCollection should be true (CH writes its own log file)")
	}
	if caps.FSIsolation != fsisolation.Image {
		t.Errorf("FSIsolation = %v, want %v", caps.FSIsolation, fsisolation.Image)
	}
	if len(caps.NetIsolationModes) != 1 || caps.NetIsolationModes[0] != drivers.NetIsolationModeNone {
		t.Errorf("NetIsolationModes = %v, want [%v]", caps.NetIsolationModes, drivers.NetIsolationModeNone)
	}
	if caps.MustInitiateNetwork {
		t.Error("MustInitiateNetwork should be false (we own the tap)")
	}
	if caps.MountConfigs != drivers.MountConfigSupportNone {
		t.Errorf("MountConfigs = %v, want %v", caps.MountConfigs, drivers.MountConfigSupportNone)
	}
}

// TestConfigSchemaPresent guards against accidental removal of the schema.
// Once a non-test ever loads it, breaking it surfaces here.
func TestConfigSchemaPresent(t *testing.T) {
	p := ch.NewPlugin(hclog.NewNullLogger())
	if _, err := p.ConfigSchema(); err != nil {
		t.Fatalf("ConfigSchema: %v", err)
	}
	if _, err := p.TaskConfigSchema(); err != nil {
		t.Fatalf("TaskConfigSchema: %v", err)
	}
}

// TestInterfaceConformance is a compile-time check that *ch.Plugin satisfies
// drivers.DriverPlugin. If any required RPC is removed or renamed, this
// stops compiling — which is exactly the lever we want.
func TestInterfaceConformance(t *testing.T) {
	var _ drivers.DriverPlugin = ch.NewPlugin(hclog.NewNullLogger())
}
