// Copyright (c) HashiCorp, Inc.
// SPDX-License-Identifier: MPL-2.0
//
// T-8b-stress-r2 driver v14: lightweight process-global counters for
// teardown-path observability. Mirrors the controller-side pattern in
// `crates/sandbox/src/metrics.rs` — atomic counters today, swappable
// for a real Prometheus registry later. No external deps.
//
// Today's surface:
//
//   - `nomad_driver_ch_taps_orphaned_total` — bumped when DestroyTask's
//     defensive cleanup branch removes a tap that the handle didn't
//     record (i.e., a leftover from a prior alloc that StartTask's tap-
//     setup half-completed for, or a tap that survived a prior crash).
//     Operators rate-graph this; a healthy fleet trends to zero.
//   - `nomad_driver_ch_destroy_task_unreaped_total` — bumped when
//     DestroyTask's bounded reap-wait exhausts without observing the
//     supervisor close h.exitDone (i.e. the CH process didn't get
//     reaped within the budget after SIGKILL). The kernel still holds
//     fcntl write locks on the zombie's rootfs.img until reap, so the
//     next alloc's CH `--restore` will hit `DiskLockError: AlreadyLocked`
//     until init/runner finally reaps. Operators rate-graph this; a
//     healthy fleet trends to zero. See T-8b-stress-r4 r4-A.
//   - `nomad_driver_ch_destroy_task_lock_held_total` — bumped when
//     DestroyTask's bounded OFD-lock-probe exhausts without successfully
//     acquiring the F_OFD_SETLK write lock on a disk path (i.e. the
//     kernel's `__fput` workqueue still holds the file open AFTER
//     `wait4()` reaped the CH process). r4-A's reap predicate is
//     necessary but not sufficient — the OFD write lock on `rootfs.img`
//     persists (attributed to PID=-1) until `__fput` completes, and the
//     next alloc's CH `--restore` then hits `DiskLockError →
//     AlreadyLocked`. Operators rate-graph this; a healthy fleet trends
//     to zero. See T-8b-stress-r5 r5-A.
//   - `nomad_driver_ch_start_task_stage_total` /
//     `nomad_driver_ch_start_task_stage_failures_total` — bumped on
//     every cold-boot StartTask where TaskConfig.StageDiskImages=true
//     (Option C Phase 2 driver-side staging). The pair lets operators
//     observe the staging surface engaging (total > 0 confirms the
//     controller emitted the flag) and the failure ratio (failures /
//     total). Healthy fleet: total bumps once per cold-boot, failures
//     trends to zero. See the 2026-05-25 staging-locality ADR.
//   - `nomad_driver_ch_destroy_task_tap_stuck_total` — bumped when
//     DestroyTask's synchronous tap-delete + ENODEV-verify gate
//     exhausts its poll budget without observing the kernel evict
//     the tap netdev. r24-A2-S2 closes the residual stress-r8
//     `Tap zsbx-nm-N already exists` window: the existing best-
//     effort tap removal (h.tap call + defensive VMIndex-keyed pass)
//     was non-blocking on failure, so DestroyTask could return to
//     Nomad with the tap still present in the kernel — the next
//     alloc on the same VMIndex then hit EEXIST. The new gate
//     verifies ENODEV via `ip link show` before returning. On
//     budget exhaustion: bump this counter, WARN-log, proceed
//     (mirrors r4-A's "never block Nomad destroy" tolerance).
//     Operators rate-graph this; a healthy fleet trends to zero.
//     See T-8b-stress-r8 r24-A2-S2.

package ch

import (
	"sync"
	"sync/atomic"
)

// tapsOrphanedTotal is the process-global counter behind
// `nomad_driver_ch_taps_orphaned_total`. Bumped on the defensive
// teardown path in DestroyTask when we delete a tap whose name we
// derived from VMIndex (not from h.tap). See `ch/stop_task.go::
// DestroyTask` for the call site.
var tapsOrphanedTotal atomic.Int64

// incTapsOrphaned bumps `nomad_driver_ch_taps_orphaned_total` by one.
// Goroutine-safe; the atomic Int64 carries its own ordering.
func incTapsOrphaned() {
	tapsOrphanedTotal.Add(1)
}

