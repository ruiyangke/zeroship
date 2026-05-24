// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-6 sprint test surface: pins the wake-from-snapshot path. Covers:
//
//   - rewriteConfigJSON branches: disks/path rewrite, net tap
//     rewrite + MAC preservation, unrelated keys pass through,
//     malformed input rejected.
//   - startTaskRestoreBranch end-to-ends: argv carries
//     `--restore source_url=file://<staged>`; API socket poll
//     succeeds when CH "binds" the socket; poll times out cleanly;
//     ch-remote resume invoked after poll; resume failure surfaces
//     stderr; persisted TaskState carries the spawned PID.
//   - Full call-sequence pinning via a recording seam:
//     rewrite_config → spawn(--restore) → wait_socket → resume.
//
// All tests use the package-level seams in ch/ (runnerFactory,
// pollAPISocketFn, resumeFn, setupTapFn, ensureTapUpFn) so no real
// cloud-hypervisor or ch-remote is required.

package tests

import (
	"encoding/json"
	"errors"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// snapshotConfigFixture is the shape of a CH `vm.create` payload
// that a controller-side snapshot would land in $RESTORE_FROM. The
// disk paths and serial.file deliberately point at a "source" alloc
// dir under /opt/nomad/data/alloc/ so the rewriter has something to
// match. The tap and mac mirror what cold-boot would have emitted.
func snapshotConfigFixture(srcAllocDir string) []byte {
	doc := map[string]any{
		"cpus":   map[string]any{"boot_vcpus": 2, "max_vcpus": 2},
		"memory": map[string]any{"size": 268435456, "shared": true},
		"payload": map[string]any{
			"kernel":  "/opt/zsbx/vmlinuz",
			"cmdline": "console=ttyS0 root=/dev/vda",
		},
		"disks": []any{
			map[string]any{
				"path":       srcAllocDir + "/rootfs.img",
				"readonly":   false,
				"image_type": "raw",
				"serial":     "zsbx-root",
			},
			map[string]any{
				"path":   srcAllocDir + "/workspace.img",
				"serial": "zsbx-work",
			},
		},
		"net": []any{
			map[string]any{
				"tap": "zsbx-nm-3", // source alloc's tap
				"mac": "12:34:56:78:9b:03",
			},
		},
		"serial": map[string]any{
			"mode": "File",
			"file": srcAllocDir + "/serial.log",
		},
		"console": map[string]any{"mode": "Off"},
	}
	b, err := json.Marshal(doc)
	if err != nil {
		panic(err)
	}
	return b
}

// stageSnapshotDir creates a fake snapshot artifact dir at
// t.TempDir()/snapshot containing the three CH-restore-required
// files (state.json, config.json, memory-ranges) so the validator
// passes. Returns the dir path so tests can pass it to
// driverConfig.RestoreFrom.
func stageSnapshotDir(t *testing.T, configBytes []byte) string {
	t.Helper()
	dir := filepath.Join(t.TempDir(), "snapshot")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir snapshot dir: %v", err)
	}
	mustWrite := func(name string, b []byte) {
		if err := os.WriteFile(filepath.Join(dir, name), b, 0o600); err != nil {
			t.Fatalf("write %s: %v", name, err)
		}
	}
	mustWrite("config.json", configBytes)
	// state.json + memory-ranges are opaque from the driver's
	// perspective — CH reads them; the validator only needs them to
	// exist. We write 1-byte sentinels so a stat() succeeds.
	mustWrite("state.json", []byte("{}"))
	mustWrite("memory-ranges", []byte{0})
	return dir
}

// validRestoreConfig returns a TaskConfig wired for the restore
// branch: VMIndex valid, RestoreFrom non-empty (caller wires it).
// The cold-boot fields (sandbox_id, workspace_img, …) are
// intentionally LEFT UNSET — the restore branch does not validate
// them; the snapshot's memory image carries the bound state.
func validRestoreConfig(restoreFrom string) ch.TaskConfig {
	return ch.TaskConfig{
		VMIndex:         3,
		SubnetBaseOctet: 99,
		RestoreFrom:     restoreFrom,
		// Operator-supplied Net entry so setup short-circuits to the
		// tap-up seam (which tests no-op) rather than shelling to
		// `ip tuntap add`.
		Net: []ch.NetSpec{{
			Tap:  "zsbx-nm-3",
			MAC:  "12:34:56:78:9b:03",
			IP:   "10.99.103.2",
			Mask: "255.255.255.252",
		}},
	}
}

// -- rewriteConfigJSON tests -----------------------------------------

func TestRewriteConfigJSON_RewritesDiskPaths(t *testing.T) {
	srcAllocDir := "/opt/nomad/data/alloc/AAAA-source/task/local"
	input := snapshotConfigFixture(srcAllocDir)
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"

	out, err := ch.RewriteConfigJSON(input, newTaskDir, 7, 99, "", "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}

	var doc map[string]any
	if err := json.Unmarshal(out, &doc); err != nil {
		t.Fatalf("unmarshal rewritten: %v", err)
	}
	disks := doc["disks"].([]any)
	if len(disks) != 2 {
		t.Fatalf("disks count = %d, want 2", len(disks))
	}
	d0 := disks[0].(map[string]any)
	wantD0 := newTaskDir + "/rootfs.img"
	if d0["path"] != wantD0 {
		t.Errorf("disks[0].path = %v, want %v", d0["path"], wantD0)
	}
	d1 := disks[1].(map[string]any)
	wantD1 := newTaskDir + "/workspace.img"
	if d1["path"] != wantD1 {
		t.Errorf("disks[1].path = %v, want %v", d1["path"], wantD1)
	}
	// serial.file is also a path-bearing key — rewrite must catch it.
	serial := doc["serial"].(map[string]any)
	wantSerial := newTaskDir + "/serial.log"
	if serial["file"] != wantSerial {
		t.Errorf("serial.file = %v, want %v", serial["file"], wantSerial)
	}
}

func TestRewriteConfigJSON_RewritesNetTap(t *testing.T) {
	srcAllocDir := "/opt/nomad/data/alloc/AAAA-source/task/local"
	input := snapshotConfigFixture(srcAllocDir)
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	newVMIndex := uint16(42)

	out, err := ch.RewriteConfigJSON(input, newTaskDir, newVMIndex, 99, "", "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}

	var doc map[string]any
	if err := json.Unmarshal(out, &doc); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	nets := doc["net"].([]any)
	if len(nets) != 1 {
		t.Fatalf("net count = %d, want 1", len(nets))
	}
	n0 := nets[0].(map[string]any)
	wantTap := "zsbx-nm-42"
	if n0["tap"] != wantTap {
		t.Errorf("net[0].tap = %v, want %v (rewrite from source vm_index 3 to new vm_index 42)", n0["tap"], wantTap)
	}
	// MAC must be PRESERVED across the rewrite — a stable MAC keeps
	// the guest's ARP cache valid post-resume.
	wantMAC := "12:34:56:78:9b:03"
	if n0["mac"] != wantMAC {
		t.Errorf("net[0].mac = %v, want %v (preserved, not rewritten)", n0["mac"], wantMAC)
	}

	// Disk paths should still rewrite — confirms the rewriter
	// handles both axes in one pass.
	disks := doc["disks"].([]any)
	d0 := disks[0].(map[string]any)
	if d0["path"] != newTaskDir+"/rootfs.img" {
		t.Errorf("disks[0].path = %v, want rewrite", d0["path"])
	}
}

// TestRewriteConfigJSON_PreservesMemoryRangesRef confirms that
// fields unrelated to the rewrite set (cpus, memory, payload, and
// any caller-supplied "memory-ranges-ref" sentinel) round-trip
// unchanged. The restore consumes memory-ranges from a SEPARATE
// staged file; the rewriter is config.json-only.
func TestRewriteConfigJSON_PreservesMemoryRangesRef(t *testing.T) {
	srcAllocDir := "/opt/nomad/data/alloc/AAAA-source/task/local"
	input := snapshotConfigFixture(srcAllocDir)
	// Inject a sentinel key that mirrors a hypothetical
	// memory-ranges reference. Marshal/unmarshal to keep the JSON
	// shape stable.
	var doc map[string]any
	if err := json.Unmarshal(input, &doc); err != nil {
		t.Fatalf("unmarshal seed: %v", err)
	}
	doc["memory_ranges_ref"] = "/var/lib/zsbx/snapshots/abc/memory-ranges"
	doc["snapshot_metadata"] = map[string]any{
		"taken_at":   "2026-05-24T00:00:00Z",
		"sandbox_id": "sbx_test",
	}
	seeded, err := json.Marshal(doc)
	if err != nil {
		t.Fatalf("marshal seed: %v", err)
	}

	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	out, err := ch.RewriteConfigJSON(seeded, newTaskDir, 7, 99, "", "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}

	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal rewritten: %v", err)
	}
	// Unknown keys must round-trip verbatim.
	if got["memory_ranges_ref"] != "/var/lib/zsbx/snapshots/abc/memory-ranges" {
		t.Errorf("memory_ranges_ref clobbered: %v", got["memory_ranges_ref"])
	}
	meta, ok := got["snapshot_metadata"].(map[string]any)
	if !ok {
		t.Fatalf("snapshot_metadata missing/wrong type: %T", got["snapshot_metadata"])
	}
	if meta["taken_at"] != "2026-05-24T00:00:00Z" {
		t.Errorf("snapshot_metadata.taken_at clobbered: %v", meta["taken_at"])
	}
	if meta["sandbox_id"] != "sbx_test" {
		t.Errorf("snapshot_metadata.sandbox_id clobbered: %v", meta["sandbox_id"])
	}
	// And the structured fields the rewriter DOES touch must be
	// rewritten so we know the rewriter ran (not silently no-op).
	disks := got["disks"].([]any)
	d0 := disks[0].(map[string]any)
	if d0["path"] != newTaskDir+"/rootfs.img" {
		t.Errorf("disks[0].path = %v, want rewritten (so we know the rewriter ran)", d0["path"])
	}
}

func TestRewriteConfigJSON_RejectsMalformedInput(t *testing.T) {
	cases := []struct {
		name string
		in   []byte
	}{
		{"empty", []byte{}},
		{"non-json junk", []byte("not json at all")},
		{"truncated", []byte(`{"disks": [{"path":`)},
		{"wrong type for disks", []byte(`{"disks": "not-a-list"}`)},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			_, err := ch.RewriteConfigJSON(tc.in, "/opt/nomad/data/alloc/x/y/local", 1, 99, "", "", nil)
			if err == nil {
				t.Fatalf("expected error for %s", tc.name)
			}
		})
	}
}

