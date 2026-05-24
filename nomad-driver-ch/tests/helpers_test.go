// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Test helpers for the tests/ package. Builds a *drivers.TaskConfig
// pointed at a t.TempDir() and msgpack-encoded with the ch.TaskConfig
// payload so StartTask's DecodeDriverConfig round-trips correctly.

package tests

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/hashicorp/nomad/plugins/drivers"

	"github.com/zeroship/nomad-driver-ch/ch"
)

// driversTaskConfig is an alias used by start_task_test.go so the helper
// signature reads naturally.
type driversTaskConfig = drivers.TaskConfig

// driversTaskResourceUsage is an alias used by task_stats_test.go to
// keep the test code readable without re-importing drivers everywhere.
type driversTaskResourceUsage = drivers.TaskResourceUsage

// driversErrTaskNotFound re-exports the sentinel TaskStats /
// InspectTask consumers compare against; aliased here so test code
// reads naturally.
var driversErrTaskNotFound = drivers.ErrTaskNotFound

// newDriversTaskConfig builds a *drivers.TaskConfig whose TaskDir()
// resolves under the supplied taskDir. The driver-config payload is
// msgpack-encoded so cfg.DecodeDriverConfig recovers a ch.TaskConfig
// identical to driverCfg.
//
// Nomad computes TaskDir() as filepath.Join(AllocDir, Name); we set
// AllocDir to the parent of taskDir and Name to its basename so
// TaskDir().Dir == taskDir. The driver's taskRunDir picks LocalDir =
// taskDir/local; we don't pre-create that — StartTask MkdirAlls it.
//
// C-2: this helper also stages the on-disk surface StartTask's
// pre-flight expects:
//
//   - WorkspaceImg / UserHomeImg / each Disks[].Path are populated
//     with a single-byte file at the path the test config references
//     (the pre-flight stat is content-blind; non-empty + non-directory
//     passes).
//   - A synthetic artifact dir is provisioned with a stub
//     `rootfs-slim.img`, and the ZSBX_ARTIFACT_DIR env var is set on
//     the TaskConfig.Env map so StartTask's rootfs-materialise step
//     finds it.
//
// Tests that need to exercise the pre-flight failure path (e.g.
// "what happens when workspace_img is missing") can override by
// calling os.Remove on the staged path after the helper returns, or
// by passing a TaskConfig that points at a deliberately-missing path
// — the helper only stages paths it can derive from the supplied
// TaskConfig.
func newDriversTaskConfig(t *testing.T, driverCfg *ch.TaskConfig, taskDir string) *drivers.TaskConfig {
	t.Helper()
	allocDir := filepath.Dir(taskDir)
	name := filepath.Base(taskDir)

	// C-2: rewrite the synthesised-disk WorkspaceImg / UserHomeImg
	// paths under root-owned prefixes (/var/...) to per-test scratch
	// paths under a fresh t.TempDir(). Driver-explicit Disks lists
	// (set by the test) are LEFT ALONE — that's the operator-supplied
	// override surface and tests use it to exercise negative
	// pre-flight paths.
	//
	// Pre-flight stat requires every disk to exist on disk; routing
	// through t.TempDir() keeps the staging hermetic (parallel test
	// runs don't race on the same path; the t.Cleanup-rooted dir
	// vanishes when the test ends).
	scratch := t.TempDir()
	if driverCfg.WorkspaceImg != "" {
		driverCfg.WorkspaceImg = relocatedScratchPath(scratch, driverCfg.WorkspaceImg, "workspace.img")
	}
	if driverCfg.UserHomeImg != "" {
		driverCfg.UserHomeImg = relocatedScratchPath(scratch, driverCfg.UserHomeImg, "userhome.img")
	}

	// Stage disk-image stubs at the synthesised disk paths. The
	// helper does NOT auto-stage driverCfg.Disks entries — explicit
	// disks are the negative-test surface and we must let the test
	// control file presence/absence/size there.
	stageDiskFile(t, driverCfg.WorkspaceImg)
	stageDiskFile(t, driverCfg.UserHomeImg)

	// Stage a synthetic artifact dir with the rootfs-slim.img stub
	// the driver's materialiseRootfs helper copies from. Returned via
	// Env (ZSBX_ARTIFACT_DIR) so the driver consults the same path the
	// controller would emit under ChPlugin mode.
	artifactDir := stageArtifactDir(t)

	// C-7-LT-12a: on the restore branch the driver hardlinks (or
	// copies on EXDEV) `RootfsSource` into runDir/rootfs.img. Auto-
	// populate the field with the staged artifact dir's stub so
	// existing restore-branch tests don't have to thread the value
	// explicitly. Tests that exercise the missing/empty-source
	// negative paths can pass a non-empty RootfsSource themselves
	// (e.g. point at "/tmp/missing-on-purpose.img") which we leave
	// alone here.
	if driverCfg.RestoreFrom != "" && driverCfg.RootfsSource == "" {
		driverCfg.RootfsSource = filepath.Join(artifactDir, ch.ChRootfsSourceName)
	}

	cfg := &drivers.TaskConfig{
		ID:       "test-task-" + name,
		Name:     name,
		AllocDir: allocDir,
		Env: map[string]string{
			ch.ChArtifactDirEnvVar: artifactDir,
		},
	}
	if err := cfg.EncodeConcreteDriverConfig(driverCfg); err != nil {
		t.Fatalf("EncodeConcreteDriverConfig: %v", err)
	}
	return cfg
}