// TapsOrphanedTotal returns the current counter value. Exported for
// tests (asserts the defensive cleanup branch fires); a future
// `/metrics` exporter would also use this read path.
func TapsOrphanedTotal() int64 {
	return tapsOrphanedTotal.Load()
}

// ResetTapsOrphanedForTest zeroes the counter so a test can pin its
// own baseline without depending on sibling-test ordering. Hidden
// behind the `ForTest` suffix the rest of the driver uses for test-
// only seams.
func ResetTapsOrphanedForTest() {
	tapsOrphanedTotal.Store(0)
}

// destroyTaskUnreapedTotal is the process-global counter behind
// `nomad_driver_ch_destroy_task_unreaped_total`. Bumped when DestroyTask's
// bounded reap-wait exhausts without observing the supervisor close
// h.exitDone — i.e. the CH process didn't get reaped within the budget
// after SIGKILL. See `ch/stop_task.go::DestroyTask` (T-8b-stress-r4 r4-A).
var destroyTaskUnreapedTotal atomic.Int64

// incDestroyTaskUnreaped bumps `nomad_driver_ch_destroy_task_unreaped_total`
// by one. Goroutine-safe; the atomic Int64 carries its own ordering.
func incDestroyTaskUnreaped() {
	destroyTaskUnreapedTotal.Add(1)
}

// DestroyTaskUnreapedTotal returns the current counter value. Exported for
// tests (asserts the reap-wait budget-exhaustion branch fires); a future
// `/metrics` exporter would also use this read path.
func DestroyTaskUnreapedTotal() int64 {
	return destroyTaskUnreapedTotal.Load()
}

// ResetDestroyTaskUnreapedForTest zeroes the counter so a test can pin
// its own baseline without depending on sibling-test ordering.
func ResetDestroyTaskUnreapedForTest() {
	destroyTaskUnreapedTotal.Store(0)
}

// destroyTaskLockHeldTotal is the process-global counter behind
// `nomad_driver_ch_destroy_task_lock_held_total`. Bumped when DestroyTask's
// bounded OFD-lock-acquire probe exhausts without successfully acquiring
// the F_OFD_SETLK write lock on a disk path — i.e. the kernel `__fput`
// workqueue still holds the file open after `wait4()` reaped the CH
// process. See T-8b-stress-r5 r5-A: r4-A's reap predicate (Go's
// `cmd.Wait()` returning) is necessary but not sufficient; until the
// deferred `__fput` runs, the OFD write lock on `rootfs.img` persists
// (attributed to PID=-1) and the next alloc's CH `--restore` hits
// `DiskLockError → AlreadyLocked`.
//
// Operators rate-graph this; a healthy fleet trends to zero. A spike
// here means the kernel workqueue is backed up — orthogonal to the
// driver, but the operator-facing diagnostic is the metric + WARN line.
var destroyTaskLockHeldTotal atomic.Int64

// incDestroyTaskLockHeld bumps `nomad_driver_ch_destroy_task_lock_held_total`
// by one. Goroutine-safe; the atomic Int64 carries its own ordering.
func incDestroyTaskLockHeld() {
	destroyTaskLockHeldTotal.Add(1)
}

// DestroyTaskLockHeldTotal returns the current counter value. Exported for
// tests (asserts the OFD-lock-probe budget-exhaustion branch fires); a
// future `/metrics` exporter would also use this read path.
func DestroyTaskLockHeldTotal() int64 {
	return destroyTaskLockHeldTotal.Load()
}

// ResetDestroyTaskLockHeldForTest zeroes the counter so a test can pin
// its own baseline without depending on sibling-test ordering.
func ResetDestroyTaskLockHeldForTest() {
	destroyTaskLockHeldTotal.Store(0)
}

// startTaskStageTotal is the process-global counter behind
// `nomad_driver_ch_start_task_stage_total`. Bumped on every cold-boot
// StartTask that runs the driver-side staging op (TaskConfig.StageDiskImages
// = true). Counts BOTH success and failure invocations — paired with
// `nomad_driver_ch_start_task_stage_failures_total` an operator can
// compute the failure ratio without subtracting two counters with
// different sample windows.
//
// Operators rate-graph this against alloc count to confirm the driver-
// side staging surface is engaged: under Option C Phase 4 (when the
// controller flips its `driver_stages_disk_images` flag to true) every
// cold-boot alloc should bump this exactly once.
var startTaskStageTotal atomic.Int64

