use std::future::Future;
use std::task::Poll;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::StreamError;

/// An injectable source of monotonic time.
///
/// A caller that computes a stall from elapsed time reads it from here rather
/// than calling [`std::time::Instant::now`] directly, so a stall-timeout
/// decision can be driven deterministically in a test instead of by real
/// elapsed wall time. [`SystemClock`] is the production default wherever a
/// caller accepts one; a test swaps in a clock it advances itself and stops
/// depending on real timer latency under load.
pub trait Clock: std::fmt::Debug + Send + Sync {
    fn now(&self) -> Instant;

    /// Time elapsed since `mark`, saturating at zero rather than panicking if
    /// `mark` is somehow ahead of `now` - never true for a correct clock, but
    /// keeps a caller from turning that bug into a panic.
    fn elapsed(&self, mark: Instant) -> Duration {
        self.now().saturating_duration_since(mark)
    }
}

/// The real monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamRecord {
    pub partition: i32,
    /// Kafka record offset. Passing this value to `commit` commits `offset + 1`
    /// as the next record to consume for the same partition.
    pub offset: i64,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamOffset {
    pub partition: i32,
    /// Inclusive last-processed record offset. Transports commit `offset + 1`
    /// to their authoritative offset store.
    pub offset: i64,
}

impl From<&StreamRecord> for StreamOffset {
    fn from(value: &StreamRecord) -> Self {
        Self {
            partition: value.partition,
            offset: value.offset,
        }
    }
}

/// Await one transport call against a stall deadline: `Some` with the call's
/// answer, or `None` once `expiry` fires while the call is still suspended.
///
/// The call is polled first on every wake-up, so an answer that is ready is
/// always taken, even when `expiry` has fired too: an acknowledged publish or a
/// delivered batch is never thrown away for a timer. (compio's `timeout` is a
/// fair `select`, which picks between two ready branches at random.) Only a
/// call that is still suspended when `expiry` fires is abandoned. A call that blocks its thread instead of suspending cannot be
/// cut this way, so a caller also checks its stall clock between calls; see
/// [`StreamTransport::stall_timeout`].
///
/// `expiry` is the caller's timer, so this stays independent of any runtime.
pub async fn unless_stalled<T>(
    call: impl Future<Output = T>,
    expiry: impl Future<Output = ()>,
) -> Option<T> {
    let mut call = std::pin::pin!(call);
    let mut expiry = std::pin::pin!(expiry);
    std::future::poll_fn(|cx| {
        if let Poll::Ready(answer) = call.as_mut().poll(cx) {
            return Poll::Ready(Some(answer));
        }
        expiry.as_mut().poll(cx).map(|()| None)
    })
    .await
}

/// Where one partition ended when [`StreamTransport::end_offsets`] captured it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PartitionEnd {
    pub partition: i32,
    /// Offset of the last record the partition held at the capture, or `None`
    /// when it held no retained record and a reader has nothing to wait for.
    pub last_offset: Option<i64>,
}

/// Kafka-family stream contract:
///
/// - partition offsets are monotonic and stored by the transport;
/// - consumer groups assign each partition to one consumer in steady state;
/// - `partition_key` controls partition placement and preserves per-key order.
#[async_trait(?Send)]
pub trait StreamTransport: Send + Sync {
    fn id(&self) -> &str;

    async fn publish(
        &self,
        topic: &str,
        partition_key: &[u8],
        payload: &[u8],
    ) -> Result<(), StreamError>;

    async fn poll(&self, max: usize) -> Result<Vec<StreamRecord>, StreamError>;

    async fn commit(&self, offsets: &[StreamOffset]) -> Result<(), StreamError>;

    /// Rewind this consumer to the retained beginning of its assigned stream.
    ///
    /// The billing event-forwarder never calls this; it relies on normal
    /// consumer-group offsets. The control spend recompute cron uses a
    /// dedicated consumer group and rewinds before each full-period snapshot so
    /// reruns overwrite the same `usage_aggregates` totals instead of adding or
    /// reading only a tail.
    async fn rewind(&self) -> Result<(), StreamError>;

    /// Capture where every partition of this transport's topic ends right now.
    ///
    /// One entry per partition of the topic, whether or not this consumer is
    /// assigned it: a caller that must read the whole topic (the control spend
    /// recompute) then counts a partition it is never handed as unfinished
    /// rather than as empty. Each entry names the offset of the last record
    /// the partition held at the moment of capture, and a reader has read the
    /// partition to that end once `poll` hands it a record at or past that
    /// offset. A topic that does not exist yet has no partitions, and the list
    /// is empty.
    ///
    /// Records appended after the capture are not covered. A scan that reads
    /// each partition up to its captured last offset therefore has a fixed,
    /// finite amount to read however fast producers write, and what they write
    /// meanwhile belongs to the next scan.
    async fn end_offsets(&self) -> Result<Vec<PartitionEnd>, StreamError>;

    /// How long a caller may wait on this transport without progress before it
    /// treats the broker as stalled, taken from this transport's own
    /// configuration.
    ///
    /// Progress is a call that moves the caller's work forward: a `poll` that
    /// delivers a record the caller had not read yet, a `publish` the broker
    /// acknowledged. The bound is on the gap between two such calls, never on
    /// the total, so a long but steadily advancing loop runs to completion and
    /// only a broker that stops answering, or answers without ever advancing,
    /// is cut off. The control spend recompute's scan and the usage outbox's
    /// batch publish both bound their loops this way, each restarting the
    /// clock whenever it progresses.
    ///
    /// `None` for an in-process transport whose calls finish without waiting on
    /// anything: there a `poll` that delivers nothing new means nothing new is
    /// coming, and a `publish` needs no bound.
    fn stall_timeout(&self) -> Option<Duration>;
}

#[cfg(test)]
mod tests {
    use std::future::{pending, ready};

    use futures::executor::block_on;

    use super::*;

    /// A call whose answer is ready is taken even though its timer has fired
    /// as well, so an answer is never discarded for a timer.
    #[test]
    fn a_ready_answer_wins_over_a_timer_that_has_fired() {
        assert_eq!(block_on(unless_stalled(ready(7), ready(()))), Some(7));
    }

    #[test]
    fn a_call_still_suspended_when_the_timer_fires_is_abandoned() {
        assert_eq!(block_on(unless_stalled(pending::<i32>(), ready(()))), None);
    }

    /// `Clock::elapsed` must not panic when `mark` is ahead of what `now`
    /// answers - never true for a correct clock, but the default impl must
    /// not propagate that as a subtraction panic.
    #[test]
    fn elapsed_saturates_at_zero_when_mark_is_ahead_of_now() {
        #[derive(Debug)]
        struct FixedClock(Instant);
        impl Clock for FixedClock {
            fn now(&self) -> Instant {
                self.0
            }
        }
        let base = Instant::now();
        let clock = FixedClock(base);
        assert_eq!(clock.elapsed(base + Duration::from_secs(1)), Duration::ZERO);
        assert_eq!(clock.elapsed(base), Duration::ZERO);
    }

    #[test]
    fn a_call_that_answers_after_suspending_is_taken_while_the_timer_waits() {
        let mut suspended = false;
        let call = std::future::poll_fn(move |cx| {
            if suspended {
                Poll::Ready(3)
            } else {
                suspended = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        });
        assert_eq!(block_on(unless_stalled(call, pending::<()>())), Some(3));
    }
}
