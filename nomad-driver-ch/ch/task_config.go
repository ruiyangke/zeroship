// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Forked from hashicorp/nomad-driver-virt (virt/config.go) on 2026-05-25 for
// Cloud Hypervisor support. The libvirt-specific TaskConfig fields (ImagePath,
// UseThinCopy, OS{Arch,Machine}, UserData, DefaultUserSSHKey) have been
// removed; CH-shaped fields (kernel/cmdline/disks/net/restore_from) are in
// their place per the proposal § 4.

package ch

import (
	"github.com/hashicorp/nomad/plugins/shared/hclspec"
)

// TaskConfig is the decoded per-task config block. The shape matches the
// proposal § 4 (the Rust struct sketched there); field names use the codec
// tag because Nomad msgpack-encodes them on the wire.
//
// Wire-format invariant: this struct is part of the controller↔driver
// contract. Adding fields is fine (default-zero is back-compat); renaming or
// removing fields is a breaking change that requires bumping TaskHandleVersion
// and handling the old shape in RecoverTask.
type TaskConfig struct {
	// VMIndex is the per-host VM index (0..MaxVMs-1). Used to derive the tap
	// name, the /30 subnet, the deterministic MAC, the API socket path, and
	// the virtiofsd socket paths.
	VMIndex uint16 `codec:"vm_index"`

	// Kernel is the absolute path to a CH-compatible kernel image (typically
	// a vmlinux blob baked into the worker image).
	Kernel string `codec:"kernel"`

	// Cmdline is the kernel command line passed verbatim as
	// `cloud-hypervisor --cmdline ...`. Includes the agent-identity tokens
	// (zsbx_pubkey, SANDBOX_AGENT_SANDBOX_ID) and the static IP token.
	Cmdline string `codec:"cmdline"`

	// CPUs is the boot vCPU count for the guest.
	CPUs uint8 `codec:"cpus"`

	// MemoryMB is the guest RAM size in mebibytes (CH expects bytes; we
	// convert at vm.create JSON build time).
	MemoryMB uint32 `codec:"memory_mb"`

	// Disks is the list of virtio-blk disks attached to the guest, in order.
	// The first entry is conventionally the rootfs; subsequent entries are
	// workspace/userhome/etc. Each disk is a single --disk argv entry.
	Disks []DiskSpec `codec:"disks"`

	// Fs is the list of virtio-fs mounts; each one corresponds to a
	// separately-spawned virtiofsd daemon and a `--fs tag=...,socket=...`
	// argv entry.
	Fs []VirtioFsSpec `codec:"fs"`

	// Net is the list of virtio-net interfaces. Today we always have exactly
	// one (the per-VM /30 tap); the slice exists for forward-compat.
	Net []NetSpec `codec:"net"`

	// RestoreFrom, when non-empty, switches the driver from cold-boot to
	// restore: CH is spawned with `--restore source_url=file://<RestoreFrom>`
	// and then `ch-remote resume` is issued. When empty, cold-boot.
	RestoreFrom string `codec:"restore_from"`
}

// DiskSpec is one virtio-blk disk.
type DiskSpec struct {
	Path     string `codec:"path"`     // host-side image path (absolute)
	Readonly bool   `codec:"readonly"` // true for the rootfs base image
	Serial   string `codec:"serial"`   // optional serial; surfaced to guest udev
}

// VirtioFsSpec is one virtio-fs mount (corresponds to one virtiofsd daemon).
type VirtioFsSpec struct {
	Tag        string `codec:"tag"`         // mount tag visible in the guest
	Socket     string `codec:"socket"`      // path the host-side virtiofsd binds
	SourcePath string `codec:"source_path"` // host directory to export
}

// NetSpec is one virtio-net interface.
type NetSpec struct {
	Tap  string `codec:"tap"`  // host-side tap device name
	MAC  string `codec:"mac"`  // deterministic MAC (12:34:56:78:9b:<vm_index>)
	IP   string `codec:"ip"`   // guest IP (host-side info only; the cmdline carries the live one)
	Mask string `codec:"mask"` // guest netmask (255.255.255.252 for our /30s)
}

// taskConfigSpec is the HCL schema for the per-task config block. Returned
// from TaskConfigSchema(); Nomad validates the operator's task config against
// it before calling StartTask.
var taskConfigSpec = hclspec.NewObject(map[string]*hclspec.Spec{
	"vm_index":     hclspec.NewAttr("vm_index", "number", true),
	"kernel":       hclspec.NewAttr("kernel", "string", true),
	"cmdline":      hclspec.NewAttr("cmdline", "string", true),
	"cpus":         hclspec.NewAttr("cpus", "number", true),
	"memory_mb":    hclspec.NewAttr("memory_mb", "number", true),
	"restore_from": hclspec.NewAttr("restore_from", "string", false),

	"disks": hclspec.NewBlockList("disks", hclspec.NewObject(map[string]*hclspec.Spec{
		"path":     hclspec.NewAttr("path", "string", true),
		"readonly": hclspec.NewAttr("readonly", "bool", false),
		"serial":   hclspec.NewAttr("serial", "string", false),
	})),

	"fs": hclspec.NewBlockList("fs", hclspec.NewObject(map[string]*hclspec.Spec{
		"tag":         hclspec.NewAttr("tag", "string", true),
		"socket":      hclspec.NewAttr("socket", "string", true),
		"source_path": hclspec.NewAttr("source_path", "string", true),
	})),

	"net": hclspec.NewBlockList("net", hclspec.NewObject(map[string]*hclspec.Spec{
		"tap":  hclspec.NewAttr("tap", "string", true),
		"mac":  hclspec.NewAttr("mac", "string", true),
		"ip":   hclspec.NewAttr("ip", "string", true),
		"mask": hclspec.NewAttr("mask", "string", true),
	})),
})
