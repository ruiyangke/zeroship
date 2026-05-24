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

package ch

import "sync/atomic"

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