// -- C-7-LT-4 R15-S2 allow-list tests --------------------------------
//
// Mirrors the bash wrapper's assert_under_task_dir guard at
// nomad-vm-wrapper.sh:537-583. Each test exercises ONE security
// invariant; failures must surface a clear operator-facing error that
// names both the offending field and the expected prefix.

// TestRewriteRestoreConfigPaths_AllPathFieldsRewritten is the happy-
// path witness: a snapshot whose disks[].path / serial.file /
// console.file / fs[].socket all sit under the source-alloc prefix
// rewrites cleanly, and EVERY post-rewrite value lives under the new
// task dir. Pins the wrapper-parity contract: any path-bearing field
// the bash wrapper rewrites, the Go port must also rewrite.
func TestRewriteRestoreConfigPaths_AllPathFieldsRewritten(t *testing.T) {
	src := "/opt/nomad/data/alloc/AAAA-source/task/local"
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	doc := map[string]any{
		"disks": []any{
			map[string]any{"path": src + "/rootfs.img"},
			map[string]any{"path": src + "/workspace.img"},
		},
		"serial":  map[string]any{"file": src + "/serial.log"},
		"console": map[string]any{"file": src + "/console.log"},
		"fs": []any{
			map[string]any{"socket": src + "/vfs.sock"},
		},
	}
	in, err := json.Marshal(doc)
	if err != nil {
		t.Fatalf("marshal seed: %v", err)
	}

	out, err := ch.RewriteConfigJSON(in, newTaskDir, 7, 99, "", "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	disks := got["disks"].([]any)
	wantDisk0 := newTaskDir + "/rootfs.img"
	if d0 := disks[0].(map[string]any); d0["path"] != wantDisk0 {
		t.Errorf("disks[0].path = %v, want %v", d0["path"], wantDisk0)
	}
	wantDisk1 := newTaskDir + "/workspace.img"
	if d1 := disks[1].(map[string]any); d1["path"] != wantDisk1 {
		t.Errorf("disks[1].path = %v, want %v", d1["path"], wantDisk1)
	}
	wantSerial := newTaskDir + "/serial.log"
	if s := got["serial"].(map[string]any); s["file"] != wantSerial {
		t.Errorf("serial.file = %v, want %v", s["file"], wantSerial)
	}
	wantConsole := newTaskDir + "/console.log"
	if c := got["console"].(map[string]any); c["file"] != wantConsole {
		t.Errorf("console.file = %v, want %v", c["file"], wantConsole)
	}
	fs := got["fs"].([]any)
	wantSock := newTaskDir + "/vfs.sock"
	if f0 := fs[0].(map[string]any); f0["socket"] != wantSock {
		t.Errorf("fs[0].socket = %v, want %v", f0["socket"], wantSock)
	}
}

// TestRewriteRestoreConfigPaths_PreservesNonPathFields is the
// witness for the "rewrite touches only known path fields" contract:
// memory, cpus, payload (kernel + cmdline), and any operator-supplied
// metadata round-trip byte-equal under the JSON map.
func TestRewriteRestoreConfigPaths_PreservesNonPathFields(t *testing.T) {
	src := "/opt/nomad/data/alloc/AAAA-source/task/local"
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	doc := map[string]any{
		"cpus":   map[string]any{"boot_vcpus": float64(2), "max_vcpus": float64(4)},
		"memory": map[string]any{"size": float64(536870912), "shared": true},
		"payload": map[string]any{
			"kernel":  "/opt/zsbx/vmlinuz",
			"cmdline": "console=ttyS0 root=/dev/vda ro",
		},
		"disks": []any{
			map[string]any{"path": src + "/rootfs.img"},
		},
		"sandbox_metadata": map[string]any{
			"taken_at":   "2026-05-23T12:00:00Z",
			"created_by": "controller-v6",
		},
	}
	in, err := json.Marshal(doc)
	if err != nil {
		t.Fatalf("marshal seed: %v", err)
	}

	out, err := ch.RewriteConfigJSON(in, newTaskDir, 1, 99, "", "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	cpus := got["cpus"].(map[string]any)
	if cpus["boot_vcpus"].(float64) != 2 || cpus["max_vcpus"].(float64) != 4 {
		t.Errorf("cpus clobbered: %v", cpus)
	}
	mem := got["memory"].(map[string]any)
	if mem["size"].(float64) != 536870912 || mem["shared"].(bool) != true {
		t.Errorf("memory clobbered: %v", mem)
	}
	payload := got["payload"].(map[string]any)
	if payload["kernel"] != "/opt/zsbx/vmlinuz" {
		t.Errorf("payload.kernel clobbered: %v", payload["kernel"])
	}
	if payload["cmdline"] != "console=ttyS0 root=/dev/vda ro" {
		t.Errorf("payload.cmdline clobbered: %v", payload["cmdline"])
	}
	meta := got["sandbox_metadata"].(map[string]any)
	if meta["taken_at"] != "2026-05-23T12:00:00Z" || meta["created_by"] != "controller-v6" {
		t.Errorf("sandbox_metadata clobbered: %v", meta)
	}
}

// TestRewriteRestoreConfigPaths_RejectsPathOutsideAllocPrefix is the
// R15-S2 security pin: a snapshot whose disks[].path points at
// `/etc/shadow` (or any other path that doesn't match the alloc
// prefix) is REJECTED. Pre-C-7-LT-4 the rewriter silently passed
// such values through; CH would then have opened them verbatim.
func TestRewriteRestoreConfigPaths_RejectsPathOutsideAllocPrefix(t *testing.T) {
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"

	cases := []struct {
		name       string
		field      string
		buildDoc   func() map[string]any
		wantField  string
		wantOffend string
	}{
		{
			name:  "disks[0].path /etc/shadow",
			field: "disks[0].path",
			buildDoc: func() map[string]any {
				return map[string]any{
					"disks": []any{map[string]any{"path": "/etc/shadow"}},
				}
			},
			wantField:  "disks[0].path",
			wantOffend: "/etc/shadow",
		},
		{
			name:  "serial.file /etc/passwd",
			field: "serial.file",
			buildDoc: func() map[string]any {
				return map[string]any{
					"serial": map[string]any{"file": "/etc/passwd"},
				}
			},
			wantField:  "serial.file",
			wantOffend: "/etc/passwd",
		},
		{
			name:  "console.file outside alloc",
			field: "console.file",
			buildDoc: func() map[string]any {
				return map[string]any{
					"console": map[string]any{"file": "/var/log/wtmp"},
				}
			},
			wantField:  "console.file",
			wantOffend: "/var/log/wtmp",
		},
		{
			name:  "fs[0].socket outside alloc",
			field: "fs[0].socket",
			buildDoc: func() map[string]any {
				return map[string]any{
					"fs": []any{map[string]any{"socket": "/run/attacker.sock"}},
				}
			},
			wantField:  "fs[0].socket",
			wantOffend: "/run/attacker.sock",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			in, err := json.Marshal(tc.buildDoc())
			if err != nil {
				t.Fatalf("marshal: %v", err)
			}
			_, err = ch.RewriteConfigJSON(in, newTaskDir, 1, 99, "", "", nil)
			if err == nil {
				t.Fatalf("expected RewriteConfigJSON to reject %s, got nil error", tc.name)
			}
			msg := err.Error()
			mustContainTest(t, "reject message", msg, tc.wantField)
			mustContainTest(t, "reject message", msg, tc.wantOffend)
			mustContainTest(t, "reject message", msg, newTaskDir)
		})
	}
}

// TestRewriteRestoreConfigPaths_RejectsParentTraversal is the
// path-traversal defence: any path containing a `..` component (even
// one anchored under the alloc prefix) is rejected before the
// containment check runs.
func TestRewriteRestoreConfigPaths_RejectsParentTraversal(t *testing.T) {
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	src := "/opt/nomad/data/alloc/AAAA-source/task/local"

	cases := []struct {
		name string
		path string
	}{
		// `..` from inside the source alloc that would escape the
		// rewritten taskDir. The wrapper rejects these BEFORE realpath
		// because the explicit component scan is faster and louder.
		{"escapes via .. after alloc prefix", src + "/../../../etc/shadow"},
		// Raw `..` even when the rest is benign-looking.
		{"intra-alloc ..", src + "/../sibling-alloc/local/rootfs.img"},
		// Relative path with `..` (not absolute, rejected at the
		// absolute-path guard, but documented here for completeness).
		{"relative with ..", "../escape"},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			doc := map[string]any{
				"disks": []any{map[string]any{"path": tc.path}},
			}
			in, err := json.Marshal(doc)
			if err != nil {
				t.Fatalf("marshal: %v", err)
			}
			_, err = ch.RewriteConfigJSON(in, newTaskDir, 1, 99, "", "", nil)
			if err == nil {
				t.Fatalf("expected RewriteConfigJSON to reject path %q, got nil", tc.path)
			}
			msg := err.Error()
			mustContainTest(t, "traversal-reject message", msg, "disks[0].path")
		})
	}
}

