//! An isolate's heap cap.
//!
//! One process hosts many tenants' isolates, so an isolate whose heap reaches
//! its cap has to stop alone. V8 consults the near-heap-limit callback once an
//! isolate's heap reaches its limit, and continues under the limit the callback
//! returns. V8 cannot fail the allocation in flight softly: a limit too small
//! for it ends in `FatalProcessOutOfMemory`, which aborts the process and every
//! tenant in it. So the callback ends the isolate instead of the allocation. It
//! requests termination, which unwinds the isolate's JavaScript at its next
//! interrupt check, and grants headroom for the allocation to finish in the
//! meantime. From then on the runtime enters no JavaScript in the isolate and
//! stops it: see `RuntimeInner::stop_for_heap_cap`.
//!
//! # The bound
//!
//! Each consultation grants at most [`UNWIND_HEADROOM`] past the current limit,
//! and the limit never passes the isolate's initial limit plus
//! [`UNWIND_GRANTS`] grants, however often V8 asks. So while it reaches the cap
//! an isolate holds at most its cap plus that much heap. It then holds that
//! only momentarily: the stop (`RuntimeInner::stop_for_heap_cap`) runs a full
//! GC to release the raised heap and notifies its host to drop the isolate at
//! the stop, rather than at the app's next request or an LRU eviction. An
//! isolate allocates only while its thread is running it, and a thread runs one
//! isolate at a time, so on each thread at most one isolate is between reaching
//! its cap and being stopped.
//!
//! What the bound does not cover: V8 honours the termination only at an
//! interrupt check. Code that reaches none keeps allocating after the
//! termination is requested, and past the bound V8 aborts the process, as it
//! would for any isolate without a callback. V8 offers an embedder no way to
//! fail those allocations instead.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::runtime::HEAP_LIMIT_CALLBACK_HITS;

/// Bytes of heap one consultation grants past the current limit.
///
/// V8's largest single heap object, a `FixedArray` or `FixedDoubleArray` at
/// `kMaxFixedArrayCapacity` elements or a two-byte string at
/// `String::kMaxLength`, plus slack for young-generation objects promoted
/// while the termination unwinds and for the runtime's own allocations in
/// that window. So the allocation that reached the cap completes whatever its
/// size.
pub const UNWIND_HEADROOM: usize = (1 << 30) + (64 << 20);

/// Consultations that can each grant [`UNWIND_HEADROOM`] while one allocation
/// completes and the isolate terminates.
///
/// Evidence: `a_single_maximal_allocation_is_caught_within_the_grant_ceiling`
/// allocates the largest single object V8 builds in one step between interrupt
/// checks (a 1 GiB `FixedDoubleArray`) under a small cap and counts the
/// callbacks. It takes two: the first lets the allocation cross the cap, the
/// second covers V8 reconsulting after a GC at the raised limit frees nothing.
/// Mutating this to 1 aborts the process on that one allocation.
///
/// This bounds one allocation step, not a run of them. Code that allocates
/// several maximal objects without the termination being honoured between them
/// (a builtin or TurboFan-optimised loop that reaches no interrupt check) needs
/// more grants the more it allocates, without limit, so no finite count bounds
/// it; that is the tracked process-kill escape, not a grant-count question.
pub const UNWIND_GRANTS: usize = 2;

/// The limit an isolate continues under after a consultation at `current`,
/// given V8's initial limit for it.
#[must_use]
pub fn granted_limit(current: usize, initial: usize) -> usize {
    let ceiling = initial.saturating_add(UNWIND_GRANTS.saturating_mul(UNWIND_HEADROOM));
    current.saturating_add(UNWIND_HEADROOM).min(ceiling).max(current)
}

/// Whether an isolate's heap reached its cap.
///
/// Set by the near-heap-limit callback and never cleared: an isolate that
/// reached its cap runs no more JavaScript. It is also an isolate slot, so a
/// microtask checkpoint can refuse to run without reaching the runtime.
#[derive(Clone, Default)]
pub struct CapReached(Arc<AtomicBool>);

impl CapReached {
    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// Whether `isolate`'s heap reached its cap. An isolate the runtime did
    /// not build carries no slot and has never reached a cap of the runtime's.
    pub fn in_isolate(isolate: &v8::Isolate) -> bool {
        isolate.get_slot::<Self>().is_some_and(Self::get)
    }
}

/// What the callback reads and writes: atomics and a thread-safe handle, so a
/// shared reference is enough.
struct Guard {
    handle: v8::IsolateHandle,
    reached: CapReached,
    /// The runtime's termination note, which says the termination it finds
    /// was the heap cap's.
    terminated: Arc<AtomicBool>,
}

/// The heap cap on one isolate: the near-heap-limit callback's registration,
/// and whether the isolate's heap reached the cap.
///
/// `RuntimeInner` declares it after the isolate, so it frees the callback's
/// state only once the isolate is disposed, after which V8 cannot call it.
pub struct HeapCap {
    guard: *mut Guard,
    reached: CapReached,
}