// incStartTaskStage bumps `nomad_driver_ch_start_task_stage_total` by
// one. Goroutine-safe; the atomic Int64 carries its own ordering.
func incStartTaskStage() {
	startTaskStageTotal.Add(1)
}

// StartTaskStageTotal returns the current counter value. Exported for
// tests (asserts the staging branch fires); a future `/metrics`
// exporter would also use this read path.
func StartTaskStageTotal() int64 {
	return startTaskStageTotal.Load()
}

// ResetStartTaskStageForTest zeroes the counter so a test can pin its
// own baseline without depending on sibling-test ordering.
func ResetStartTaskStageForTest() {
	startTaskStageTotal.Store(0)
}

// startTaskStageFailuresTotal is the process-global counter behind
// `nomad_driver_ch_start_task_stage_failures_total`. Bumped when the
// driver-side staging op (cold-boot, StageDiskImages=true) returns a
// non-nil error. Paired with `start_task_stage_total` to compute the
// failure ratio.
//
// A spike here means truncate(1) or mkfs.ext4(8) is failing on the
// worker — typical causes: disk-full (ENOSPC), missing binaries, or a
// quota cap. Triage hint: tail the next NOMAD task log for the typed
// staging error (the wrapper-equivalent error mentions the offending
// disk path).
var startTaskStageFailuresTotal atomic.Int64

// incStartTaskStageFailures bumps
// `nomad_driver_ch_start_task_stage_failures_total` by one.
// Goroutine-safe; the atomic Int64 carries its own ordering.
func incStartTaskStageFailures() {
	startTaskStageFailuresTotal.Add(1)
}

// StartTaskStageFailuresTotal returns the current counter value. Exported
// for tests; a future `/metrics` exporter would also use this read path.
func StartTaskStageFailuresTotal() int64 {
	return startTaskStageFailuresTotal.Load()
}

// ResetStartTaskStageFailuresForTest zeroes the counter so a test can
// pin its own baseline without depending on sibling-test ordering.
func ResetStartTaskStageFailuresForTest() {
	startTaskStageFailuresTotal.Store(0)
}

// destroyTaskTapStuckTotal is the process-global counter behind
// `nomad_driver_ch_destroy_task_tap_stuck_total`. Bumped when
// DestroyTask's synchronous tap-delete + ENODEV-verify gate exhausts
// its poll budget without observing the kernel evict the tap netdev.
//
// r24-A2-S2 (T-8b-stress-r8): the existing best-effort tap removal
// (h.tap call + defensive VMIndex-keyed pass) returned to Nomad on
// failure without verification. Stress-r8 showed cycles 1-19 failing
// identically with `Tap zsbx-nm-N already exists`: the kernel hadn't
// finished evicting the netdev between DestroyTask return and the
// next StartTask's tuntap-add. The strictly-stronger predicate is
// to poll `ip link show <tap>` for ENODEV before returning — if the
// netdev is still listed, the next alloc's tuntap-add will collide.
//
// On budget exhaustion: bump this counter, WARN-log, proceed (so
// Nomad still gets a definitive terminal signal and doesn't loop the
// destroy). Operators rate-graph this; a healthy fleet trends to
// zero. A spike means kernel netdev cleanup is wedged — orthogonal
// to the driver, but only the driver is positioned to observe it.
var destroyTaskTapStuckTotal atomic.Int64

// incDestroyTaskTapStuck bumps
// `nomad_driver_ch_destroy_task_tap_stuck_total` by one. Goroutine-
// safe; the atomic Int64 carries its own ordering.
func incDestroyTaskTapStuck() {
	destroyTaskTapStuckTotal.Add(1)
}

// DestroyTaskTapStuckTotal returns the current counter value.
// Exported for tests (asserts the tap-delete-and-verify budget-
// exhaustion branch fires); a future `/metrics` exporter would also
// use this read path.
func DestroyTaskTapStuckTotal() int64 {
	return destroyTaskTapStuckTotal.Load()
}

