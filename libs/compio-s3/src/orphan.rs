//! Best-effort orphaned-multipart cleanup queue.
//!
//! A multipart upload that is DROP-cancelled mid-flight (bounded-plan wall
//! timeout → `cancel.cancel()` + future drop, client disconnect, isolate LRU
//! eviction) never reaches its explicit, awaited `abort_multipart` on the
//! error path. Its uploaded parts are then orphaned — and S3 BILLS for orphaned
//! parts until a lifecycle rule reclaims them.
//!
//! The cancellation happens in a synchronous `Drop`, where we MUST NOT spawn or
//! await (spawning in `Drop` panics off-runtime; a panic in `Drop` while
//! unwinding aborts the process — the C1 rule). So the `Drop` guard does the
//! only two sound things:
//!
//! 1. `tracing::warn!` that parts may be orphaned, and
//! 2. push `(stored_key, upload_id)` onto this process-thread-local queue.
//!
//! A later storage op (or an explicit drainer task) then calls
//! [`S3Client::drain_orphaned_uploads`](crate::S3Client::drain_orphaned_uploads),
//! which pops the queue and issues a real `abort_multipart` for each entry IN
//! ASYNC CONTEXT — the abort the `Drop` could not perform itself.
//!
//! The queue is `thread_local!`: every storage op runs on the worker thread
//! that owns the V8 isolate + compio runtime, so a thread-local is the natural
//! process-owned home and needs no locking. It is bounded so a pathological
//! drop storm cannot grow it without limit.

use std::cell::RefCell;
use std::collections::VecDeque;

use crate::client::UploadId;

/// Hard cap on queued orphan records. A drop storm past this drops the OLDEST
/// record (it has already been `warn!`-logged, and an S3 lifecycle rule is the
/// backstop-of-last-resort) rather than growing unbounded.
const MAX_QUEUED_ORPHANS: usize = 4096;

thread_local! {
    /// `(stored_key, upload_id)` pairs awaiting a best-effort async abort.
    static ORPHANED_UPLOADS: RefCell<VecDeque<(String, UploadId)>> =
        const { RefCell::new(VecDeque::new()) };
}

/// Record an orphaned multipart upload for later best-effort async abort.
///
/// Called from a synchronous, possibly-unwinding `Drop` — so it NEVER spawns,
/// awaits, or panics. It only enqueues. The cap bounds memory under a drop
/// storm (oldest record evicted).
pub fn record_orphan(stored_key: String, upload_id: UploadId) {
    ORPHANED_UPLOADS.with(|q| {
        let mut q = q.borrow_mut();
        if q.len() >= MAX_QUEUED_ORPHANS {
            q.pop_front();
        }
        q.push_back((stored_key, upload_id));
    });
}

/// Drain every currently-queued orphan record, returning them for the caller to
/// abort in async context. Leaves the queue empty.
#[must_use]
pub fn take_orphans() -> Vec<(String, UploadId)> {
    ORPHANED_UPLOADS.with(|q| q.borrow_mut().drain(..).collect())
}

/// Number of records currently queued (test/diagnostic aid).
#[must_use]
pub fn queued_orphan_count() -> usize {
    ORPHANED_UPLOADS.with(|q| q.borrow().len())
}

/// A RAII guard arming the best-effort orphan-cleanup path for ONE in-flight
/// multipart upload.
///
/// It shares the live `UploadId` with the upload body via an interior-mutable
/// `RefCell<Option<UploadId>>` reached through an `&MultipartGuard` borrow
/// (single-threaded `?Send` world): the body calls [`MultipartGuard::set`]
/// right after
/// `create_multipart`, and [`MultipartGuard::clear`] on a CLEAN complete (or
/// the explicit, awaited error-path abort). If the upload future is instead
/// DROP-cancelled mid-flight, the cell still holds `Some(id)` when this guard's
/// `Drop` runs — so `Drop`:
///
/// 1. emits a `tracing::warn!` (orphaned parts; S3 lifecycle reclaims them),
///    and
/// 2. enqueues `(key, upload_id)` via [`record_orphan`] for a later async
///    `drain_orphaned_uploads`.
///
/// `Drop` is SYNC and never spawns/awaits/panics — the C1 rule. The real abort
/// happens later, in async context, off this queue.
///
/// The guard is single-owner: `put_stream` / `put_blob_stream` keep it on their
/// stack and hand the body an `&MultipartGuard`. Whether that stack frame
/// completes, errors, or is dropped mid-await, exactly one `Drop` runs — and it
/// enqueues iff an id is still live (i.e. neither clean-completed nor
/// explicitly aborted).
#[derive(Debug)]
pub struct MultipartGuard {
    /// Logical key (pre-`config.prefix`) the upload was created under.
    key: String,
    /// The live upload id, shared with the upload body. `None` until the body
    /// creates the multipart, and again after a clean complete / explicit abort.
    upload: RefCell<Option<UploadId>>,
}