// TestRewriteRestoreConfigPaths_HandlesMissingOptionalFields confirms
// that snapshots without one or more of the optional path-bearing
// fields (no serial, no console, no fs) do NOT error — the rewriter
// must touch only what's present and pass everything else through.
func TestRewriteRestoreConfigPaths_HandlesMissingOptionalFields(t *testing.T) {
	src := "/opt/nomad/data/alloc/AAAA-source/task/local"
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	// Only disks present; no serial, no console, no fs.
	doc := map[string]any{
		"disks": []any{map[string]any{"path": src + "/rootfs.img"}},
	}
	in, err := json.Marshal(doc)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	out, err := ch.RewriteConfigJSON(in, newTaskDir, 1, 99, "", "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON (missing-optional-fields): %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	disks := got["disks"].([]any)
	d0 := disks[0].(map[string]any)
	if d0["path"] != newTaskDir+"/rootfs.img" {
		t.Errorf("disks[0].path = %v, want rewrite", d0["path"])
	}
	// serial / console / fs must remain absent.
	if _, ok := got["serial"]; ok {
		t.Errorf("serial unexpectedly present in output: %v", got["serial"])
	}
	if _, ok := got["console"]; ok {
		t.Errorf("console unexpectedly present in output: %v", got["console"])
	}
	if _, ok := got["fs"]; ok {
		t.Errorf("fs unexpectedly present in output: %v", got["fs"])
	}
}

// TestStartTaskRestore_RewritesConfigBeforeCHSpawn is the integration
// witness for C-7-LT-4: the rewritten config.json on disk after
// StartTask returns MUST point at the NEW alloc's task_dir and MUST
// NOT contain the source-alloc path. This is the call-site pin —
// confirms restore_task.go invokes the rewriter BEFORE handing the
// config off to CH.
func TestStartTaskRestore_RewritesConfigBeforeCHSpawn(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA-source/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	taskDir := t.TempDir()
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}

	// runDir is taskDir/local — same convention as the existing
	// TestStartTaskRestore_FullSequencePinning above.
	runDir := filepath.Join(taskDir, "local")
	rewritten, err := os.ReadFile(filepath.Join(runDir, "config.json"))
	if err != nil {
		t.Fatalf("rewritten config.json missing under runDir %s: %v", runDir, err)
	}
	rewrittenStr := string(rewritten)
	// Pin 1: source-alloc path must NOT appear in the rewritten doc.
	if strings.Contains(rewrittenStr, srcAlloc) {
		t.Errorf("rewritten config still references source alloc %q", srcAlloc)
	}
	// Pin 2: rewritten disk paths land under runDir. The fixture
	// declares two disks (rootfs.img, workspace.img) — both must
	// resolve to runDir-prefixed paths.
	var doc map[string]any
	if err := json.Unmarshal(rewritten, &doc); err != nil {
		t.Fatalf("rewritten config.json not valid JSON: %v", err)
	}
	disks, ok := doc["disks"].([]any)
	if !ok || len(disks) == 0 {
		t.Fatalf("disks missing from rewritten config: %v", doc)
	}
	for i, d := range disks {
		dm := d.(map[string]any)
		p, _ := dm["path"].(string)
		if !strings.HasPrefix(p, runDir) {
			t.Errorf("disks[%d].path = %q does not have runDir prefix %q", i, p, runDir)
		}
	}
}

// TestStartTaskRestore_RejectsMaliciousSnapshotBeforeCHSpawn is the
// integration witness for the R15-S2 reject path at the call site:
// a snapshot config.json whose disks[].path is `/etc/shadow` (or any
// out-of-alloc path) must abort StartTask BEFORE CH spawns. The
// witness: the runner factory is NEVER invoked (no spawn(--restore)
// recorded) and the returned error names both the offending field
// and the malicious value.
func TestStartTaskRestore_RejectsMaliciousSnapshotBeforeCHSpawn(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	// Build a malicious snapshot dir: config.json carries an absolute
	// path outside the alloc-prefix.
	maliciousDoc := map[string]any{
		"cpus":   map[string]any{"boot_vcpus": 2, "max_vcpus": 2},
		"memory": map[string]any{"size": 268435456},
		"disks": []any{
			map[string]any{"path": "/etc/shadow"},
		},
	}
	cfgBytes, err := json.Marshal(maliciousDoc)
	if err != nil {
		t.Fatalf("marshal malicious doc: %v", err)
	}
	staged := stageSnapshotDir(t, cfgBytes)

	rec := &restoreSequenceRecorder{}
	capturedPtr, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	_, _, err = p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected StartTask to reject malicious snapshot, got nil error")
	}
	msg := err.Error()
	mustContainTest(t, "malicious-snapshot reject", msg, "disks[0].path")
	mustContainTest(t, "malicious-snapshot reject", msg, "/etc/shadow")

	// Runner factory must NOT have been invoked — abort happens before
	// CH spawns.
	if captured := *capturedPtr; captured != nil {
		t.Errorf("runner factory invoked despite reject: captured.argv=%v", captured.argv)
	}
	// And no spawn(--restore) step recorded.
	for _, step := range rec.snapshot() {
		if step == "spawn(--restore)" {
			t.Errorf("spawn(--restore) fired despite reject; steps=%v", rec.snapshot())
		}
	}
}

// -- restoreSequenceRecorder -----------------------------------------

// restoreSequenceRecorder captures the ordered list of wake-path
// steps as they fire. Each seam wrapper appends its tag; the test
// asserts the full sequence at the end.
type restoreSequenceRecorder struct {
	mu    sync.Mutex
	steps []string
}

