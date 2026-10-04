//! Monotonic execution deadlines enforced independently of the executor thread.

#[cfg(test)]
mod tests;

use zeroship_workflow::WorkflowServiceError;
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

type Interrupt = Arc<dyn Fn() + Send + Sync>;

/// Why an execution budget stopped authorizing work.
///
/// Expiry is a resource signal: this host ran out of local time, while the
/// delivery that granted the work still stands and the journal still fences
/// every submission against its lease. A delivery whose renewal extends nothing
/// is ended by the runner dropping the attempt, not by this budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BudgetEnd {
    /// The lease or execution deadline passed, or the host finished the
    /// execution itself.
    Expired,
}
impl From<BudgetEnd> for WorkflowServiceError {
    fn from(_: BudgetEnd) -> Self {
        Self::Timeout
    }
}

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

struct Entry {
    deadline: Instant,
    execution_deadline: Instant,
    ended: Option<BudgetEnd>,
    finished: bool,
    interrupt_registered: bool,
    interrupt: Option<Interrupt>,
}
impl Entry {
    /// The first condition to end an execution names it, and hands back the
    /// interrupt to run once the registry lock is released.
    fn cancel(&mut self, end: BudgetEnd) -> Option<Interrupt> {
        if self.ended.is_some() {
            None
        } else {
            self.ended = Some(end);
            self.interrupt.take()
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
    signal: WakeSignal,
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
        loop {
            // Reset before observing state. A notification while the registry is
            // read stays latched until `wait` checks the same signal mutex, so no
            // deadline change is lost.
            self.signal.reset();
            let mut registry = self.registry();
            let now = Instant::now();
            let mut interrupts = Vec::new();
            let mut next: Option<Instant> = None;
            for entry in registry.entries.values_mut().filter(|e| e.ended.is_none()) {
                if entry.deadline <= now {
                    interrupts.extend(entry.cancel(BudgetEnd::Expired));
                } else {
                    next = Some(next.map_or(entry.deadline, |n| n.min(entry.deadline)));
                }
            }
            drop(registry);
            // An interrupt may inspect its budget, so none runs under the lock.
            if !interrupts.is_empty() {
                for interrupt in interrupts {
                    interrupt();
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
    /// Reject work once the lease or execution deadline has expired, or once
    /// the host has finished the execution.
    ///
    /// # Errors
    /// Names the condition that ended the budget.
    pub fn check(&self) -> Result<(), BudgetEnd> {
        self.0.with_entry(|entry| match entry.ended {
            Some(end) => Err(end),
            None if entry.deadline <= Instant::now() => Err(BudgetEnd::Expired),
            None => Ok(()),
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
                .then(|| entry.cancel(BudgetEnd::Expired))
                .flatten();
            if entry.ended.is_some() {
                Ok((true, cancelled))
            } else {
                entry.interrupt = Some(interrupt.clone());
                Ok((false, cancelled))
            }
        })?;
        if let Some(cancelled) = cancelled {
            cancelled();
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
                    ended: None,
                    finished: false,
                    interrupt_registered: false,
                    interrupt: None,
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

    pub(super) fn renew_lease(&self, deadline: Instant) -> Result<(), WorkflowServiceError> {
        let (result, interrupt) = self.budget.0.with_entry(|entry| {
            if entry.finished {
                (Ok(()), None)
            } else if entry.ended.is_some()
                || entry.deadline <= Instant::now()
                || deadline <= Instant::now()
            {
                (
                    Err(WorkflowServiceError::Timeout),
                    entry.cancel(BudgetEnd::Expired),
                )
            } else {
                entry.deadline = deadline.min(entry.execution_deadline);
                (Ok(()), None)
            }
        });
        self.budget.0.watchdog.signal.notify();
        if let Some(interrupt) = interrupt {
            interrupt();
        }
        result
    }

    pub(super) fn finish(&self) {
        let interrupt = self.budget.0.with_entry(|entry| {
            entry.finished = true;
            entry.cancel(BudgetEnd::Expired)
        });
        self.budget.0.watchdog.signal.notify();
        if let Some(interrupt) = interrupt {
            interrupt();
        }
    }
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.finish();
    }
}
