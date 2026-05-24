// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// Option C Phase 2 (2026-05-25 staging-locality ADR): driver-side disk
// staging. When TaskConfig.StageDiskImages=true, StartTask invokes
// stageDiskImages BEFORE spawning Cloud Hypervisor. The op creates the
// sparse image, runs mkfs.ext4 -q -F, then fsyncs file + parent dir so
// the dirent is durable + visible to peer processes (notably CH itself,
// which opens the path with O_DIRECT-class semantics under virtio-blk).
//
// This file is the driver-side mirror of the controller's
// `create_ext4_image_if_missing` in
// `crates/sandbox/src/backend/nomad_ch.rs:3913` — the function it
// replaces under Option C. We do NOT call out to that controller; the
// driver re-implements the truncate + mkfs.ext4 + fsync sequence
// locally so the staging happens on the SAME node the alloc lands on
// (collapsing the cross-alloc kernel-state retention surface the
// layer-peel rounds 1-5 chased — see ADR Context table).

package ch

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
)

// stageImageOp is the swappable seam tests substitute to drive the
// failure paths (mkfs.ext4 failure, truncate failure) without engineering
// a real ENOSPC / missing-binary scenario in t.TempDir().
//
// Production default: stageImageOpDefault below. Tests override via
// SetStageImageOpForTest in export_test_api.go.
//
// Contract: given an absolute path + a size in bytes, materialize a
// sparse ext4 image at the path. Idempotent on re-invocation (a path
// that already exists with non-zero size is left alone — mirrors the
// controller's `create_ext4_image_if_missing` idempotency contract).
// Returns the first error encountered or nil on success.
var stageImageOp = stageImageOpDefault

