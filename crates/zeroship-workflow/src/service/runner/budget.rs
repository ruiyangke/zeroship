//! Monotonic execution deadlines enforced independently of the executor thread.

#[cfg(test)]
mod tests;

use crate::WorkflowServiceError;
use futures::{
    future::{BoxFuture, Shared},
    FutureExt,
};
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    task::{Context, Wake, Waker},
    time::{Duration, Instant},
};

type Interrupt = Arc<dyn Fn() + Send + Sync>;
type Cancellation = Shared<BoxFuture<'static, ()>>;

#[derive(Default)]
struct WakeSignal {
    notified: Mutex<bool>,
    changed: Condvar,
}
impl WakeSignal {
    fn notify(&self) {
        *self.notified.lock().expect("workflow deadline signal") = true;
        self.changed.notify_one();
    }

    fn reset(&self) {
        *self.notified.lock().expect("workflow deadline signal") = false;
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "the notification lock must reach the condition-variable wait without a lost wake"
    )]
    fn wait(&self, deadline: Option<Instant>) {
        let notified = self.notified.lock().expect("workflow deadline signal");
        match deadline {
            Some(deadline) => {
                drop(
                    self.changed
                        .wait_timeout_while(
                            notified,
                            deadline.saturating_duration_since(Instant::now()),
                            |notified| !*notified,
                        )
                        .expect("workflow deadline wait"),
                );
            }
            None => {
                drop(
                    self.changed
                        .wait_while(notified, |notified| !*notified)
                        .expect("workflow deadline wait"),
                );
            }
        }
    }
}
impl Wake for WakeSignal {
    fn wake(self: Arc<Self>) {
        self.notify();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
    }
}

struct Cancelled {
    interrupt: Option<Interrupt>,
    cancellation: Option<Cancellation>,
}
impl Cancelled {
    fn finish(self) {
        drop(self.cancellation);
        if let Some(interrupt) = self.interrupt {
            interrupt();
        }
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "cancellation, completion and callback registration are independent watchdog state"
)]
struct Entry {
    deadline: Instant,
    execution_deadline: Instant,
    cancelled: bool,
    finished: bool,
    interrupt_registered: bool,
    interrupt: Option<Interrupt>,
    cancellation_registered: bool,
    cancellation: Option<Cancellation>,
}
impl Entry {
    fn cancel(&mut self) -> Option<Cancelled> {
        if std::mem::replace(&mut self.cancelled, true) {
            None
        } else {
            Some(Cancelled {
                interrupt: self.interrupt.take(),
                cancellation: self.cancellation.take(),
            })
        }
    }
}
#[derive(Default)]
struct Registry {
    next: u64,
    entries: HashMap<u64, Entry>,
}
#[derive(Default)]
struct Watchdog {
    registry: Mutex<Registry>,
    signal: Arc<WakeSignal>,
}
impl Watchdog {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().expect("workflow deadline registry")
    }

    fn shared() -> Result<&'static Arc<Self>, WorkflowServiceError> {
        static SHARED: OnceLock<Result<Arc<Watchdog>, String>> = OnceLock::new();
        SHARED
            .get_or_init(|| {
                let watchdog = Arc::new(Self::default());
                let task = watchdog.clone();
                std::thread::Builder::new()
                    .name("workflow-deadlines".into())
                    .spawn(move || task.run())
                    .map_err(|e| format!("start workflow deadline watchdog: {e}"))?;
                Ok(watchdog)
            })
            .as_ref()
            .map_err(|e| WorkflowServiceError::Unavailable(e.clone()))
    }

    fn run(&self) {
        let waker = Waker::from(self.signal.clone());
        loop {
            // Reset before observing state. Wakes during polling remain latched
            // until wait checks the same signal mutex, so no readiness is lost.
            self.signal.reset();
            let mut registry = self.registry();
            let now = Instant::now();
            let mut interrupts = Vec::new();
            let mut cancellations = Vec::new();
            let mut next: Option<Instant> = None;
            for (id, entry) in registry.entries.iter_mut().filter(|(_, e)| !e.cancelled) {
                if entry.deadline <= now {
                    interrupts.extend(entry.cancel());
                } else {
                    next = Some(next.map_or(entry.deadline, |n| n.min(entry.deadline)));
                    if let Some(cancellation) = entry.cancellation.take() {
                        cancellations.push((*id, cancellation));
                    }
                }
            }
            drop(registry);
            // A cancellation future may wake immediately or inspect its budget.
            // Neither polling nor dropping it may run under the registry lock.
            for (id, mut cancellation) in cancellations {
                let ready = cancellation
                    .poll_unpin(&mut Context::from_waker(&waker))
                    .is_ready();
                let mut cancellation = Some(cancellation);
                let cancelled = {
                    let mut registry = self.registry();
                    match registry.entries.get_mut(&id) {
                        Some(entry) if ready => entry.cancel(),
                        Some(entry) if !entry.cancelled => {
                            // Retain the polled Shared handle itself: dropping
                            // a temporary clone would unregister its waker.
                            entry.cancellation = cancellation.take();
                            None
                        }
                        _ => None,
                    }
                };
                drop(cancellation);
                interrupts.extend(cancelled);
            }
            if !interrupts.is_empty() {
                for interrupt in interrupts {
                    interrupt.finish();
                }
                continue;
            }
            self.signal.wait(next);
        }
    }
}

