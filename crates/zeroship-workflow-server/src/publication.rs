//! A per-app coalescing wake for committed publication intents.
//!
//! A settle, a start, a signal, a transition or a restart commits immutable
//! publication intents into this service's journal. The host that owns the
//! queue publishes them through [`AppWorkflows::publish_pending_jobs`], which
//! is the one drain both the local CLI host and this service use. Waiting for
//! the manager's periodic reconciliation makes every step, sleep, child call
//! and signal resume at that cadence, so a committed intent wakes one drain
//! task for its app instead.
//!
//! The wake is a channel that holds one pending signal: a signal while a pass
//! is running leaves exactly one more pass to take, and further signals
//! collapse into it rather than queuing one pass each. Publication stays
//! idempotent and app-scoped, so a redundant pass confirms nothing new and a
//! publisher never crosses its app.
use crate::sweeps::LanePublisher;
use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use zeroship_core::app_id::AppId;
use zeroship_workflow::service::{AppWorkflows, CommitHint};
use zeroship_workflow_manager::Queue;

/// Bound on one drain pass. A pass cut off here leaves the rest of the app's
/// intents for the next wake or for the manager's reconciliation.
const PUBLICATION_DEADLINE: Duration = Duration::from_secs(10);

/// One app's publication wake on the runtime that owns its journal.
///
/// The pass counter is shared with the drain task on its own, never through the
/// wake: a task holding the wake would hold its sender, and a wake dropped when
/// its app is rebound would never disconnect and end the task.
#[derive(Debug)]
pub struct PublicationWake {
    pending: flume::Sender<()>,
    passes: Rc<Cell<u64>>,
    /// Set once a signal found the drain task gone, so the disconnect is
    /// reported once rather than on every later commit.
    reported: Arc<AtomicBool>,
}

impl PublicationWake {
    pub(crate) fn new(pending: flume::Sender<()>) -> Self {
        Self {
            pending,
            passes: Rc::new(Cell::new(0)),
            reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Drain passes this wake has started. A pass is counted when it begins,
    /// including a pass whose publication a later attempt completes.
    ///
    /// This is the observable a caller waits on to know a wake has run; the
    /// drain itself publishes and reports nothing here.
    #[must_use]
    pub fn passes(&self) -> u64 {
        self.passes.get()
    }

    /// The host hint the engine fires after a mutating commit. A signal already
    /// pending absorbs this one, so a burst schedules one pass rather than one
    /// per signal. A disconnected channel means this wake's task has ended; the
    /// host is told once, because a reconnect would need a new wake anyway.
    pub(crate) fn hint(&self) -> CommitHint {
        let pending = self.pending.clone();
        let reported = Arc::clone(&self.reported);
        Arc::new(move || {
            if pending.try_send(()).is_err() && !reported.swap(true, Ordering::Relaxed) {
                tracing::warn!("workflow publication wake is disconnected");
            }
        })
    }

    pub(crate) fn counter(&self) -> Rc<Cell<u64>> {
        Rc::clone(&self.passes)
    }
}

/// The coalescing half of a wake, separate from the journal so a case can
/// drive it directly.
///
/// A pass is counted when it begins. When `pass` reports it was cut off or
/// refused, this takes exactly one more pass: a deadline or a refused page
/// leaves intents pending, and spending the wake on them beats leaving them to
/// reconciliation. The automatic pass is not itself re-armed, so a persistent
/// refusal cannot spin the loop.
#[expect(
    clippy::future_not_send,
    reason = "the coalescing loop drives a pass over this service's journal on its runtime"
)]
async fn coalesce<F>(pending: &flume::Receiver<()>, passes: &Cell<u64>, mut pass: F)
where
    F: std::ops::AsyncFnMut() -> bool,
{
    while pending.recv_async().await.is_ok() {
        while pending.try_recv().is_ok() {}
        passes.set(passes.get() + 1);
        if pass().await {
            // A cut-off or refused pass leaves intents pending, so take one
            // more pass now rather than waiting for reconciliation. The
            // automatic pass is never itself re-armed, so a persistent refusal
            // runs at most two passes per signal and cannot spin.
            passes.set(passes.get() + 1);
            let _ = pass().await;
        }
    }
}