func (r *restoreSequenceRecorder) record(step string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.steps = append(r.steps, step)
}

func (r *restoreSequenceRecorder) snapshot() []string {
	r.mu.Lock()
	defer r.mu.Unlock()
	out := make([]string, len(r.steps))
	copy(out, r.steps)
	return out
}

// installRestoreSeams installs poll + resume + tap-up seams that
// record into the recorder and return the supplied outcomes. The
// returned cleanup fn restores the previous seam values.
//
// The runner factory wrapper records "spawn(--restore)" when the
// fake runner is constructed (Start is the moment the argv lands;
// the test consults capturedRunner.argv afterwards).
type restoreSeamOutcomes struct {
	pollErr    error
	resumeErr  error
	pollDelay  time.Duration // how long pollFn blocks before returning
	tapUpErr   error
}

func installRestoreSeams(t *testing.T, rec *restoreSequenceRecorder, out restoreSeamOutcomes) (capturedRunnerPtr **fakeRunner, factory func(cmd *exec.Cmd) ch.ProcessRunnerSeam) {
	t.Helper()

	var captured *fakeRunner
	factory = func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		rec.record("spawn(--restore)")
		captured = newFakeRunner(cmd)
		// Drive Wait to return cleanly so the supervisor goroutine
		// doesn't dangle past the test. The fake runner's Wait
		// blocks on r.waitCh until the test closes it.
		go func(r *fakeRunner) {
			// Closed in t.Cleanup below.
			<-r.waitCh
		}(captured)
		return captured
	}

	prevPoll := ch.SetPollAPISocketForTest(func(c *ch.Client, sock string, timeout, interval time.Duration) error {
		rec.record("wait_socket")
		if out.pollDelay > 0 {
			time.Sleep(out.pollDelay)
		}
		return out.pollErr
	})
	prevResume := ch.SetResumeForTest(func(c *ch.Client, sock string) error {
		rec.record("resume")
		return out.resumeErr
	})
	prevTap := ch.SetEnsureTapUpForTest(func(string) error {
		return out.tapUpErr
	})

	t.Cleanup(func() {
		ch.SetPollAPISocketForTest(prevPoll)
		ch.SetResumeForTest(prevResume)
		ch.SetEnsureTapUpForTest(prevTap)
		// Unblock the fake runner's Wait so the supervisor exits
		// cleanly under -race.
		if captured != nil {
			func() {
				defer func() { recover() }() // already-closed safe
				close(captured.waitCh)
			}()
		}
	})

	capturedRunnerPtr = &captured
	return capturedRunnerPtr, factory
}

// Note on "rewrite_config" witnessing: rewriteConfigJSON is a pure
// function the restore branch calls inline with no seam. The
// full-sequence test below witnesses the rewrite step by stat'ing
// the rewritten config in runDir AFTER StartTask returns — the
// restore branch always writes that file before spawning CH, so
// its presence (with the source-alloc path scrubbed) is proof the
// rewrite happened first.

// -- startTaskRestoreBranch end-to-end tests -------------------------

func TestStartTaskRestore_SpawnsWithRestoreFlag(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	chRemote := writeStubBinary(t, "ch-remote")
	t.Setenv("ZSBX_CH_BIN", chBin)
	t.Setenv("ZSBX_CH_REMOTE_BIN", chRemote)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	capturedPtr, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	handle, _, err := p.StartTask(taskCfg)
	if err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}
	if handle == nil {
		t.Fatal("nil handle")
	}
	captured := *capturedPtr
	if captured == nil {
		t.Fatal("runner factory not invoked")
	}

	argv := captured.argv
	if len(argv) == 0 || argv[0] != chBin {
		t.Errorf("argv[0] = %q, want %q", argv[0], chBin)
	}
	if !containsAdjacent(argv, "--api-socket") {
		t.Errorf("argv missing --api-socket: %v", argv)
	}
	if !containsAdjacent(argv, "--restore") {
		t.Errorf("argv missing --restore: %v", argv)
	}
	restoreVal := argvAfter(argv, "--restore")
	wantPrefix := "source_url=file://"
	if !strings.HasPrefix(restoreVal, wantPrefix) {
		t.Errorf("--restore arg = %q, want prefix %q", restoreVal, wantPrefix)
	}
	if !strings.HasSuffix(restoreVal, staged) {
		t.Errorf("--restore arg = %q, want suffix %q (staged dir)", restoreVal, staged)
	}
	// Cold-boot-only flags MUST be absent — they'd conflict with
	// --restore (per CH docs).
	for _, banned := range []string{"--kernel", "--cmdline", "--disk", "--net", "--memory", "--cpus", "--serial", "--config"} {
		if containsAdjacent(argv, banned) {
			t.Errorf("argv carries %s which conflicts with --restore: %v", banned, argv)
		}
	}
}

func TestStartTaskRestore_PollsApiSocket(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{
		pollDelay: 50 * time.Millisecond,
	})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	start := time.Now()
	_, _, err := p.StartTask(taskCfg)
	if err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}
	elapsed := time.Since(start)
	if elapsed < 50*time.Millisecond {
		t.Errorf("StartTask returned in %v; expected >= 50ms (the poll delay)", elapsed)
	}
	if !containsString(rec.snapshot(), "wait_socket") {
		t.Errorf("wait_socket step not recorded; steps=%v", rec.snapshot())
	}
}

func TestStartTaskRestore_BailsOnSocketTimeout(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{
		pollErr: errors.New("ch: api socket not responsive at /tmp/x within 10ms"),
	})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected StartTask to fail when poll times out")
	}
	if !strings.Contains(err.Error(), "api socket not responsive") {
		t.Errorf("error %q does not surface poll-timeout message", err.Error())
	}
	// Resume must NOT have been called once poll failed.
	for _, step := range rec.snapshot() {
		if step == "resume" {
			t.Errorf("resume should not fire when poll timed out; steps=%v", rec.snapshot())
		}
	}
}

func TestStartTaskRestore_IssuesChRemoteResume(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}

	steps := rec.snapshot()
	if !containsString(steps, "resume") {
		t.Fatalf("resume step not recorded; steps=%v", steps)
	}
	// Resume must come AFTER wait_socket.
	resumeIdx := indexOf(steps, "resume")
	socketIdx := indexOf(steps, "wait_socket")
	if resumeIdx < 0 || socketIdx < 0 || resumeIdx <= socketIdx {
		t.Errorf("resume should follow wait_socket; steps=%v", steps)
	}
}

func TestStartTaskRestore_BailsIfResumeFails(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{
		resumeErr: errors.New("ch-remote resume: exit status 1 (output=\"VM not found\")"),
	})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected StartTask to fail when resume fails")
	}
	if !strings.Contains(err.Error(), "resume failed") {
		t.Errorf("error %q does not mention 'resume failed'", err.Error())
	}
	// Stderr-bearing message must be preserved.
	if !strings.Contains(err.Error(), "VM not found") {
		t.Errorf("error %q does not preserve resume stderr", err.Error())
	}
}

func TestStartTaskRestore_PersistsTaskStateWithNewPID(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	capturedPtr, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, t.TempDir(), factory)

	handle, _, err := p.StartTask(taskCfg)
	if err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}
	captured := *capturedPtr
	wantPid := captured.pid // fake runner's pid = 424242 by default
	if wantPid == 0 {
		t.Fatal("fake runner pid is 0")
	}

	var state ch.TaskState
	if err := handle.GetDriverState(&state); err != nil {
		t.Fatalf("GetDriverState: %v", err)
	}
	if state.CHPid != wantPid {
		t.Errorf("TaskState.CHPid = %d, want %d", state.CHPid, wantPid)
	}
	if state.Mode != "restore" {
		t.Errorf("TaskState.Mode = %q, want %q", state.Mode, "restore")
	}
	if !strings.HasSuffix(state.APISocket, "ch.sock") {
		t.Errorf("TaskState.APISocket = %q, want suffix ch.sock", state.APISocket)
	}
	if state.VMIndex != cfg.VMIndex {
		t.Errorf("TaskState.VMIndex = %d, want %d", state.VMIndex, cfg.VMIndex)
	}
}

