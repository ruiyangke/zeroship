//! Reusable query captures, entered only while synchronous work is running.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use super::ReadSetEntry;

#[derive(Debug)]
struct Buffer {
    recording: bool,
    entries: Vec<ReadSetEntry>,
}

/// A query's dependency buffer. The host owns its lifetime and may resume it
/// across asynchronous work without keeping an ambient guard across await.
#[derive(Clone, Debug)]
pub struct Capture(Rc<RefCell<Buffer>>);

thread_local! {
    static CURRENT: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

struct Entered {
    previous: Option<Capture>,
}

impl Drop for Entered {
    fn drop(&mut self) {
        CURRENT.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

impl Capture {
    /// `recording` is chosen by the host from the invoking procedure's kind.
    #[must_use]
    pub fn new(recording: bool) -> Self {
        Self(Rc::new(RefCell::new(Buffer {
            recording,
            entries: Vec::new(),
        })))
    }

    /// Run planning or other synchronous work with this capture active.
    /// Nested calls and panic unwinding restore the caller's capture.
    pub fn with<R>(&self, body: impl FnOnce() -> R) -> R {
        let previous = CURRENT.with(|slot| slot.replace(Some(self.clone())));
        let _entered = Entered { previous };
        body()
    }

    /// Poll queued native work under its captured query. No ambient capture
    /// remains installed while the future is pending or after cancellation.
    #[allow(clippy::future_not_send, reason = "Captures and queued operations run on their owning compio thread")]
    pub async fn with_future<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        std::future::poll_fn(|cx| self.with(|| future.as_mut().poll(cx))).await
    }

    /// Clone the dependencies for a collection without draining the buffer.
    /// A handler may attach the same capture to several subscriptions.
    #[must_use]
    pub fn snapshot_for(&self, collection: &str) -> Vec<ReadSetEntry> {
        self.0
            .borrow()
            .entries
            .iter()
            .filter(|entry| entry.collection == collection)
            .cloned()
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> Vec<ReadSetEntry> {
        self.0.borrow().entries.clone()
    }
}

/// Whether the current synchronous operation has a capture, including an inert
/// capture selected by a host that is running a non-query operation.
#[must_use]
pub fn is_active() -> bool {
    CURRENT.with(|slot| slot.borrow().is_some())
}

pub(super) fn is_recording() -> bool {
    CURRENT.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|capture| capture.0.borrow().recording)
    })
}

pub(super) fn record(entry: ReadSetEntry) {
    CURRENT.with(|slot| {
        if let Some(capture) = slot.borrow().as_ref() {
            let mut buffer = capture.0.borrow_mut();
            if buffer.recording {
                buffer.entries.push(entry);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdc::read_set::record_if_active;
    use crate::schema::FieldMap;
    use crate::value;

    fn read(collection: &str, id: &str) {
        record_if_active(collection, &value!({"id":id}), &FieldMap::new());
    }

    #[test]
    fn interleaved_captures_keep_their_own_reads() {
        let first = Capture::new(true);
        let second = Capture::new(true);
        first.with(|| read("messages", "first"));
        second.with(|| read("messages", "second"));
        first.with(|| read("messages", "resumed"));
        let first_rows = first.snapshot_for("messages");
        let second_rows = second.snapshot_for("messages");
        assert_eq!(first_rows.len(), 2);
        assert_eq!(second_rows.len(), 1);
        assert!(first_rows[0].matches(&std::collections::HashMap::from([(
            "id".into(),
            "first".into()
        )])));
        assert!(!second_rows[0].matches(&std::collections::HashMap::from([(
            "id".into(),
            "first".into()
        )])));
        assert!(!is_active());
    }

    #[test]
    fn nested_captures_restore_the_parent() {
        let outer = Capture::new(true);
        let inner = Capture::new(true);
        outer.with(|| {
            read("messages", "outer-before");
            inner.with(|| read("messages", "inner"));
            read("messages", "outer-after");
        });
        assert_eq!(outer.snapshot_for("messages").len(), 2);
        assert_eq!(inner.snapshot_for("messages").len(), 1);
        assert!(!is_active());
    }

    #[test]
    fn panic_restores_the_parent_capture() {
        let outer = Capture::new(true);
        let inner = Capture::new(true);
        outer.with(|| {
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                inner.with(|| {
                    read("inner", "before-panic");
                    panic!("capture unwind regression");
                });
            }));
            assert!(panic.is_err());
            read("outer", "after-panic");
        });
        assert_eq!(outer.snapshot_for("outer").len(), 1);
        assert!(outer.snapshot_for("inner").is_empty());
        assert_eq!(inner.snapshot_for("inner").len(), 1);
        assert!(!is_active());
    }

    #[test]
    fn inert_and_missing_captures_do_not_record() {
        let capture = Capture::new(false);
        capture.with(|| {
            assert!(is_active());
            read("messages", "inert");
        });
        read("messages", "outside");
        assert!(capture.snapshot_for("messages").is_empty());
        assert!(!is_active());
    }

    #[test]
    fn snapshots_clone_and_filter_without_consuming_reads() {
        let capture = Capture::new(true);
        capture.with(|| {
            read("messages", "message");
            read("todos", "todo");
        });
        let first = capture.snapshot_for("messages");
        assert_eq!(first.len(), 1);
        assert_eq!(capture.snapshot_for("messages"), first);
        assert_eq!(capture.snapshot_for("todos").len(), 1);
        assert!(capture.snapshot_for("unread").is_empty());
    }

    #[test]
    fn pending_native_work_and_cancellation_restore_the_host_capture() {
        use std::task::{Context, Poll, Waker};
        let host = Capture::new(true);
        let query = Capture::new(true);
        let mut polls = 0;
        let operation = std::future::poll_fn(|_| {
            polls += 1;
            read("query", "poll");
            if polls == 1 {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        });
        let mut future = Box::pin(query.with_future(operation));
        let mut context = Context::from_waker(Waker::noop());
        host.with(|| {
            assert!(future.as_mut().poll(&mut context).is_pending());
            read("host", "while-pending");
            assert!(future.as_mut().poll(&mut context).is_ready());
            read("host", "after-ready");
        });
        assert_eq!(query.snapshot_for("query").len(), 2);
        assert!(query.snapshot_for("host").is_empty());
        assert_eq!(host.snapshot_for("host").len(), 2);
        assert!(host.snapshot_for("query").is_empty());

        let pending = std::future::poll_fn(|_| {
            read("cancelled", "before-cancel");
            Poll::<()>::Pending
        });
        let mut cancelled = Box::pin(query.with_future(pending));
        host.with(|| {
            assert!(cancelled.as_mut().poll(&mut context).is_pending());
            drop(cancelled);
            read("host", "after-cancel");
        });
        assert_eq!(query.snapshot_for("cancelled").len(), 1);
        assert_eq!(host.snapshot_for("host").len(), 3);
        assert!(host.snapshot_for("cancelled").is_empty());
        assert!(!is_active());
    }
}