// ResetDestroyTaskTapStuckForTest zeroes the counter so a test can
// pin its own baseline without depending on sibling-test ordering.
func ResetDestroyTaskTapStuckForTest() {
	destroyTaskTapStuckTotal.Store(0)
}

// -- StartTaskRestoreFailures: per-stage restore failure counter ----
//
// T-8b-stress-r9-retry-4 NEXT-LAYER: stress-r9-retry-4 surfaced 5/6
// wake failures terminating with a generic
// `restore_backend_failed: ch: startTaskRestoreBran[truncated]`
// message — the controller's wake_jobs error_message column and the
// harness's 180-char display both clip the verbatim driver error
// before any stage detail surfaces.
//
// This counter family complements the per-stage error-string
// enrichment in `restore_task.go`: every named stage that returns a
// non-nil error from `startTaskRestoreBranch` bumps
// `nomad_driver_ch_start_task_restore_failures_total{stage="<stage>"}`
// by one. Operators rate-graph the family to see WHICH stage is
// dominating in the failure mix — e.g. a spike in
// `stage="restore_spawn"` localises the regression to CH process
// spawn (likely binary missing / EACCES), whereas
// `stage="livez_probe"` localises it to CH-internal restore-time
// (likely memory-image deserialisation or page-fault-in).
//
// Storage shape: a sync.Map keyed by stage string. The map is
// process-global; entries are created on first observation of a
// given stage (so a fleet that never sees `stage="resume"` failures
// won't carry a useless zero sample). Reads use Range to iterate
// entries in arbitrary order — the metrics exporter sorts them
// before rendering for stable output.
//
// Why a map and not a fixed struct: stages are listed in
// `restore_task.go` as untyped string constants; adding a new stage
// later (or splitting an existing one) should not require touching
// `metrics.go`. The map shape mirrors the labelled-counter pattern
// the Prometheus client library uses, kept dependency-free.
//
// Stage label cardinality is bounded by the number of named return
// sites in `startTaskRestoreBranch` (~15 today); no risk of unbounded
// cardinality.
var startTaskRestoreFailuresTotal sync.Map // map[string]*atomic.Int64

// incStartTaskRestoreFailures bumps the per-stage counter by one.
// Goroutine-safe: sync.Map.LoadOrStore returns the existing entry on
// concurrent first-touch so we never lose a sample. Empty stage is
// rejected (caller bug — pin via the named stage constants below
// rather than constructing strings at the call site).
func incStartTaskRestoreFailures(stage string) {
	if stage == "" {
		return
	}
	v, _ := startTaskRestoreFailuresTotal.LoadOrStore(stage, new(atomic.Int64))
	v.(*atomic.Int64).Add(1)
}

// StartTaskRestoreFailuresTotal returns the current value for a given
// stage. Returns 0 if the stage was never observed (i.e. no failures
// of that kind have occurred). Exported for tests; a future
// /metrics exporter would also use this read path.
func StartTaskRestoreFailuresTotal(stage string) int64 {
	v, ok := startTaskRestoreFailuresTotal.Load(stage)
	if !ok {
		return 0
	}
	return v.(*atomic.Int64).Load()
}

// StartTaskRestoreFailuresSnapshot returns a copy of the entire
// per-stage counter map. Used by the Prometheus textfile exporter to
// render one sample per observed stage. Caller must not mutate the
// returned map (it's a fresh copy, but the convention keeps the API
// shape consistent with the other Total accessors).
func StartTaskRestoreFailuresSnapshot() map[string]int64 {
	out := make(map[string]int64)
	startTaskRestoreFailuresTotal.Range(func(k, v any) bool {
		out[k.(string)] = v.(*atomic.Int64).Load()
		return true
	})
	return out
}

// ResetStartTaskRestoreFailuresForTest zeroes every per-stage counter
// so a test can pin its own baseline without depending on sibling-
// test ordering. Removes entries entirely (next read returns 0 via
// the not-found branch) so a test asserting on a fresh-process state
// sees the expected zero-cardinality snapshot.
func ResetStartTaskRestoreFailuresForTest() {
	startTaskRestoreFailuresTotal.Range(func(k, _ any) bool {
		startTaskRestoreFailuresTotal.Delete(k)
		return true
	})
}