// relocatedScratchPath maps a production-shaped absolute disk path
// (e.g. "/var/lib/zsbx/img/workspace.img") to a per-test scratch
// path under t.TempDir() (e.g. "<scratch>/workspace.img"). The
// production prefix is irrelevant to the driver's pre-flight stat
// check — it just needs the file to exist + be non-empty + non-dir.
//
// Paths that already live under /tmp are passed through verbatim so
// a test that deliberately uses a specific path (e.g. negative
// pre-flight tests pointing at "/tmp/missing-on-purpose.img") still
// hits its intended target.
func relocatedScratchPath(scratch, orig, fallbackBase string) string {
	if filepath.IsAbs(orig) && len(orig) >= 4 && orig[:4] == "/tmp" {
		return orig
	}
	base := filepath.Base(orig)
	if base == "" || base == "." || base == "/" {
		base = fallbackBase
	}
	return filepath.Join(scratch, base)
}

// stageDiskFile creates a 1-byte file at path (with any required
// parent dirs) if path is non-empty and the file doesn't already
// exist. Tests use this to satisfy the C-2 pre-flight stat check
// against test-controlled WorkspaceImg / UserHomeImg / Disks[].Path
// values like "/tmp/ws.img" that wouldn't otherwise exist on the
// test host.
//
// Idempotent: a path that already exists is left alone (so a test
// that pre-stages a specific size or content isn't clobbered).
// Skips empty path (the driver's pre-flight surfaces empty-path as
// its own error; that's what we want to exercise in some negative
// tests).
func stageDiskFile(t *testing.T, path string) {
	t.Helper()
	if path == "" {
		return
	}
	if _, err := os.Stat(path); err == nil {
		return
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatalf("stageDiskFile: mkdir %s: %v", filepath.Dir(path), err)
	}
	if err := os.WriteFile(path, []byte("x"), 0o600); err != nil {
		t.Fatalf("stageDiskFile: write %s: %v", path, err)
	}
	t.Cleanup(func() { _ = os.Remove(path) })
}

// stageArtifactDir provisions a t.TempDir() with a stub
// rootfs-slim.img inside. Returns the dir path. The C-2 fix copies
// $artifactDir/rootfs-slim.img into the run dir's rootfs.img on
// every cold-boot StartTask; without this stage the synthesised-disk
// path errors out before reaching the spawn step.
func stageArtifactDir(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	src := filepath.Join(dir, ch.ChRootfsSourceName)
	if err := os.WriteFile(src, []byte("stub-rootfs"), 0o600); err != nil {
		t.Fatalf("stageArtifactDir: write %s: %v", src, err)
	}
	return dir
}
