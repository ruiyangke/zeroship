//! Per-isolate live stream budget.
//!
//! Every `ReadableStream`, `WritableStream` and `TransformStream` holds one
//! unit of its isolate's budget from construction until its boxed state is
//! reclaimed: by the V8 weak finalizer when the stream is collected, or when
//! the isolate is disposed at the latest. A construction that would take the
//! isolate past [`MAX_LIVE_STREAMS`] throws `RangeError("too many concurrent
//! streams")` in that isolate.
//!
//! The count is an isolate slot, so it belongs to the runtime that owns the
//! isolate and dies with it. A worker thread hosts many apps' isolates; a
//! count held by the thread would let one app's live streams refuse its
//! neighbour's.
//!
//! The cap turns a stream leak into an error in the isolate that leaks. It is
//! not a throughput limit.

use std::cell::Cell;
use std::rc::Rc;

use crate::state::OpError;

/// Cap on concurrent live ReadableStream/WritableStream/TransformStream
/// instances per isolate. Construction past this throws `RangeError`.
pub const MAX_LIVE_STREAMS: u32 = 65_536;

/// One isolate's live stream count. Clones share the count.
#[derive(Clone, Debug, Default)]
pub struct StreamBudget {
    live: Rc<Cell<u32>>,
}

impl StreamBudget {
    /// The budget of `isolate`, installed on first use.
    pub fn of(isolate: &mut v8::Isolate) -> Self {
        if let Some(budget) = isolate.get_slot::<Self>() {
            return budget.clone();
        }
        let budget = Self::default();
        isolate.set_slot(budget.clone());
        budget
    }

    /// Streams currently charged to this budget.
    #[must_use]
    pub fn live(&self) -> u32 {
        self.live.get()
    }

    /// Charge `N` streams at once, or none: a construction that builds
    /// several streams is refused whole before it builds any of them.
    ///
    /// # Errors
    ///
    /// A `RangeError` when fewer than `N` streams are free.
    pub fn try_alloc<const N: usize>(&self) -> Result<[StreamBudgetGuard; N], OpError> {
        let live = self.live.get();
        let wanted = u32::try_from(N).unwrap_or(u32::MAX);
        if wanted > MAX_LIVE_STREAMS - live {
            return Err(OpError::range_error("too many concurrent streams"));
        }
        self.live.set(live + wanted);
        Ok(std::array::from_fn(|_| StreamBudgetGuard {
            live: self.live.clone(),
        }))
    }
}

/// Charge one stream to `isolate`'s budget.
///
/// # Errors
///
/// A `RangeError` when the isolate is at [`MAX_LIVE_STREAMS`].
pub fn try_alloc_stream(isolate: &mut v8::Isolate) -> Result<StreamBudgetGuard, OpError> {
    let [guard] = StreamBudget::of(isolate).try_alloc::<1>()?;
    Ok(guard)
}

/// Charge `N` streams to `isolate`'s budget, all or none.
///
/// # Errors
///
/// A `RangeError` when fewer than `N` streams are free.
pub fn try_alloc_streams<const N: usize>(
    isolate: &mut v8::Isolate,
) -> Result<[StreamBudgetGuard; N], OpError> {
    StreamBudget::of(isolate).try_alloc::<N>()
}

/// One stream's charge. Held by the stream's boxed state and released when
/// that state drops, which the V8 weak finalizer or isolate disposal does.
#[derive(Debug)]
pub struct StreamBudgetGuard {
    live: Rc<Cell<u32>>,
}

impl Drop for StreamBudgetGuard {
    fn drop(&mut self) {
        self.live.set(self.live.get() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_charges_its_budget_until_dropped() {
        let budget = StreamBudget::default();
        let [guard] = budget.try_alloc::<1>().expect("under cap");
        assert_eq!(budget.live(), 1);
        drop(guard);
        assert_eq!(budget.live(), 0);
    }

    #[test]
    fn a_refused_batch_charges_nothing() {
        let budget = StreamBudget::default();
        let held: Vec<_> = (1..MAX_LIVE_STREAMS)
            .map(|_| budget.try_alloc::<1>().expect("under cap"))
            .collect();
        assert_eq!(budget.live(), MAX_LIVE_STREAMS - 1);
        let refused = budget.try_alloc::<2>().expect_err("two do not fit in one");
        assert!(matches!(refused.kind, crate::state::OpErrorKind::RangeError));
        assert_eq!(budget.live(), MAX_LIVE_STREAMS - 1);
        let [last] = budget.try_alloc::<1>().expect("one still fits");
        assert_eq!(budget.live(), MAX_LIVE_STREAMS);
        drop((held, last));
        assert_eq!(budget.live(), 0);
    }
}