struct Registration {
    id: u64,
    watchdog: Arc<Watchdog>,
}
impl Registration {
    fn with_entry<T>(&self, operation: impl FnOnce(&mut Entry) -> T) -> T {
        let mut registry = self.watchdog.registry();
        let result = operation(
            registry
                .entries
                .get_mut(&self.id)
                .expect("registered execution"),
        );
        drop(registry);
        result
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let entry = self
            .watchdog
            .registry
            .lock()
            .expect("workflow deadline registry")
            .entries
            .remove(&self.id);
        drop(entry);
        self.watchdog.signal.notify();
    }
}

/// Read-only execution authority supplied by the runner. The trusted executor
/// may install a thread-safe interrupt, but cannot extend the deadline.
#[derive(Clone)]
pub struct ExecutionBudget(Arc<Registration>);
impl std::fmt::Debug for ExecutionBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionBudget").finish_non_exhaustive()
    }
}
impl ExecutionBudget {
    /// Reject work once the lease or execution deadline has expired.
    ///
    /// # Errors
    /// Returns a timeout after cancellation or expiry.
    pub fn check(&self) -> Result<(), WorkflowServiceError> {
        self.0.with_entry(|entry| {
            if entry.cancelled || entry.deadline <= Instant::now() {
                Err(WorkflowServiceError::Timeout)
            } else {
                Ok(())
            }
        })
    }

    /// Install the executor's interrupt before entering app code. It runs on
    /// the watchdog or calling thread and must be nonblocking, infallible and
    /// independent of the executor thread. Registration after expiry interrupts
    /// immediately; cancellation cannot be lost while code is loading.
    ///
    /// # Errors
    /// Rejects replacement of an already installed interrupt.
    pub fn on_interrupt(
        &self,
        interrupt: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), WorkflowServiceError> {
        let interrupt: Interrupt = Arc::new(interrupt);
        let (fire, cancelled) = self.0.with_entry(|entry| {
            if entry.interrupt_registered {
                return Err(WorkflowServiceError::Conflict(
                    "execution interrupt already installed".into(),
                ));
            }
            entry.interrupt_registered = true;
            let cancelled = (entry.deadline <= Instant::now())
                .then(|| entry.cancel())
                .flatten();
            if entry.cancelled {
                Ok((true, cancelled))
            } else {
                entry.interrupt = Some(interrupt.clone());
                Ok((false, cancelled))
            }
        })?;
        if let Some(cancelled) = cancelled {
            cancelled.finish();
        }
        if fire {
            interrupt();
        }
        Ok(())
    }
}

/// Owns a local execution deadline.
///
/// Dropping the guard interrupts remaining execution. Distributed runners constrain it to a confirmed
/// lease; executor code receives only its read-only budget.
#[derive(Debug)]
pub struct ExecutionGuard {
    budget: ExecutionBudget,
}
impl ExecutionGuard {
    /// # Errors
    /// Rejects an empty or overflowing timeout and watchdog startup failure.
    pub fn new(timeout: Duration) -> Result<Self, WorkflowServiceError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .filter(|_| !timeout.is_zero())
            .ok_or_else(|| {
                WorkflowServiceError::InvalidRequest("invalid execution timeout".into())
            })?;
        let watchdog = Watchdog::shared()?.clone();
        let id = {
            let mut registry = watchdog.registry();
            let id = registry.next;
            registry.next = id.checked_add(1).ok_or_else(|| {
                WorkflowServiceError::Unavailable("execution deadline identifiers exhausted".into())
            })?;
            registry.entries.insert(
                id,
                Entry {
                    deadline,
                    execution_deadline: deadline,
                    cancelled: false,
                    finished: false,
                    interrupt_registered: false,
                    interrupt: None,
                    cancellation_registered: false,
                    cancellation: None,
                },
            );
            id
        };
        watchdog.signal.notify();
        Ok(Self {
            budget: ExecutionBudget(Arc::new(Registration { id, watchdog })),
        })
    }

    #[must_use]
    pub fn budget(&self) -> ExecutionBudget {
        self.budget.clone()
    }

    /// Attach the captured host authority's cancellation signal before loading.
    /// The signal must be nonblocking and independent of an async runtime; the
    /// existing watchdog polls it even while creator code blocks its own thread.
    ///
    /// # Errors
    /// Rejects replacement of an already installed cancellation source.
    pub(super) fn cancel_on(&self, cancellation: Cancellation) -> Result<(), WorkflowServiceError> {
        let mut cancellation = Some(cancellation);
        self.budget.0.with_entry(|entry| {
            if entry.cancellation_registered {
                return Err(WorkflowServiceError::Conflict(
                    "execution cancellation already installed".into(),
                ));
            }
            entry.cancellation_registered = true;
            if !entry.cancelled {
                entry.cancellation = cancellation.take();
            }
            Ok(())
        })?;
        self.budget.0.watchdog.signal.notify();
        Ok(())
    }

    pub(super) fn renew_lease(&self, deadline: Instant) -> Result<(), WorkflowServiceError> {
        let (result, interrupt) = self.budget.0.with_entry(|entry| {
            if entry.finished {
                (Ok(()), None)
            } else if entry.cancelled
                || entry.deadline <= Instant::now()
                || deadline <= Instant::now()
            {
                (Err(WorkflowServiceError::Timeout), entry.cancel())
            } else {
                entry.deadline = deadline.min(entry.execution_deadline);
                (Ok(()), None)
            }
        });
        self.budget.0.watchdog.signal.notify();
        if let Some(interrupt) = interrupt {
            interrupt.finish();
        }
        result
    }

    pub(super) fn finish(&self) {
        let interrupt = self.budget.0.with_entry(|entry| {
            entry.finished = true;
            entry.cancel()
        });
        self.budget.0.watchdog.signal.notify();
        if let Some(interrupt) = interrupt {
            interrupt.finish();
        }
    }
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.finish();
    }
}
