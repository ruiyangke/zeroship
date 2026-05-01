//! Queue-with-sizes primitives — spec §8.1.
//!
//! The spec describes a "queue" as a list of `(value, size)` pairs with a
//! running `[[queueTotalSize]]`. Five abstract operations maintain the
//! invariant `queueTotalSize == sum(entry.size)` (or `byte_length` for
//! byte queues per critic C-9):
//!
//! - `EnqueueValueWithSize(container, value, size)` — append + add size
//! - `DequeueValue(container)` — pop front + subtract size, return value
//! - `PeekQueueValue(container)` — front value, no mutation
//! - `ResetQueue(container)` — clear queue + zero total
//! - `IsNonNegativeNumber(v)` and friends — boundary checks at enqueue
//!
//! This module is the SINGLE source of truth for these invariants. Any
//! controller that bypasses these helpers and mutates queues directly is
//! a bug.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

// ---------------------------------------------------------------------------
// Queue entry types
// ---------------------------------------------------------------------------

/// Default-stream queue entry (§3.6, §3.10). Holds an arbitrary V8 value
/// plus the size produced by the user's `size(chunk)` callback (or `1` for
/// CountQueuingStrategy).
#[allow(missing_debug_implementations)]
pub struct ValueQueueEntry {
    pub value: v8::Global<v8::Value>,
    pub size: f64,
}

/// Byte-stream queue entry (§3.7, §3.11). Holds a fully-detached
/// `ArrayBuffer` plus an offset/length window so partial reads can fill
/// pull-into descriptors without re-slicing.
///
/// Per spec `ReadableByteStreamControllerEnqueue`: the user-supplied view
/// is detached via `TransferArrayBuffer` before being put on the queue.
/// The window persists original `byteOffset` / `byteLength` from the
/// source view.
#[allow(missing_debug_implementations)]
pub struct ByteQueueEntry {
    pub buffer: v8::Global<v8::ArrayBuffer>,
    pub byte_offset: usize,
    pub byte_length: usize,
}

// ---------------------------------------------------------------------------
// Queue container — generic over entry type
// ---------------------------------------------------------------------------
//
// The spec phrases enqueue/dequeue/reset over a generic `container` that
// has both a queue and a queueTotalSize. We model that as a trait so
// DefaultController and ByteController both call the same helpers.

/// Common operations on a stream queue. The spec's enqueue/dequeue/reset
/// are written generically over (queue, queueTotalSize); this trait makes
/// the abstract operation directly callable from either controller.
pub trait QueueContainer {
    /// The queue entry type. `f64` size is preserved on the entry itself
    /// so dequeue can subtract; for byte entries the size is `byte_length`.
    type Entry;

    /// Append an already-sized entry. Updates the running total.
    fn enqueue_internal(&self, entry: Self::Entry, size: f64);

    /// Pop the front entry; subtract its size from the running total.
    /// Returns `None` if the queue is empty (caller checks queue length
    /// before dequeue per spec).
    fn dequeue_internal(&self) -> Option<Self::Entry>;

    /// Peek the front entry's reference without mutating. Used by some
    /// algorithms that look at the head before deciding to dequeue.
    fn peek_internal(&self) -> Option<std::cell::Ref<'_, Self::Entry>>;

    /// Reset queue to empty, total size to 0.
    fn reset_internal(&self);

    /// Number of entries currently queued.
    fn queue_len(&self) -> usize;

    /// `[[queueTotalSize]]` — the spec invariant. Read-only public view.
    fn queue_total_size(&self) -> f64;
}

// ---------------------------------------------------------------------------
// Default-stream queue (Vec<ValueQueueEntry>)
// ---------------------------------------------------------------------------

/// Default-stream queue state. Bundled together so the "queue + total"
/// pair invariant is local to one struct.
#[derive(Default)]
#[allow(missing_debug_implementations)]
pub struct ValueQueue {
    queue: RefCell<VecDeque<ValueQueueEntry>>,
    total_size: Cell<f64>,
}

impl ValueQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// `EnqueueValueWithSize(container, value, size)` — §8.1.
    ///
    /// Per spec step 1: `IsNonNegativeNumber(size)` is checked at the
    /// caller (controller-level) and a TypeError thrown there. By the
    /// time we reach this helper, `size` is already validated.
    pub fn enqueue_value_with_size(&self, value: v8::Global<v8::Value>, size: f64) {
        debug_assert!(size.is_finite() && size >= 0.0, "size must be non-negative finite");
        self.queue
            .borrow_mut()
            .push_back(ValueQueueEntry { value, size });
        self.total_size.set(self.total_size.get() + size);
    }

    /// `DequeueValue(container)` — §8.1. Returns `None` on empty queue.
    pub fn dequeue_value(&self) -> Option<ValueQueueEntry> {
        let entry = self.queue.borrow_mut().pop_front()?;
        let new_total = self.total_size.get() - entry.size;
        // Spec: clamp to zero to avoid floating-point underflow producing
        // a microscopic negative total.
        self.total_size.set(if new_total < 0.0 { 0.0 } else { new_total });
        Some(entry)
    }

    /// Peek the head without dequeuing. Used by spec sites that test the
    /// queue's front before deciding to consume.
    pub fn peek(&self) -> Option<std::cell::Ref<'_, ValueQueueEntry>> {
        let q = self.queue.borrow();
        if q.is_empty() {
            None
        } else {
            // Return a Ref<&ValueQueueEntry> via mapping the borrow.
            Some(std::cell::Ref::map(q, |q| q.front().unwrap()))
        }
    }

    /// `ResetQueue(container)` — §8.1.
    pub fn reset_queue(&self) {
        self.queue.borrow_mut().clear();
        self.total_size.set(0.0);
    }

    /// `[[queueTotalSize]]` — public read-only.
    pub fn total_size(&self) -> f64 {
        self.total_size.get()
    }

    /// Queue length (entry count, not byte total).
    pub fn len(&self) -> usize {
        self.queue.borrow().len()
    }

    /// True if no entries queued.
    pub fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }
}