/// Drain `engine`'s pending intents until every sender of this wake is
/// dropped.
///
/// After one signal is taken, a signal that arrives while the pass runs is the
/// one extra pass; further signals fold into it. A pass that refuses or times
/// out leaves its intents pending for the next wake or for the manager's
/// reconciliation, which never loses them, and the wake spends one more pass on
/// them before yielding.
#[expect(
    clippy::future_not_send,
    reason = "the wake drains this service's journal on the runtime that opened it"
)]
pub(crate) async fn wake_loop(
    engine: AppWorkflows,
    queue: Queue,
    app: AppId,
    pending: flume::Receiver<()>,
    passes: Rc<Cell<u64>>,
) {
    coalesce(&pending, passes.as_ref(), async || {
        drain(&engine, &queue, &app).await
    })
    .await;
}

/// Run one pass. `true` reports the pass did not finish cleanly, so the wake
/// owes itself another attempt; `false` reports it reached the end of this
/// app's intents.
#[expect(
    clippy::future_not_send,
    reason = "publication runs on the thread that owns the journal handle"
)]
async fn drain(engine: &AppWorkflows, queue: &Queue, app: &AppId) -> bool {
    let publisher = LanePublisher::new(queue, app.clone());
    match compio::time::timeout(
        PUBLICATION_DEADLINE,
        engine.publish_pending_jobs(&publisher),
    )
    .await
    {
        Ok(Ok(())) => false,
        Ok(Err(error)) => {
            tracing::warn!(
                code = error.code(),
                "workflow publication wake left intents to manager reconciliation"
            );
            true
        }
        Err(_) => {
            tracing::warn!(
                "workflow publication wake pass timed out; intents left to manager reconciliation"
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A burst that lands while a pass is running is one follow-up pass, not
    /// one pass per signal.
    ///
    /// This drives [`coalesce`], the loop [`wake_loop`] is, rather than the
    /// channel underneath it: the first signal starts a pass, the 32 sent
    /// during it collapse to the single follow-up, and the loop takes no third.
    #[compio::test]
    async fn a_burst_during_a_pass_is_one_follow_up_pass() {
        const BURST: usize = 32;
        let (sender, receiver) = flume::bounded(1);
        sender.try_send(()).expect("the first signal is unqueued");
        let passes = Cell::new(0);
        let runs = Cell::new(0);
        let mut sender = Some(sender);
        coalesce(&receiver, &passes, async || {
            runs.set(runs.get() + 1);
            if let Some(sender) = sender.take() {
                for _ in 0..BURST {
                    let _ = sender.try_send(());
                }
            }
            false
        })
        .await;
        assert_eq!(
            runs.get(),
            2,
            "one pass for the first signal and one for the whole burst"
        );
        assert_eq!(passes.get(), 2);
    }

    /// A pass that was cut off owes exactly one more, and the pass that follows
    /// it does not re-arm, so a persistent refusal cannot spin.
    #[compio::test]
    async fn a_cut_off_pass_rearms_exactly_once() {
        let (sender, receiver) = flume::bounded(1);
        sender.try_send(()).expect("the first signal is unqueued");
        let passes = Cell::new(0);
        let attempts = Cell::new(0);
        let mut sender = Some(sender);
        coalesce(&receiver, &passes, async || {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 2 {
                drop(sender.take());
            }
            true
        })
        .await;
        assert_eq!(
            attempts.get(),
            2,
            "a cut-off pass schedules one more and the automatic pass schedules none"
        );
        assert_eq!(passes.get(), 2);
    }

    /// A signal whose task has ended is surfaced once, not on every later
    /// commit.
    #[compio::test]
    async fn a_disconnected_wake_is_reported_once() {
        let (sender, receiver) = flume::bounded(1);
        let wake = PublicationWake::new(sender);
        let hint = wake.hint();
        assert!(
            !wake.reported.load(Ordering::Relaxed),
            "a live wake has not reported"
        );
        drop(receiver);
        hint();
        assert!(
            wake.reported.load(Ordering::Relaxed),
            "a signal to an ended task is surfaced"
        );
    }
}
