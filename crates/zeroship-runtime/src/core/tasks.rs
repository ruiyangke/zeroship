//! Cancellation and join boundary for native work owned by an isolate.

use futures::{
    channel::oneshot,
    future::{AbortHandle, Abortable},
};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

#[derive(Clone, Default)]
pub(crate) struct RuntimeTasks(Rc<Inner>);

#[derive(Default)]
struct Inner {
    closed: Cell<bool>,
    next: Cell<usize>,
    active: Cell<usize>,
    roots: RefCell<HashMap<usize, AbortHandle>>,
    waiters: RefCell<Vec<oneshot::Sender<()>>>,
}

impl RuntimeTasks {
    pub(crate) fn spawn(&self, future: impl Future<Output = ()> + 'static) {
        if self.0.closed.get() {
            return;
        }
        let (abort, registration) = AbortHandle::new_pair();
        let tracked = self.track(Abortable::new(future, registration));
        self.0.roots.borrow_mut().insert(tracked.guard.id, abort);
        compio::runtime::spawn(tracked).detach();
    }

    /// Child tasks retain their existing join handles. Their parent's teardown
    /// cancels those handles; tracking also waits for child destruction.
    pub(crate) fn track<F: Future>(&self, future: F) -> Tracked<F> {
        assert!(
            !self.0.closed.get(),
            "cannot spawn a child after isolate quarantine"
        );
        let id = self.0.next.get();
        self.0
            .next
            .set(id.checked_add(1).expect("native task identity exhausted"));
        self.0.active.set(
            self.0
                .active
                .get()
                .checked_add(1)
                .expect("native task capacity exhausted"),
        );
        Tracked {
            future: Box::pin(future),
            guard: TaskGuard {
                owner: self.clone(),
                id,
            },
        }
    }

    pub(crate) fn cancel(&self) -> bool {
        if self.0.closed.replace(true) {
            return false;
        }
        for (_, task) in self.0.roots.take() {
            task.abort();
        }
        true
    }

    pub(crate) fn is_idle(&self) -> bool {
        self.0.active.get() == 0
    }

    pub(crate) async fn join(&self) {
        if self.0.active.get() != 0 {
            let (sender, receiver) = oneshot::channel();
            self.0.waiters.borrow_mut().push(sender);
            let _ = receiver.await;
        }
    }
}

pub(crate) struct Tracked<F> {
    // Drop the future (including its native resources) before notifying joins.
    future: Pin<Box<F>>,
    guard: TaskGuard,
}
impl<F: Future> Future for Tracked<F> {
    type Output = F::Output;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}
struct TaskGuard {
    owner: RuntimeTasks,
    id: usize,
}
impl Drop for TaskGuard {
    fn drop(&mut self) {
        let inner = &self.owner.0;
        inner.roots.borrow_mut().remove(&self.id);
        let remaining = inner.active.get() - 1;
        inner.active.set(remaining);
        if remaining == 0 {
            for waiter in inner.waiters.take() {
                let _ = waiter.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    struct Dropped(Rc<Cell<bool>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[compio::test]
    async fn quarantine_joins_root_and_child_destruction() {
        let tasks = RuntimeTasks::default();
        let root = Rc::new(Cell::new(false));
        let child = Rc::new(Cell::new(false));
        let started = Rc::new(Cell::new(false));
        let nested = tasks.clone();
        let root_drop = Dropped(root.clone());
        let child_drop = Dropped(child.clone());
        let ready = started.clone();
        tasks.spawn(async move {
            let _root = root_drop;
            let child = compio::runtime::spawn(nested.track(async move {
                let _child = child_drop;
                ready.set(true);
                std::future::pending::<()>().await;
            }));
            let _ = child.await;
        });
        while !started.get() {
            compio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        tasks.cancel();
        // A cancelled waiter must not consume the join notification needed by
        // another caller resuming shutdown.
        let _ = tasks.join().now_or_never();
        tasks.join().await;
        assert!(root.get());
        assert!(child.get());
        let ran = Rc::new(Cell::new(false));
        let rejected = ran.clone();
        tasks.spawn(async move {
            rejected.set(true);
        });
        tasks.join().await;
        assert!(!ran.get());
    }
}