// TestStartTaskRestore_FullSequencePinning is the witness for the
// wake-path call order: rewrite_config → spawn(--restore) →
// wait_socket → resume. Implemented via the recording seam +
// a post-condition check that the rewritten config landed in the
// runDir (which is restore_task.go's pre-spawn step).
func TestStartTaskRestore_FullSequencePinning(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	taskDir := t.TempDir()
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}

	// "rewrite_config" is witnessed by the rewritten config landing
	// in the runDir. Determine the runDir the same way StartTask
	// does: Nomad's TaskDir().LocalDir resolves to taskDir/local
	// (filepath.Join(AllocDir, Name, "local")).
	runDir := filepath.Join(taskDir, "local")
	rewritten, err := os.ReadFile(filepath.Join(runDir, "config.json"))
	if err != nil {
		t.Fatalf("rewritten config.json missing from runDir %s: %v", runDir, err)
	}
	// The rewriter must have updated the disk path away from the
	// source alloc dir.
	if strings.Contains(string(rewritten), srcAlloc) {
		t.Errorf("rewritten config still references source alloc %q", srcAlloc)
	}
	// And the rewritten config must be valid JSON.
	var doc map[string]any
	if err := json.Unmarshal(rewritten, &doc); err != nil {
		t.Fatalf("rewritten config.json is not valid JSON: %v", err)
	}

	// Seam-driven steps in expected order.
	steps := rec.snapshot()
	want := []string{"spawn(--restore)", "wait_socket", "resume"}
	if !sequenceMatches(steps, want) {
		t.Errorf("step sequence = %v, want subsequence containing %v in order", steps, want)
	}

	// Also assert "spawn(--restore)" comes BEFORE "wait_socket"
	// which comes BEFORE "resume" — the pin the brief calls out
	// explicitly. (rewrite_config is witnessed above by the
	// on-disk artifact; the sequence below pins the remaining 3.)
	if idx := atomicIdx(steps); idx.spawn < 0 || idx.poll < 0 || idx.resume < 0 ||
		!(idx.spawn < idx.poll && idx.poll < idx.resume) {
		t.Errorf("sequence violated: spawn=%d poll=%d resume=%d (steps=%v)", idx.spawn, idx.poll, idx.resume, steps)
	}
}

// -- helpers ---------------------------------------------------------

func containsString(haystack []string, needle string) bool {
	for _, s := range haystack {
		if s == needle {
			return true
		}
	}
	return false
}

func indexOf(haystack []string, needle string) int {
	for i, s := range haystack {
		if s == needle {
			return i
		}
	}
	return -1
}

// sequenceMatches returns true if `want` appears as an in-order
// subsequence of `got` (other steps in between are allowed).
func sequenceMatches(got, want []string) bool {
	i := 0
	for _, g := range got {
		if i < len(want) && g == want[i] {
			i++
		}
	}
	return i == len(want)
}

type stepIdx struct {
	spawn, poll, resume int
}

func atomicIdx(steps []string) stepIdx {
	idx := stepIdx{spawn: -1, poll: -1, resume: -1}
	for i, s := range steps {
		switch s {
		case "spawn(--restore)":
			idx.spawn = i
		case "wait_socket":
			idx.poll = i
		case "resume":
			idx.resume = i
		}
	}
	return idx
}

// -- C-7-LT-3-PR1 waitForCHSocketReady tests -------------------------

// TestWaitForCHSocketReady_Happy: a goroutine binds a Unix socket
// mid-loop; the probe returns nil within budget.
//
// Pins the first-success behaviour: once Dial succeeds the helper
// returns immediately rather than consuming the remaining budget.
func TestWaitForCHSocketReady_Happy(t *testing.T) {
	sockDir := t.TempDir()
	sockPath := filepath.Join(sockDir, "ch.sock")

	// Bind the listener after a short delay so the probe enters its
	// retry loop at least once. Tighter than the cadence so the
	// first retry attempt sees a live listener.
	listenerReady := make(chan struct{})
	var ln net.Listener
	go func() {
		time.Sleep(150 * time.Millisecond)
		var err error
		ln, err = net.Listen("unix", sockPath)
		if err != nil {
			t.Errorf("net.Listen unix %s: %v", sockPath, err)
			close(listenerReady)
			return
		}
		close(listenerReady)
		// Accept-loop so the probe's Dial doesn't observe immediate
		// connection refused after acceptance opens.
		go func() {
			for {
				c, err := ln.Accept()
				if err != nil {
					return
				}
				_ = c.Close()
			}
		}()
	}()
	t.Cleanup(func() {
		<-listenerReady
		if ln != nil {
			_ = ln.Close()
		}
	})

	start := time.Now()
	err := ch.WaitForCHSocketReady(sockPath, 2*time.Second, 200*time.Millisecond, 50*time.Millisecond)
	elapsed := time.Since(start)
	if err != nil {
		t.Fatalf("WaitForCHSocketReady (happy): %v", err)
	}
	// Must return shortly after the listener becomes ready —
	// definitely under 1s (much less than the 2s budget).
	if elapsed > time.Second {
		t.Errorf("WaitForCHSocketReady took %v; expected first-success well under 1s", elapsed)
	}
	// And must have waited at least one cadence tick (the listener
	// is delayed 150ms; we'd expect at least one failed Dial).
	if elapsed < 100*time.Millisecond {
		t.Errorf("WaitForCHSocketReady returned in %v; suspiciously fast (did the helper actually probe?)", elapsed)
	}
}

// TestWaitForCHSocketReady_Timeout: no listener ever exists; the
// probe runs the full budget and returns an error naming attempt
// count + lastErr + budget so an operator can see what happened.
func TestWaitForCHSocketReady_Timeout(t *testing.T) {
	sockDir := t.TempDir()
	sockPath := filepath.Join(sockDir, "ch.sock")
	// Deliberately do NOT create the socket.

	start := time.Now()
	err := ch.WaitForCHSocketReady(sockPath, 250*time.Millisecond, 50*time.Millisecond, 25*time.Millisecond)
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("WaitForCHSocketReady: expected timeout error, got nil")
	}
	// Error shape: must name the budget, attempt count, and lastErr.
	msg := err.Error()
	mustContainTest(t, "timeout error", msg, sockPath)
	mustContainTest(t, "timeout error", msg, "not responsive")
	mustContainTest(t, "timeout error", msg, "attempts=")
	mustContainTest(t, "timeout error", msg, "lastErr=")
	mustContainTest(t, "timeout error", msg, "250ms")

	// Elapsed must be close to the budget (within +/- 50% slack).
	if elapsed < 200*time.Millisecond {
		t.Errorf("WaitForCHSocketReady returned in %v; expected ~250ms (the budget)", elapsed)
	}
	if elapsed > 500*time.Millisecond {
		t.Errorf("WaitForCHSocketReady took %v; expected near-budget (~250ms), well under 500ms", elapsed)
	}
}

// TestWaitForCHSocketReady_AcceptAtBudgetEdge: the listener appears
// right at the deadline boundary. The probe must still succeed if
// it can dial before the budget expires; the helper's deadline
// math should not exit early.
func TestWaitForCHSocketReady_AcceptAtBudgetEdge(t *testing.T) {
	sockDir := t.TempDir()
	sockPath := filepath.Join(sockDir, "ch.sock")

	// Bind the listener at ~250ms — close enough to the 500ms
	// budget edge that any off-by-one in the deadline math would
	// surface as a spurious timeout.
	listenerReady := make(chan struct{})
	var ln net.Listener
	go func() {
		time.Sleep(250 * time.Millisecond)
		var err error
		ln, err = net.Listen("unix", sockPath)
		if err != nil {
			close(listenerReady)
			return
		}
		close(listenerReady)
		go func() {
			for {
				c, err := ln.Accept()
				if err != nil {
					return
				}
				_ = c.Close()
			}
		}()
	}()
	t.Cleanup(func() {
		<-listenerReady
		if ln != nil {
			_ = ln.Close()
		}
	})

	err := ch.WaitForCHSocketReady(sockPath, 500*time.Millisecond, 50*time.Millisecond, 25*time.Millisecond)
	if err != nil {
		t.Fatalf("WaitForCHSocketReady at budget edge: %v", err)
	}
}

// TestWaitForCHSocketReady_EmptyPath asserts the defensive guard:
// passing an empty path returns an immediate error, doesn't enter
// the loop, and doesn't consume the budget.
func TestWaitForCHSocketReady_EmptyPath(t *testing.T) {
	start := time.Now()
	err := ch.WaitForCHSocketReady("", time.Minute, 200*time.Millisecond, 100*time.Millisecond)
	elapsed := time.Since(start)
	if err == nil {
		t.Fatal("expected error for empty sockPath, got nil")
	}
	if elapsed > 100*time.Millisecond {
		t.Errorf("empty-path check took %v; expected immediate return", elapsed)
	}
	mustContainTest(t, "empty-path error", err.Error(), "empty socket path")
}