impl HeapCap {
    /// Register the callback on `isolate`. `terminated` is the note the
    /// runtime reads to name a termination's cause. Whether the cap was
    /// reached is also installed as `isolate`'s slot.
    pub fn install(isolate: &mut v8::OwnedIsolate, terminated: Arc<AtomicBool>) -> Self {
        let reached = CapReached::default();
        let guard = Box::into_raw(Box::new(Guard {
            handle: isolate.thread_safe_handle(),
            reached: reached.clone(),
            terminated,
        }));
        isolate.set_slot(reached.clone());
        isolate.add_near_heap_limit_callback(near_heap_limit, guard.cast::<c_void>());
        Self { guard, reached }
    }

    /// Whether the isolate's heap reached its cap.
    pub fn reached(&self) -> bool {
        self.reached.get()
    }
}

impl Drop for HeapCap {
    fn drop(&mut self) {
        // SAFETY: `guard` came from `Box::into_raw` in `install` and is freed
        // only here. The isolate that held it as callback data is disposed
        // before this runs (see the type's doc), so nothing reads it after.
        drop(unsafe { Box::from_raw(self.guard) });
    }
}

/// V8's near-heap-limit callback: end the isolate, and let the allocation in
/// flight finish within the bound.
unsafe extern "C" fn near_heap_limit(
    data: *mut c_void,
    current_heap_limit: usize,
    initial_heap_limit: usize,
) -> usize {
    // SAFETY: `data` is the `Guard` that `HeapCap` frees only after the
    // isolate is disposed, and V8 calls this only on the isolate's own thread
    // while the isolate is alive.
    let Some(guard) = (unsafe { data.cast::<Guard>().as_ref() }) else {
        return current_heap_limit;
    };
    // Process-wide and monotonic, so an observer can tell "V8 never consulted
    // the cap" from "it did and the isolate still ran on".
    HEAP_LIMIT_CALLBACK_HITS.fetch_add(1, Ordering::Relaxed);
    guard.reached.0.store(true, Ordering::Relaxed);
    guard.terminated.store(true, Ordering::Relaxed);
    // Every consultation requests termination again: a termination the
    // runtime already observed is spent, and JavaScript must not run on
    // headroom granted only for unwinding.
    guard.handle.terminate_execution();
    let granted = granted_limit(current_heap_limit, initial_heap_limit);
    if granted > current_heap_limit {
        tracing::warn!(
            heap_limit_mb = current_heap_limit >> 20,
            granted_mb = granted >> 20,
            "isolate reached its heap cap; terminating it"
        );
    } else {
        tracing::error!(
            heap_limit_mb = current_heap_limit >> 20,
            "isolate allocated past its heap cap headroom while terminating"
        );
    }
    granted
}

#[cfg(test)]
mod tests {
    use super::{UNWIND_GRANTS, UNWIND_HEADROOM, granted_limit};

    const CAP: usize = 64 << 20;

    /// The first consultation grants a whole headroom, so the allocation that
    /// reached the cap completes whatever its size.
    #[test]
    fn the_first_consultation_grants_a_whole_headroom() {
        assert_eq!(granted_limit(CAP, CAP), CAP + UNWIND_HEADROOM);
    }

    /// However often V8 asks, the limit stops at the initial limit plus the
    /// granted headroom, and a consultation at the ceiling grants nothing.
    #[test]
    fn repeated_consultations_stop_at_the_ceiling() {
        let ceiling = CAP + UNWIND_GRANTS * UNWIND_HEADROOM;
        let mut limit = CAP;
        let mut grants = 0;
        for _ in 0..UNWIND_GRANTS + 3 {
            let next = granted_limit(limit, CAP);
            assert!(next <= ceiling, "the limit passed the ceiling: {next} > {ceiling}");
            if next > limit {
                grants += 1;
            }
            limit = next;
        }
        assert_eq!(limit, ceiling);
        assert_eq!(grants, UNWIND_GRANTS, "only the granting consultations raise the limit");
        assert_eq!(granted_limit(ceiling, CAP), ceiling, "a consultation at the ceiling grants nothing");
    }

    /// V8 can clamp the limit below what was granted; the next consultation
    /// grants again, still within the ceiling, and never lowers the limit.
    #[test]
    fn a_clamped_limit_is_granted_again_within_the_ceiling() {
        let clamped = CAP + UNWIND_HEADROOM / 2;
        let next = granted_limit(clamped, CAP);
        assert_eq!(next, clamped + UNWIND_HEADROOM);
        assert!(next <= CAP + UNWIND_GRANTS * UNWIND_HEADROOM);
        let above = CAP + UNWIND_GRANTS * UNWIND_HEADROOM + 1;
        assert_eq!(granted_limit(above, CAP), above, "a limit already past the ceiling is kept, not lowered");
    }
}