impl MultipartGuard {
    /// Arm a fresh guard for an upload under `key` (no upload id yet).
    #[must_use]
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            upload: RefCell::new(None),
        }
    }

    /// Record the live upload id (call right after `create_multipart`).
    pub fn set(&self, id: UploadId) {
        *self.upload.borrow_mut() = Some(id);
    }

    /// Whether the multipart upload has been created (an id is live).
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.upload.borrow().is_some()
    }

    /// Take the live upload id, disarming the guard. Used by the explicit
    /// error-path abort (which performs the abort itself) and by a clean
    /// complete (which passes `None` — there is nothing to abort).
    #[must_use]
    pub fn take(&self) -> Option<UploadId> {
        self.upload.borrow_mut().take()
    }
}

impl Drop for MultipartGuard {
    fn drop(&mut self) {
        // Enqueue iff an id is still live (not clean-completed / explicitly
        // aborted). `take` makes this idempotent regardless of drop path.
        if let Some(id) = self.upload.borrow_mut().take() {
            tracing::warn!(
                key = %self.key,
                upload_id = %id.0,
                "multipart upload dropped without completion — orphaned parts; \
                 enqueued for best-effort async abort (ensure an S3 \
                 AbortIncompleteMultipartUpload lifecycle rule as a backstop)",
            );
            record_orphan(self.key.clone(), id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_take_roundtrips() {
        // Clear any residue from earlier tests on this thread.
        let _ = take_orphans();
        record_orphan("blobs/abc".into(), UploadId("u-1".into()));
        record_orphan("blobs/def".into(), UploadId("u-2".into()));
        assert_eq!(queued_orphan_count(), 2);
        let taken = take_orphans();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].0, "blobs/abc");
        assert_eq!(taken[0].1 .0, "u-1");
        // Drained.
        assert_eq!(queued_orphan_count(), 0);
    }

    #[test]
    fn guard_dropped_with_live_id_enqueues_orphan() {
        let _ = take_orphans();
        {
            let g = MultipartGuard::new("blobs/live");
            g.set(UploadId("u-live".into()));
            assert!(g.is_armed());
            // Simulate a DROP-cancellation: the guard goes out of scope WITHOUT
            // a clean complete (no `take()` by the owner) → its Drop must arm
            // the orphan queue.
        }
        assert_eq!(queued_orphan_count(), 1, "drop must enqueue the orphan");
        let taken = take_orphans();
        assert_eq!(taken[0].0, "blobs/live");
        assert_eq!(taken[0].1 .0, "u-live");
    }

    #[test]
    fn guard_cleanly_completed_does_not_enqueue() {
        let _ = take_orphans();
        {
            let g = MultipartGuard::new("blobs/clean");
            g.set(UploadId("u-clean".into()));
            // Clean complete: owner takes the id (disarm) before drop.
            let _ = g.take();
        }
        assert_eq!(
            queued_orphan_count(),
            0,
            "a cleanly-completed upload must NOT be enqueued"
        );
    }

    #[test]
    fn guard_never_created_does_not_enqueue() {
        let _ = take_orphans();
        {
            // Single-part path: create_multipart never ran, so no id set.
            let _g = MultipartGuard::new("blobs/single");
        }
        assert_eq!(queued_orphan_count(), 0);
    }

    #[test]
    fn queue_is_bounded_under_storm() {
        let _ = take_orphans();
        for i in 0..(MAX_QUEUED_ORPHANS + 100) {
            record_orphan(format!("k{i}"), UploadId(format!("u{i}")));
        }
        assert_eq!(queued_orphan_count(), MAX_QUEUED_ORPHANS);
        // Oldest evicted: the first surviving record is k100, not k0.
        let taken = take_orphans();
        assert_eq!(taken.first().unwrap().0, "k100");
    }
}