// -- C-7-LT-3-PR2 stderr-capture tests -------------------------------

// TestStartTaskRestoreBranch_StderrCaptured: drive a restore that
// reaches the spawn step; assert the per-alloc stderr file is
// created under the run dir at the expected name. Pins the
// PR2 wire-level constant ChStderrLogName.
//
// The fake runner does not actually exec — but the spawn block opens
// the stderr file BEFORE calling the runner factory, so the file's
// existence post-StartTask is the witness.
func TestStartTaskRestoreBranch_StderrCaptured(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	rec := &restoreSequenceRecorder{}
	_, factory := installRestoreSeams(t, rec, restoreSeamOutcomes{})

	cfg := validRestoreConfig(staged)
	taskDir := t.TempDir()
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)

	if _, _, err := p.StartTask(taskCfg); err != nil {
		t.Fatalf("StartTask (restore): %v", err)
	}

	// Witness the stderr-log file under runDir. The restore branch
	// uses TaskDir().LocalDir which the helper wires to
	// taskDir/local.
	runDir := filepath.Join(taskDir, "local")
	stderrPath := filepath.Join(runDir, ch.ChStderrLogName)
	if _, err := os.Stat(stderrPath); err != nil {
		t.Fatalf("expected ch stderr log at %s, got: %v", stderrPath, err)
	}
}

// TestStartTaskRestoreBranch_StderrTailEmbeddedOnSocketTimeout:
// when the API-socket probe times out, the error message must
// embed the tail of CH stderr. The fake runner's stderr buffer
// stands in for what CH would have written; we pre-populate the
// on-disk stderr file directly (the spawn block opens it with
// O_TRUNC so we can't pre-stage via the file alone — instead the
// fake runner writes to cmd.Stderr during the test's spawn step
// which the production io.MultiWriter tees to the file).
//
// Simpler witness: drive a poll-timeout, write a marker into the
// on-disk file mid-flight, and assert the marker surfaces in the
// returned error.
func TestStartTaskRestoreBranch_StderrTailEmbeddedOnSocketTimeout(t *testing.T) {
	chBin := writeStubBinary(t, "cloud-hypervisor")
	t.Setenv("ZSBX_CH_BIN", chBin)

	srcAlloc := "/opt/nomad/data/alloc/AAAA/task/local"
	staged := stageSnapshotDir(t, snapshotConfigFixture(srcAlloc))

	// Custom poll seam: write a marker into the stderr file then
	// return a timeout error. This simulates CH having logged to
	// stderr before the socket-timeout fires.
	rec := &restoreSequenceRecorder{}
	taskDir := t.TempDir()
	runDir := filepath.Join(taskDir, "local")
	stderrPath := filepath.Join(runDir, ch.ChStderrLogName)

	prevPoll := ch.SetPollAPISocketForTest(func(_ *ch.Client, _ string, _, _ time.Duration) error {
		rec.record("wait_socket")
		// File was created by the spawn block; append a marker.
		f, err := os.OpenFile(stderrPath, os.O_WRONLY|os.O_APPEND, 0o600)
		if err != nil {
			return errors.New("ch: api socket not responsive (within 10ms, marker-write-failed)")
		}
		_, _ = f.WriteString("PANIC: synthetic CH crash output for test\n")
		_ = f.Close()
		return errors.New("ch: api socket not responsive at /tmp/x within 10ms (attempts=1, lastErr=connection refused)")
	})
	t.Cleanup(func() { ch.SetPollAPISocketForTest(prevPoll) })

	prevTap := ch.SetEnsureTapUpForTest(func(string) error { return nil })
	t.Cleanup(func() { ch.SetEnsureTapUpForTest(prevTap) })

	var captured *fakeRunner
	factory := func(cmd *exec.Cmd) ch.ProcessRunnerSeam {
		captured = newFakeRunner(cmd)
		go func(r *fakeRunner) { <-r.waitCh }(captured)
		return captured
	}
	t.Cleanup(func() {
		if captured != nil {
			func() {
				defer func() { recover() }()
				close(captured.waitCh)
			}()
		}
	})

	cfg := validRestoreConfig(staged)
	p, taskCfg := newTestPluginWithFactory(t, &cfg, taskDir, factory)

	_, _, err := p.StartTask(taskCfg)
	if err == nil {
		t.Fatal("expected socket-timeout error")
	}
	msg := err.Error()
	mustContainTest(t, "socket-timeout error", msg, "PANIC: synthetic CH crash")
	mustContainTest(t, "socket-timeout error", msg, "ch_stderr_tail=")
	mustContainTest(t, "socket-timeout error", msg, stderrPath)
}

// mustContainTest is a local helper to keep these C-7-LT-3 tests
// self-contained (start_task_test.go has the package-wide
// mustContain helper but it takes a different signature shape; the
// alias here is purely for readability).
func mustContainTest(t *testing.T, label, haystack, needle string) {
	t.Helper()
	if !strings.Contains(haystack, needle) {
		t.Errorf("%s: missing %q in %q", label, needle, haystack)
	}
}

// -- C-7-LT-6 per-field path allow-list tests ------------------------
//
// Smoke-r16 caught C-7-LT-4's "all paths under task_dir" invariant
// rejecting the legitimate per-sandbox persistent workspace.img path
// at /var/zeroship/ch/<sandbox_id>/workspace.img. C-7-LT-6 replaces
// the single invariant with a per-field allow-list:
//
//   - PathFieldRuntimeFile (serial.file, console.file): under task_dir
//   - PathFieldDisk (disks[*].path): under per-sandbox prefix OR
//     task_dir OR caller-supplied content-addressed root
//   - PathFieldFsSocket (fs[*].socket): under task_dir
//
// Each test below pins one branch of the new allow-list.

// TestValidatePath_DiskUnderSandboxPrefix is the C-7-LT-6 happy path:
// a disk under /var/zeroship/ch/<sbx>/ is accepted with the per-sandbox
// allow-list active. Pin from smoke-r16's verbatim failure mode.
func TestValidatePath_DiskUnderSandboxPrefix(t *testing.T) {
	const sandboxID = "019e5979e2cc77c0934ca3afe37b06a4"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	diskPath := "/var/zeroship/ch/" + sandboxID + "/workspace.img"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[1].path", diskPath, taskDir, sandboxID, "", nil)
	if err != nil {
		t.Fatalf("disk under per-sandbox prefix rejected: %v", err)
	}
}

// TestValidatePath_DiskUnderTaskDir confirms the task_dir branch of
// the disk allow-list still works (e.g. a cold-boot-staged rootfs.img
// the driver materialises into the alloc dir).
func TestValidatePath_DiskUnderTaskDir(t *testing.T) {
	const sandboxID = "sbx_test"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	diskPath := taskDir + "/rootfs.img"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[0].path", diskPath, taskDir, sandboxID, "", nil)
	if err != nil {
		t.Fatalf("disk under task_dir rejected: %v", err)
	}
}

// TestValidatePath_DiskUnderContentAddressedRoot confirms the
// content-addressed allow-list slot: a read-only base rootfs image
// under an operator-configured root is accepted.
func TestValidatePath_DiskUnderContentAddressedRoot(t *testing.T) {
	const sandboxID = "sbx_test"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	diskPath := "/var/zeroship/ch/rootfs/sha256-abc123/rootfs.img"

	err := ch.ValidatePathByKind(
		ch.PathFieldDiskForTest,
		"disks[0].path",
		diskPath, taskDir, sandboxID, "",
		[]string{"/var/zeroship/ch/rootfs"},
	)
	if err != nil {
		t.Fatalf("disk under content-addressed root rejected: %v", err)
	}
}

// TestValidatePath_DiskRejectsRandomAbsolute is the defence-in-depth
// pin: an absolute path that matches none of the allow-list prefixes
// (e.g. /etc/shadow) is REJECTED. The malicious-snapshot scenario.
func TestValidatePath_DiskRejectsRandomAbsolute(t *testing.T) {
	const sandboxID = "019e5979e2cc77c0934ca3afe37b06a4"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[0].path", "/etc/shadow", taskDir, sandboxID, "", []string{"/var/zeroship/ch/rootfs"})
	if err == nil {
		t.Fatal("/etc/shadow accepted as disk path; allow-list bypassed")
	}
	mustContainTest(t, "random-absolute reject", err.Error(), "/etc/shadow")
	mustContainTest(t, "random-absolute reject", err.Error(), "disks[0].path")
}

