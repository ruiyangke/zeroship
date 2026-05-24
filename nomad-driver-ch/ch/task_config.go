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
	// a vmlinux blob baked into the worker image). Mirrors the wrapper's
	// "$ZSBX_ARTIFACT_DIR/vmlinuz" path.
	Kernel string `codec:"kernel"`

	// Cmdline is the kernel command line passed verbatim as
	// `cloud-hypervisor --cmdline ...`. When empty, StartTask synthesises
	// one from the agent-identity tokens below (PubkeyHex / SandboxId /
	// VMIndex / SubnetBaseOctet). When non-empty, it overrides synthesis —
	// kept for forward-compat with controllers that want to drive the
	// cmdline themselves.
	Cmdline string `codec:"cmdline"`

	// CPUs is the boot vCPU count for the guest.
	CPUs uint8 `codec:"cpus"`

	// MemoryMB is the guest RAM size in mebibytes (CH expects bytes; we
	// convert at vm.create JSON build time).
	MemoryMB uint32 `codec:"memory_mb"`

	// Disks is the list of virtio-blk disks attached to the guest, in order.
	// The first entry is conventionally the rootfs; subsequent entries are
	// workspace/userhome/etc. Each disk is a single --disk argv entry.
	//
	// When Disks is empty AND the wrapper-equivalent fields below
	// (WorkspaceImg / UserHomeImg) are set, StartTask synthesises a 3-disk
	// list (rootfs / workspace / userhome) mirroring the bash wrapper.
	Disks []DiskSpec `codec:"disks"`

	// Fs is the list of virtio-fs mounts; each one corresponds to a
	// separately-spawned virtiofsd daemon and a `--fs tag=...,socket=...`
	// argv entry.
	Fs []VirtioFsSpec `codec:"fs"`

	// Net is the list of virtio-net interfaces. Today we always have exactly
	// one (the per-VM /30 tap); the slice exists for forward-compat. When
	// empty, StartTask synthesises a single entry from VMIndex /
	// SubnetBaseOctet (matches the wrapper's `zsbx-nm-$IDX` convention).
	Net []NetSpec `codec:"net"`

	// RestoreFrom, when non-empty, switches the driver from cold-boot to
	// restore: CH is spawned with `--restore source_url=file://<RestoreFrom>`
	// and then `ch-remote resume` is issued. When empty, cold-boot.
	RestoreFrom string `codec:"restore_from"`

	// --- Wrapper-equivalent env-mapped fields ---------------------------
	//
	// These mirror the bash wrapper's ZSBX_* env vars (see
	// crates/sandbox/scripts/nomad-vm-wrapper.sh § "Inputs"). Each field
	// maps 1:1 to one wrapper env var; StartTask reads them to synthesise
	// the cmdline and disks when those higher-level fields aren't already
	// populated.

	// SandboxId is the typed-id (`sbx_…`) of the sandbox row. Embedded in
	// the kernel cmdline as `SANDBOX_AGENT_SANDBOX_ID=<id>`. Wrapper env:
	// ZSBX_SANDBOX_ID. Validated to be [0-9a-zA-Z_] only at StartTask time
	// (would otherwise corrupt the kernel cmdline at the whitespace tokeniser).
	SandboxId string `codec:"sandbox_id"`

	// UserId is the typed-id (`usr_…`) of the sandbox owner. Used by the
	// restore-branch path-rewriter (C-7-LT-7) to accept disks that live
	// under the per-user persistent home prefix
	// `/var/zeroship/ch/users/<user_id>/` — the per-user home image
	// (`home.img`) is shared across every sandbox a user owns, so it
	// lives OUTSIDE the per-sandbox prefix by design and must be
	// whitelisted separately. Cross-tenant isolation is preserved via
	// strict equality on the path segment: a disk path that names a
	// DIFFERENT user is rejected.
	//
	// Empty disables the per-user-home allow-list entry (the validator
	// falls back to per-sandbox + task_dir + content-addressed roots
	// only). The driver's StartTask does NOT require it for cold-boot;
	// it only matters on the restore branch when the snapshot's
	// config.json names a `/var/zeroship/ch/users/...` disk.
	UserId string `codec:"user_id"`

	// WorkspaceImg is the absolute host path to the workspace ext4 image
	// (raw, attached as virtio-blk → guest /dev/vdb → /workspace). Wrapper
	// env: ZSBX_WORKSPACE_IMG.
	WorkspaceImg string `codec:"workspace_img"`

	// UserHomeImg is the absolute host path to the per-user $HOME ext4
	// image (raw, attached as virtio-blk → guest /dev/vdc → /userhome).
	// Wrapper env: ZSBX_USER_HOME_IMG.
	UserHomeImg string `codec:"user_home_img"`

	// RootfsSource is the absolute host path to the source rootfs image
	// the driver hardlinks (or copies on EXDEV fallback) into runDir
	// during the restore branch. C-7-LT-12a (smoke-r22).
	//
	// Background: the snapshot's config.json records `disks[0].path =
	// /opt/nomad/data/alloc/<OLD>/ch/local/rootfs.img` — an alloc-scoped
	// path that gets GC'd alongside the source alloc. The driver's
	// path-rewriter retargets the field to `<NEW-runDir>/rootfs.img` and
	// validates against the task_dir allow-list (C-7-LT-6), but pre-fix
	// NOTHING staged a real file at that destination — CH then aborted
	// at `VM Restore failed: DeviceManager(Disk(NotFound))`. workspace.img
	// and home.img survive the source-alloc teardown because they live at
	// stable persistent paths (`/var/zeroship/ch/<sbx>/workspace.img` and
	// `/var/zeroship/ch/users/<usr>/home.img`); rootfs.img is the only
	// disk that needs explicit staging on the restore branch.
	//
	// The controller emits this as `runtime_dir/rootfs-slim.img` — the
	// same source the cold-boot's materializeRootfs copies from. On the
	// restore branch the driver tries `os.Link` first (hardlink, O(1)
	// regardless of image size); falls back to a stdlib copy on EXDEV
	// (cross-device, e.g. runtime_dir on a separate filesystem from the
	// nomad alloc dir).
	//
	// Empty on cold-boot — only the restore branch consumes this. An
	// empty value on the restore branch surfaces as a clear error
	// (`rootfs_source is empty`) rather than the cryptic CH NotFound.
	RootfsSource string `codec:"rootfs_source"`

	// PubkeyHex is the lowercase-hex controller signing pubkey, no `0x`
	// prefix. Embedded in the cmdline as `zsbx_pubkey=<hex>`; the guest
	// /sbin/init decodes + writes /keys/controller-pubkey. Wrapper env:
	// ZSBX_PUBKEY_HEX. Validated even-length + hex-only at StartTask time.
	PubkeyHex string `codec:"pubkey_hex"`

	// SubnetBaseOctet is the second octet of the per-VM /30 subnet
	// (default 99, keeping the historical 10.99/16 layout). Used together
	// with VMIndex to derive the guest IP / host IP. Wrapper env:
	// ZSBX_SUBNET_BASE_OCTET. Bounded to u8 (0..255).
	SubnetBaseOctet uint16 `codec:"subnet_base_octet"`

	// StageDiskImages, when true, tells the driver to materialize the
	// per-alloc disk images (workspace.img and per-user home.img)
	// itself BEFORE spawning Cloud Hypervisor. This is the Option C
	// pivot from the 2026-05-25 staging-locality ADR (Phase 2): the
	// controller no longer stages disks on its local filesystem when
	// this flag is set — instead it emits the `WorkspaceImg` and
	// `UserHomeImg` paths declaratively, and the driver creates the
	// sparse files + mkfs.ext4 inside StartTask on the same node the
	// alloc lands on.
	//
	// When false (default), the legacy controller-side staging path
	// applies and the driver consumes pre-staged paths verbatim (the
	// `preflightDiskPaths` stat-check then catches a controller bug
	// before the spawn). Phase 4 flips the default to true after
	// stress-validating that driver-side staging collapses the
	// cross-alloc kernel-state retention surface the layer-peel rounds
	// 1-5 chased.
	//
	// Cold-boot only — the restore branch stages rootfs via its own
	// `RootfsSource` hardlink/copy (C-7-LT-12a) and consumes the
	// persistent `WorkspaceImg` / `UserHomeImg` from snapshot artifacts
	// per the ADR Phase 3 plan; the StageDiskImages flag has no effect
	// on the restore branch in Phase 2.
	StageDiskImages bool `codec:"stage_disk_images"`
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
	"cmdline":      hclspec.NewAttr("cmdline", "string", false),
	"cpus":         hclspec.NewAttr("cpus", "number", true),
	"memory_mb":    hclspec.NewAttr("memory_mb", "number", true),
	"restore_from": hclspec.NewAttr("restore_from", "string", false),

	// Wrapper-equivalent fields (see TaskConfig comments). All optional at
	// the schema level so existing operator configs that drive Disks/Cmdline
	// directly still validate; StartTask enforces the cold-boot subset.
	"sandbox_id":        hclspec.NewAttr("sandbox_id", "string", false),
	"user_id":           hclspec.NewAttr("user_id", "string", false),
	"workspace_img":     hclspec.NewAttr("workspace_img", "string", false),
	"user_home_img":     hclspec.NewAttr("user_home_img", "string", false),
	"rootfs_source":     hclspec.NewAttr("rootfs_source", "string", false),
	"pubkey_hex":        hclspec.NewAttr("pubkey_hex", "string", false),
	"subnet_base_octet":  hclspec.NewAttr("subnet_base_octet", "number", false),
	"stage_disk_images":  hclspec.NewAttr("stage_disk_images", "bool", false),

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
