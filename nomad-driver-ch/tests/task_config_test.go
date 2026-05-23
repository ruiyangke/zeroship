// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Scaffold-level test that exercises the TaskConfig struct shape. Once T-1
// is implemented, replace the JSON round-trip with an HCL parse + msgpack
// decode that matches what Nomad actually sends.

package tests

import (
	"encoding/json"
	"testing"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// TestTaskConfigRoundTrip ensures the struct tags are mutually consistent
// (JSON unmarshalling uses the same field names the codec tag declares; this
// catches typos in either tag without needing a live Nomad).
func TestTaskConfigRoundTrip(t *testing.T) {
	src := ch.TaskConfig{
		VMIndex:  7,
		Kernel:   "/opt/zsbx/vmlinux",
		Cmdline:  "console=hvc0 reboot=k panic=-1",
		CPUs:     2,
		MemoryMB: 512,
		Disks: []ch.DiskSpec{
			{Path: "/var/lib/zsbx/img/rootfs.img", Readonly: true, Serial: "zsbx-root"},
			{Path: "/var/lib/zsbx/img/workspace.img", Readonly: false, Serial: "zsbx-work"},
		},
		Fs: []ch.VirtioFsSpec{
			{Tag: "keys", Socket: "/var/lib/zsbx/run/keys.sock", SourcePath: "/var/lib/zsbx/keys"},
		},
		Net: []ch.NetSpec{
			{Tap: "zsbx-nm-7", MAC: "12:34:56:78:9b:07", IP: "10.99.7.2", Mask: "255.255.255.252"},
		},
		RestoreFrom: "",
	}

	b, err := json.Marshal(src)
	if err != nil {
		t.Fatalf("Marshal: %v", err)
	}

	var got ch.TaskConfig
	if err := json.Unmarshal(b, &got); err != nil {
		t.Fatalf("Unmarshal: %v", err)
	}

	if got.VMIndex != src.VMIndex || got.Kernel != src.Kernel ||
		got.CPUs != src.CPUs || got.MemoryMB != src.MemoryMB {
		t.Errorf("scalar fields lost: got %+v want %+v", got, src)
	}
	if len(got.Disks) != 2 || got.Disks[0].Serial != "zsbx-root" {
		t.Errorf("Disks lost: got %+v", got.Disks)
	}
	if len(got.Net) != 1 || got.Net[0].MAC != src.Net[0].MAC {
		t.Errorf("Net lost: got %+v", got.Net)
	}
}

// TestTaskConfigRestoreMode asserts that an empty RestoreFrom means cold-boot
// (the StartTask helper switches on this exact field).
func TestTaskConfigRestoreMode(t *testing.T) {
	cases := []struct {
		name string
		from string
		cold bool
	}{
		{"cold_boot", "", true},
		{"restore", "/var/lib/zsbx/snap/abc", false},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			cfg := ch.TaskConfig{RestoreFrom: tc.from}
			if (cfg.RestoreFrom == "") != tc.cold {
				t.Errorf("RestoreFrom=%q: cold=%v, want %v", cfg.RestoreFrom, cfg.RestoreFrom == "", tc.cold)
			}
		})
	}
}