// stageImageOpDefault is the production implementation. truncate(1) +
// mkfs.ext4(8) via os/exec, mirroring the controller's
// `create_ext4_image_if_missing`. Both binaries are universally present
// on the cluster's Nomad worker image (debian + linux-image-cloud);
// missing-binary surfaces as `exec.LookPath` / `exec.ErrNotFound` from
// exec.Command which we propagate verbatim with the binary name in the
// error so triage is one-step.
//
// fsync semantics:
//   - File-level fsync via O_SYNC | O_WRONLY on the truncated file, so
//     the metadata write (the sparse-file size) is durable.
//   - Parent-dir fsync via opening the dir + sync_all, so the new
//     dirent is visible to peer processes (CH spawns shortly after and
//     opens the disk for virtio-blk). The fsync_dir step is the same
//     mitigation the controller v33 fix landed for T-8b-stress Bug 1
//     (49/60 CREATEs hit "workspace.img does not exist" because the
//     dirent commit lagged the controller's submit).
func stageImageOpDefault(path string, sizeBytes int64) error {
	// Idempotent: a non-empty file at the path is left alone. Mirrors
	// the controller's `create_ext4_image_if_missing` skip-on-exists
	// branch. Zero-byte files are treated as a half-staged failure
	// and re-staged.
	if info, err := os.Stat(path); err == nil {
		if info.IsDir() {
			return fmt.Errorf("stage %s: path is a directory (expected file)", path)
		}
		if info.Size() > 0 {
			return nil
		}
		// Half-staged: zero-byte file. Remove it and re-stage. The
		// controller's idempotency contract would re-mkfs.ext4 the
		// truncated file in-place; we delete first to avoid the
		// "mkfs.ext4 sees a non-zero file" prompt path even though -F
		// suppresses it.
		if err := os.Remove(path); err != nil {
			return fmt.Errorf("stage %s: remove zero-byte prior: %w", path, err)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("stage %s: stat: %w", path, err)
	}

	// truncate(1) sparse-creates the file. We use the CLI (not
	// File.Truncate) to keep the failure mode bit-for-bit identical
	// to the controller's prior implementation — an operator reading
	// "truncate -s 20G /path failed" can reproduce it by hand.
	size := fmt.Sprintf("%d", sizeBytes)
	truncCmd := exec.Command("truncate", "-s", size, path)
	if out, err := truncCmd.CombinedOutput(); err != nil {
		return fmt.Errorf("stage %s: truncate -s %s: %w (output=%q)", path, size, err, string(out))
	}

	// mkfs.ext4 -q -F: -q silences the verbose progress output, -F
	// forces formatting of a regular file (without -F, mkfs prompts
	// and aborts when the target isn't a block device).
	mkfsCmd := exec.Command("mkfs.ext4", "-q", "-F", path)
	if out, err := mkfsCmd.CombinedOutput(); err != nil {
		// Clean up the half-created sparse file so a retry's
		// idempotency check doesn't see a non-zero unformatted file
		// and skip the mkfs.
		_ = os.Remove(path)
		return fmt.Errorf("stage %s: mkfs.ext4 -q -F: %w (output=%q)", path, err, string(out))
	}

	// Fsync the file's metadata (the sparse-file size) so a crash
	// between mkfs and the dirent-fsync below doesn't leave an
	// unformatted truncated file.
	if f, err := os.OpenFile(path, os.O_WRONLY, 0); err == nil {
		_ = f.Sync()
		_ = f.Close()
	}

	// Fsync the parent dir so the dirent is durable AND visible to
	// peer processes statting the path before the kernel's lazy
	// dirent commit. Best-effort: a failure here surfaces as a
	// staging error since a non-durable dirent is the exact failure
	// mode T-8b-stress Bug 1 produced.
	parent := filepath.Dir(path)
	if dirf, err := os.Open(parent); err == nil {
		if syncErr := dirf.Sync(); syncErr != nil {
			_ = dirf.Close()
			return fmt.Errorf("stage %s: fsync parent %s: %w", path, parent, syncErr)
		}
		if err := dirf.Close(); err != nil {
			return fmt.Errorf("stage %s: close parent %s: %w", path, parent, err)
		}
	} else {
		return fmt.Errorf("stage %s: open parent %s for fsync: %w", path, parent, err)
	}
	return nil
}

// stageDiskImages materializes the per-alloc disk images the cold-boot
// path needs BEFORE Cloud Hypervisor spawns. Option C Phase 2 entry
// point: invoked from StartTask when TaskConfig.StageDiskImages=true.
//
// What this stages today:
//
//   - `WorkspaceImg` — per-sandbox ext4 image (raw, attached as
//     virtio-blk → guest /dev/vdb → /workspace). Freshly created on
//     every cold-boot of the sandbox. Idempotent re-stage on retry.
//
//   - `UserHomeImg` — per-user ext4 image (raw, attached as virtio-blk
//     → guest /dev/vdc → /userhome). Created once on the user's first
//     sandbox; reused across subsequent sandboxes (so the idempotent
//     skip-on-exists branch is the common case after the first cold-
//     boot for that user).
//
// Size: a fixed 20 GiB sparse allocation, matching the controller's
// `workspace_image_size_gb` default (the pre-Phase-2 controller used
// the same value for both images per the virtio-blk pivot in
// `crates/sandbox/src/config.rs:186`). Phase 4 may parameterise this
// via a new TaskConfig field if cluster validation surfaces a need to
// vary; for now we pin to the same constant the controller used.
//
// What this does NOT stage:
//
//   - `rootfs.img` — the cold-boot rootfs lives at `runDir/rootfs.img`
//     and is materialised by `materializeRootfs` (the cp from
//     `$ZSBX_ARTIFACT_DIR/rootfs-slim.img` the wrapper does at
//     `nomad-vm-wrapper.sh:300-305`). That stays on its existing
//     code path because it's a copy from a host-fs source, not a
//     fresh truncate+mkfs.ext4 — different staging semantics.
//
//   - Restore-branch disks — under restore, CH consumes the snapshot's
//     own disk paths (rewritten by the T-6 path-rewriter); no fresh
//     image staging happens. The StageDiskImages flag has no effect
//     on the restore branch (caller already short-circuits earlier).
//
// Error contract: returns the first error encountered. On error, the
// CH spawn is NOT attempted; Nomad marks the task failed and the
// controller-side CreateGuard rolls back its state (per ADR Phase 2:
// "the driver returns the error WITHOUT spawning CH; CreateGuard
// rollback contract preserved").
//
// Side effects: creates the parent dirs of each image path with mkdir
// -p (0o755). The controller used to do this on its local fs; under
// Option C the driver does it on the alloc's runtime fs.
//
// Counters: bumps `nomad_driver_ch_start_task_stage_total` on every
// invocation, plus `nomad_driver_ch_start_task_stage_failures_total`
// on first error. Caller-driven; this function does not check the
// flag (caller has already done so).
func stageDiskImages(taskConfig *TaskConfig) error {
	incStartTaskStage()

	// 20 GiB matches the controller's workspace_image_size_gb default
	// (crates/sandbox/src/config.rs:186 "default 20"). Captured here as
	// a const so a future TaskConfig field expansion has one anchor to
	// search-and-rename.
	const stageImageSizeBytes int64 = 20 * 1024 * 1024 * 1024

	images := []struct {
		role string
		path string
	}{
		{role: "workspace_img", path: taskConfig.WorkspaceImg},
		{role: "user_home_img", path: taskConfig.UserHomeImg},
	}

	for _, img := range images {
		if img.path == "" {
			incStartTaskStageFailures()
			return fmt.Errorf("ch: stageDiskImages: %s is empty (StageDiskImages=true requires both image paths)", img.role)
		}
		parent := filepath.Dir(img.path)
		if err := os.MkdirAll(parent, 0o755); err != nil {
			incStartTaskStageFailures()
			return fmt.Errorf("ch: stageDiskImages: mkdir parent of %s (%s): %w", img.role, parent, err)
		}
		if err := stageImageOp(img.path, stageImageSizeBytes); err != nil {
			incStartTaskStageFailures()
			return fmt.Errorf("ch: stageDiskImages: %s: %w", img.role, err)
		}
	}
	return nil
}