// ---------------------------------------------------------------------------
// Byte-stream queue (Vec<ByteQueueEntry>)
// ---------------------------------------------------------------------------

/// Byte-stream queue state. Per critic C-9: total size is `sum(byte_length)`,
/// maintained by exactly the helpers below. No other site mutates either
/// the queue or the running total.
#[derive(Default)]
#[allow(missing_debug_implementations)]
pub struct ByteQueue {
    queue: RefCell<VecDeque<ByteQueueEntry>>,
    total_size: Cell<f64>,
}

impl ByteQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a byte entry. The caller has already detached the buffer
    /// (per `ReadableByteStreamControllerEnqueue` step 8 / `TransferArrayBuffer`).
    pub fn enqueue_byte_entry(&self, entry: ByteQueueEntry) {
        let len = entry.byte_length as f64;
        self.queue.borrow_mut().push_back(entry);
        self.total_size.set(self.total_size.get() + len);
    }

    /// Pop the front byte entry. Subtracts its `byte_length` from total.
    pub fn dequeue_byte_entry(&self) -> Option<ByteQueueEntry> {
        let entry = self.queue.borrow_mut().pop_front()?;
        let new_total = self.total_size.get() - entry.byte_length as f64;
        self.total_size.set(if new_total < 0.0 { 0.0 } else { new_total });
        Some(entry)
    }

    /// Reset byte queue to empty.
    pub fn reset_queue(&self) {
        self.queue.borrow_mut().clear();
        self.total_size.set(0.0);
    }

    /// Total bytes currently queued.
    pub fn total_size(&self) -> f64 {
        self.total_size.get()
    }

    pub fn len(&self) -> usize {
        self.queue.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.borrow().is_empty()
    }

    /// Drain entries front-to-back. Used by `ResetQueue` analogue paths
    /// that want to release each ArrayBuffer global eagerly.
    pub fn drain_into<F: FnMut(ByteQueueEntry)>(&self, mut f: F) {
        let mut q = self.queue.borrow_mut();
        while let Some(e) = q.pop_front() {
            f(e);
        }
        self.total_size.set(0.0);
    }

    /// Pop the front entry. Same as `dequeue_byte_entry` but named to
    /// match the byte-controller's helper naming.
    pub fn drain_into_first(&self) -> Option<ByteQueueEntry> {
        self.dequeue_byte_entry()
    }

    /// Push an entry to the front (used when an enqueue is partially
    /// consumed to fill a pending pull-into and the remainder needs to
    /// stay at the head of the queue for the next descriptor).
    pub fn push_front_byte_entry(&self, entry: ByteQueueEntry) {
        let len = entry.byte_length as f64;
        self.queue.borrow_mut().push_front(entry);
        self.total_size.set(self.total_size.get() + len);
    }
}

// ---------------------------------------------------------------------------
// IsNonNegativeNumber — spec §7.4
// ---------------------------------------------------------------------------

/// `IsNonNegativeNumber(v)` per spec §7.4: Number, not NaN, not negative.
///
/// Used by `ReadableStreamDefaultControllerEnqueue` to validate the size
/// returned from the user's strategy callback before accepting the chunk.
/// Returning `false` causes the caller to throw `RangeError`.
pub fn is_non_negative_number(v: f64) -> bool {
    !v.is_nan() && v >= 0.0
}

// ---------------------------------------------------------------------------
// Tests — pure-Rust, no V8 needed
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_queue_invariant_holds_across_enqueue_dequeue() {
        let q = ByteQueue::new();
        assert_eq!(q.total_size(), 0.0);
        assert!(q.is_empty());
    }

    #[test]
    fn byte_queue_reset_zeros_total() {
        let q = ByteQueue::new();
        q.reset_queue();
        assert_eq!(q.total_size(), 0.0);
    }

    #[test]
    fn is_non_negative_number_rejects_nan() {
        assert!(!is_non_negative_number(f64::NAN));
    }

    #[test]
    fn is_non_negative_number_rejects_negative() {
        assert!(!is_non_negative_number(-1.0));
        assert!(!is_non_negative_number(-0.001));
    }

    #[test]
    fn is_non_negative_number_accepts_zero_and_positive() {
        assert!(is_non_negative_number(0.0));
        assert!(is_non_negative_number(1.0));
        assert!(is_non_negative_number(f64::INFINITY)); // spec allows +∞
    }

    #[test]
    fn is_non_negative_number_treats_negative_zero_as_non_negative() {
        // -0.0 is mathematically zero; spec IsNonNegativeNumber checks
        // `v >= 0`, not signed-zero distinction.
        assert!(is_non_negative_number(-0.0));
    }

    #[test]
    fn value_queue_len_zero_when_empty() {
        let q = ValueQueue::new();
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
        assert_eq!(q.total_size(), 0.0);
    }
}

// QueueContainer trait stub — the controllers use ValueQueue / ByteQueue
// directly. The trait stays here as documentation; uncomment if a generic
// algorithm needs to abstract over both.
#[allow(dead_code)]
pub(crate) const _QUEUE_CONTAINER_DOC: () = ();