// TestValidatePath_DiskRejectsWrongSandbox is the per-tenant isolation
// pin: a disk path under ANOTHER sandbox's prefix is REJECTED. The
// per-sandbox allow-list keys on the CURRENT alloc's sandbox_id, not
// any sandbox.
func TestValidatePath_DiskRejectsWrongSandbox(t *testing.T) {
	const ourSandbox = "019e5979e2cc77c0934ca3afe37b06a4"
	const otherSandbox = "022ffffff2cc77c0934ca3afe37bDEAD"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	otherPath := "/var/zeroship/ch/" + otherSandbox + "/workspace.img"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[1].path", otherPath, taskDir, ourSandbox, "", nil)
	if err == nil {
		t.Fatalf("disk under other sandbox %q accepted while current sandbox is %q (per-tenant isolation bypassed)", otherSandbox, ourSandbox)
	}
	mustContainTest(t, "wrong-sandbox reject", err.Error(), "disks[1].path")
	mustContainTest(t, "wrong-sandbox reject", err.Error(), otherSandbox)
}

// TestValidatePath_SerialFileMustBeUnderTaskDir is the runtime-file
// kind pin: serial.file (and console.file) MUST be under task_dir
// even if it textually matches a per-sandbox prefix. The runtime kind
// is alloc-scoped — a serial.log under /var/zeroship/ch/<sbx>/ is a
// red flag (the snapshot's recorded serial path should always have
// been under the OLD alloc's task_dir, which the rewriter then
// rewrites to the NEW alloc's task_dir).
func TestValidatePath_SerialFileMustBeUnderTaskDir(t *testing.T) {
	const sandboxID = "019e5979e2cc77c0934ca3afe37b06a4"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	weird := "/var/zeroship/ch/" + sandboxID + "/serial.log"

	err := ch.ValidatePathByKind(ch.PathFieldRuntimeFileForTest, "serial.file", weird, taskDir, sandboxID, "", nil)
	if err == nil {
		t.Fatal("serial.file under per-sandbox prefix accepted; runtime-file kind must enforce task_dir only")
	}
	mustContainTest(t, "serial.file-prefix reject", err.Error(), "serial.file")
	mustContainTest(t, "serial.file-prefix reject", err.Error(), "task_dir")
}

// TestValidatePath_ParentTraversalRejected confirms `..` components
// are caught for the disk kind too — even when the prefix textually
// looks like a per-sandbox path, `..` is a red flag we reject before
// the containment check runs.
func TestValidatePath_ParentTraversalRejected(t *testing.T) {
	const sandboxID = "019e5979e2cc77c0934ca3afe37b06a4"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	traversal := "/var/zeroship/ch/" + sandboxID + "/../" + "escape"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[0].path", traversal, taskDir, sandboxID, "", nil)
	if err == nil {
		t.Fatal("path with `..` component accepted; traversal defence bypassed")
	}
	mustContainTest(t, "traversal reject", err.Error(), "..")
}

