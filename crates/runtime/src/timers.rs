//! Min-heap timer system — setTimeout/setInterval with zero-CPU idle.
//!
//! Timers are stored in a BinaryHeap (min-heap via Reverse) for O(log n) insert
//! and O(1) peek of next-to-fire. Cleared timers use lazy deletion — they stay
//! in the heap but are skipped when popped.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::event_loop::SharedState;

/// Entry in the timer min-heap. Ordered by (fire_at, id).
#[derive(Eq, PartialEq)]
pub(crate) struct TimerHeapEntry {
    pub(crate) fire_at: Instant,
    pub(crate) id: u32,
}

impl Ord for TimerHeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.fire_at
            .cmp(&other.fire_at)
            .then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for TimerHeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Backing storage for a timer's callback.
#[allow(missing_debug_implementations)]
pub(crate) struct TimerCallback {
    pub(crate) callback: v8::Global<v8::Function>,
    /// None = setTimeout (one-shot), Some(dur) = setInterval (repeating).
    pub(crate) interval: Option<Duration>,
}

/// A timer min-heap + callback store.
#[allow(missing_debug_implementations)]
pub(crate) struct TimerState {
    /// Min-heap: next-to-fire on top (via Reverse for BinaryHeap).
    pub(crate) heap: BinaryHeap<Reverse<TimerHeapEntry>>,
    /// Callback storage, keyed by timer ID.
    pub(crate) callbacks: HashMap<u32, TimerCallback>,
    pub(crate) next_id: u32,
}

impl TimerState {
    pub(crate) fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            callbacks: HashMap::new(),
            next_id: 1,
        }
    }
}

/// Fire all timers whose fire_at <= now. Returns true if any timer fired.
pub(crate) fn fire_ready_timers(scope: &mut v8::PinScope, state: &SharedState) -> bool {
    let mut any_fired = false;
    let now = Instant::now();

    loop {
        let should_fire = {
            let s = state.borrow();
            s.timers
                .heap
                .peek()
                .map(|Reverse(e)| e.fire_at <= now)
                .unwrap_or(false)
        };
        if !should_fire {
            break;
        }

        let entry = state.borrow_mut().timers.heap.pop().unwrap().0;

        // Take callback out (lazy deletion: cleared timers won't have an entry)
        let cb_opt = state.borrow_mut().timers.callbacks.remove(&entry.id);

        if let Some(cb) = cb_opt {
            any_fired = true;

            let func = v8::Local::new(scope, &cb.callback);
            let undefined = v8::undefined(scope).into();
            func.call(scope, undefined, &[]);
            scope.perform_microtask_checkpoint();

            if let Some(dur) = cb.interval {
                // setInterval: re-insert callback + new heap entry
                state.borrow_mut().timers.callbacks.insert(entry.id, cb);
                state.borrow_mut().timers.heap.push(Reverse(TimerHeapEntry {
                    fire_at: Instant::now() + dur,
                    id: entry.id,
                }));
            }
            // setTimeout: cb drops here, Global handle freed — no leak
        }
    }

    any_fired
}
