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

	out, err := ch.RewriteConfigJSON(input, newTaskDir, 7, 99)
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

	out, err := ch.RewriteConfigJSON(input, newTaskDir, newVMIndex, 99)
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
	out, err := ch.RewriteConfigJSON(seeded, newTaskDir, 7, 99)
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
			_, err := ch.RewriteConfigJSON(tc.in, "/opt/nomad/data/alloc/x/y/local", 1, 99)
			if err == nil {
				t.Fatalf("expected error for %s", tc.name)
			}
		})
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