// TestRewriteRestoreConfigPaths_PreservesPersistentWorkspace is the
// full integration witness for C-7-LT-6: a snapshot config.json whose
// disks[].path mixes (a) the OLD alloc's task_dir paths (must rewrite)
// and (b) the per-sandbox persistent workspace.img (must PRESERVE
// verbatim) rewrites cleanly. The serial.file (alloc-scoped) gets
// rewritten to the NEW task_dir.
func TestRewriteRestoreConfigPaths_PreservesPersistentWorkspace(t *testing.T) {
	const sandboxID = "019e5979e2cc77c0934ca3afe37b06a4"
	srcAlloc := "/opt/nomad/data/alloc/AAAA-source/task/local"
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	persistentWorkspace := "/var/zeroship/ch/" + sandboxID + "/workspace.img"
	persistentUserHome := "/var/zeroship/ch/" + sandboxID + "/userhome.img"

	doc := map[string]any{
		"disks": []any{
			// disk[0]: staged rootfs under OLD alloc's task_dir →
			// rewriter substitutes to NEW task_dir.
			map[string]any{"path": srcAlloc + "/rootfs.img"},
			// disk[1]: per-sandbox persistent workspace → MUST be
			// preserved verbatim (the smoke-r16 failure mode).
			map[string]any{"path": persistentWorkspace},
			// disk[2]: per-sandbox persistent userhome → likewise.
			map[string]any{"path": persistentUserHome},
		},
		"serial": map[string]any{"file": srcAlloc + "/serial.log"},
	}
	in, err := json.Marshal(doc)
	if err != nil {
		t.Fatalf("marshal seed: %v", err)
	}

	out, err := ch.RewriteConfigJSON(in, newTaskDir, 1, 99, sandboxID, "", nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	disks := got["disks"].([]any)
	if d0 := disks[0].(map[string]any); d0["path"] != newTaskDir+"/rootfs.img" {
		t.Errorf("disks[0].path = %v, want %v (rewritten to new task_dir)", d0["path"], newTaskDir+"/rootfs.img")
	}
	if d1 := disks[1].(map[string]any); d1["path"] != persistentWorkspace {
		t.Errorf("disks[1].path = %v, want %v (persistent path preserved)", d1["path"], persistentWorkspace)
	}
	if d2 := disks[2].(map[string]any); d2["path"] != persistentUserHome {
		t.Errorf("disks[2].path = %v, want %v (persistent path preserved)", d2["path"], persistentUserHome)
	}
	if s := got["serial"].(map[string]any); s["file"] != newTaskDir+"/serial.log" {
		t.Errorf("serial.file = %v, want %v (rewritten to new task_dir)", s["file"], newTaskDir+"/serial.log")
	}
}

// TestRewriteRestoreConfigPaths_DifferentiatesDiskFromSerial confirms
// the per-field allow-list dispatch: a disk under /var/zeroship/ch/
// passes (PathFieldDisk allow-list), while a serial.file under the
// same prefix is REJECTED (PathFieldRuntimeFile is task_dir only).
// Both fields share the same config; the rewriter must apply the
// right kind to each.
func TestRewriteRestoreConfigPaths_DifferentiatesDiskFromSerial(t *testing.T) {
	const sandboxID = "019e5979e2cc77c0934ca3afe37b06a4"
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	persistentDisk := "/var/zeroship/ch/" + sandboxID + "/workspace.img"

	// Case 1: disk under per-sandbox prefix + serial under task_dir →
	// both pass.
	docOK := map[string]any{
		"disks":  []any{map[string]any{"path": persistentDisk}},
		"serial": map[string]any{"file": newTaskDir + "/serial.log"},
	}
	in, err := json.Marshal(docOK)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	out, err := ch.RewriteConfigJSON(in, newTaskDir, 1, 99, sandboxID, "", nil)
	if err != nil {
		t.Fatalf("disk-under-prefix + serial-under-task_dir should pass: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}
	if d := got["disks"].([]any)[0].(map[string]any); d["path"] != persistentDisk {
		t.Errorf("disks[0].path = %v, want %v", d["path"], persistentDisk)
	}

	// Case 2: serial under per-sandbox prefix → REJECTED (runtime
	// files must be alloc-scoped).
	docBad := map[string]any{
		"disks":  []any{map[string]any{"path": persistentDisk}},
		"serial": map[string]any{"file": "/var/zeroship/ch/" + sandboxID + "/serial.log"},
	}
	in, err = json.Marshal(docBad)
	if err != nil {
		t.Fatalf("marshal: %v", err)
	}
	_, err = ch.RewriteConfigJSON(in, newTaskDir, 1, 99, sandboxID, "", nil)
	if err == nil {
		t.Fatal("serial.file under per-sandbox prefix accepted; runtime-file kind not enforced")
	}
	mustContainTest(t, "differentiation reject", err.Error(), "serial.file")
}

// -- C-7-LT-7 per-user-home prefix tests -----------------------------
//
// Smoke-r17 caught C-7-LT-6's per-sandbox allow-list rejecting the
// legitimate per-user persistent `home.img` at
// /var/zeroship/ch/users/<user_id>/home.img — one home shared across
// every sandbox a user owns, so it lives OUTSIDE the per-sandbox
// prefix by design. C-7-LT-7 extends the allow-list with a fourth
// entry keyed on the current alloc's user_id.

// TestValidatePath_UserHomePrefix_OK is the C-7-LT-7 happy path: a
// disk under /var/zeroship/ch/users/<usr>/ is accepted when the
// per-user allow-list is active. Pin from smoke-r17's verbatim
// failure mode.
func TestValidatePath_UserHomePrefix_OK(t *testing.T) {
	const userID = "usr_033MDp768TZVdYSaq2M14o"
	const sandboxID = "019e5993440570939fcc8fbfde705022"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	diskPath := "/var/zeroship/ch/users/" + userID + "/home.img"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[2].path", diskPath, taskDir, sandboxID, userID, nil)
	if err != nil {
		t.Fatalf("disk under per-user home prefix rejected: %v", err)
	}
}

// TestValidatePath_UserHomePrefix_CrossUser_Rejected is the per-user
// isolation pin: a disk path under ANOTHER user's home prefix is
// REJECTED. The per-user allow-list keys on the CURRENT alloc's
// user_id, not any user — mirrors the sandbox-isolation property of
// C-7-LT-6's per-sandbox check.
func TestValidatePath_UserHomePrefix_CrossUser_Rejected(t *testing.T) {
	const ourUser = "usr_033MDp768TZVdYSaq2M14o"
	const otherUser = "usr_DEADBEEF8TZVdYSaq2M14o"
	const sandboxID = "019e5993440570939fcc8fbfde705022"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	otherPath := "/var/zeroship/ch/users/" + otherUser + "/home.img"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[2].path", otherPath, taskDir, sandboxID, ourUser, nil)
	if err == nil {
		t.Fatalf("disk under other user %q accepted while current user is %q (per-tenant isolation bypassed)", otherUser, ourUser)
	}
	mustContainTest(t, "cross-user reject", err.Error(), "disks[2].path")
	mustContainTest(t, "cross-user reject", err.Error(), otherUser)
}

// TestValidatePath_UserHomePrefix_EmptyUserId_Rejected guards against
// misconfig: when user_id is empty (i.e. the controller failed to
// emit it or the driver upgraded against an older controller), a
// disk under a user-home-shaped path MUST NOT be silently accepted.
// Empty user_id disables the per-user slot entirely.
func TestValidatePath_UserHomePrefix_EmptyUserId_Rejected(t *testing.T) {
	const sandboxID = "019e5993440570939fcc8fbfde705022"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	diskPath := "/var/zeroship/ch/users/usr_033MDp768TZVdYSaq2M14o/home.img"

	err := ch.ValidatePathByKind(ch.PathFieldDiskForTest, "disks[2].path", diskPath, taskDir, sandboxID, "", nil)
	if err == nil {
		t.Fatal("disk under per-user prefix accepted with empty user_id; misconfig defence bypassed")
	}
	mustContainTest(t, "empty-userid reject", err.Error(), "disks[2].path")
	// The error message MUST NOT mention a user-home prefix when
	// user_id was disabled — otherwise an operator would chase a
	// phantom mismatch instead of fixing the missing user_id.
	if errMsg := err.Error(); strings.Contains(errMsg, "user=") {
		t.Errorf("error message references user= prefix while user_id was empty: %q", errMsg)
	}
}

// TestValidatePath_UserHomePrefix_NotADisk_RejectsAtRuntimeKind is the
// kind-dispatch pin: a path that LOOKS like a per-user-home path but
// is being validated as a runtime file (serial.file) MUST be REJECTED
// — the per-user-home allow-list applies ONLY to disks, not to
// runtime files. Mirrors TestValidatePath_SerialFileMustBeUnderTaskDir
// for the per-sandbox prefix.
func TestValidatePath_UserHomePrefix_NotADisk_RejectsAtRuntimeKind(t *testing.T) {
	const userID = "usr_033MDp768TZVdYSaq2M14o"
	const sandboxID = "019e5993440570939fcc8fbfde705022"
	const taskDir = "/opt/nomad/data/alloc/AAAA/ch/local"
	weird := "/var/zeroship/ch/users/" + userID + "/serial.log"

	err := ch.ValidatePathByKind(ch.PathFieldRuntimeFileForTest, "serial.file", weird, taskDir, sandboxID, userID, nil)
	if err == nil {
		t.Fatal("serial.file under per-user home prefix accepted; runtime-file kind must enforce task_dir only")
	}
	mustContainTest(t, "serial-under-user-home reject", err.Error(), "serial.file")
	mustContainTest(t, "serial-under-user-home reject", err.Error(), "task_dir")
}

// TestRewriteRestoreConfigPaths_AcceptsUserHomeAndSandboxAndTaskDir is
// the full integration witness for C-7-LT-7: a snapshot config.json
// with disks across all three layout-stable namespaces (rootfs under
// OLD task_dir → rewritten to NEW; workspace under per-sandbox prefix
// → preserved; home under per-user prefix → preserved) rewrites
// cleanly. This is the byte-for-byte shape smoke-r17 surfaced from
// production.
func TestRewriteRestoreConfigPaths_AcceptsUserHomeAndSandboxAndTaskDir(t *testing.T) {
	const sandboxID = "019e5993440570939fcc8fbfde705022"
	const userID = "usr_033MDp768TZVdYSaq2M14o"
	srcAlloc := "/opt/nomad/data/alloc/AAAA-source/task/local"
	newTaskDir := "/opt/nomad/data/alloc/BBBB-new/task/local"
	persistentWorkspace := "/var/zeroship/ch/" + sandboxID + "/workspace.img"
	persistentUserHome := "/var/zeroship/ch/users/" + userID + "/home.img"

	doc := map[string]any{
		"disks": []any{
			// disks[0]: staged rootfs under OLD alloc's task_dir →
			// rewriter substitutes to NEW task_dir.
			map[string]any{"path": srcAlloc + "/rootfs.img"},
			// disks[1]: per-sandbox persistent workspace (the C-7-LT-6
			// case) → MUST be preserved verbatim.
			map[string]any{"path": persistentWorkspace},
			// disks[2]: per-user persistent home (the C-7-LT-7 case,
			// smoke-r17 failure mode) → MUST be preserved verbatim.
			map[string]any{"path": persistentUserHome},
		},
		"serial": map[string]any{"file": srcAlloc + "/serial.log"},
	}
	in, err := json.Marshal(doc)
	if err != nil {
		t.Fatalf("marshal seed: %v", err)
	}

	out, err := ch.RewriteConfigJSON(in, newTaskDir, 1, 99, sandboxID, userID, nil)
	if err != nil {
		t.Fatalf("RewriteConfigJSON: %v", err)
	}
	var got map[string]any
	if err := json.Unmarshal(out, &got); err != nil {
		t.Fatalf("unmarshal: %v", err)
	}

	disks := got["disks"].([]any)
	if d0 := disks[0].(map[string]any); d0["path"] != newTaskDir+"/rootfs.img" {
		t.Errorf("disks[0].path = %v, want %v (rewritten to new task_dir)", d0["path"], newTaskDir+"/rootfs.img")
	}
	if d1 := disks[1].(map[string]any); d1["path"] != persistentWorkspace {
		t.Errorf("disks[1].path = %v, want %v (per-sandbox persistent path preserved)", d1["path"], persistentWorkspace)
	}
	if d2 := disks[2].(map[string]any); d2["path"] != persistentUserHome {
		t.Errorf("disks[2].path = %v, want %v (per-user persistent path preserved)", d2["path"], persistentUserHome)
	}
	if s := got["serial"].(map[string]any); s["file"] != newTaskDir+"/serial.log" {
		t.Errorf("serial.file = %v, want %v (rewritten to new task_dir)", s["file"], newTaskDir+"/serial.log")
	}
}

// TestUserHomePrefixForTest pins the layout convention so a future
// rename of the per-user-home root (e.g. `users/` → `homes/`) surfaces
// here at compile/test time rather than at the next cluster smoke.
// C-7-LT-7.
func TestUserHomePrefixForTest(t *testing.T) {
	const userID = "usr_033MDp768TZVdYSaq2M14o"
	want := "/var/zeroship/ch/users/" + userID + "/"
	if got := ch.UserHomePrefixForTest(userID); got != want {
		t.Errorf("UserHomePrefixForTest(%q) = %q, want %q", userID, got, want)
	}
	if got := ch.UserHomePrefixForTest(""); got != "" {
		t.Errorf("UserHomePrefixForTest(\"\") = %q, want empty (signals per-user slot disabled)", got)
	}
}

