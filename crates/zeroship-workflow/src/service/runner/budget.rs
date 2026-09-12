//! Monotonic execution deadlines enforced independently of the executor thread.

#[cfg(test)]
mod tests;

use crate::WorkflowServiceError;
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

type Interrupt = Arc<dyn Fn() + Send + Sync>;

struct Entry {
    deadline: Instant,
    execution_deadline: Instant,
    cancelled: bool,
    finished: bool,
    interrupt_registered: bool,
    interrupt: Option<Interrupt>,
}
impl Entry {
    fn cancel(&mut self) -> Option<Interrupt> {
        if std::mem::replace(&mut self.cancelled, true) {
            None
        } else {
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
    changed: Condvar,
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
        let mut registry = self.registry.lock().expect("workflow deadline registry");
        loop {
            let now = Instant::now();
            let mut interrupts = Vec::new();
            let mut next: Option<Instant> = None;
            for entry in registry.entries.values_mut().filter(|e| !e.cancelled) {
                if entry.deadline <= now {
                    interrupts.extend(entry.cancel());
                } else {
                    next = Some(next.map_or(entry.deadline, |n| n.min(entry.deadline)));
                }
            }
            if !interrupts.is_empty() {
                drop(registry);
                for interrupt in interrupts {
                    interrupt();
                }
                registry = self.registry.lock().expect("workflow deadline registry");
                continue;
            }
            registry = match next {
                Some(next) => {
                    self.changed
                        .wait_timeout(registry, next.saturating_duration_since(Instant::now()))
                        .expect("workflow deadline wait")
                        .0
                }
                None => self.changed.wait(registry).expect("workflow deadline wait"),
            };
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
        self.watchdog
            .registry
            .lock()
            .expect("workflow deadline registry")
            .entries
            .remove(&self.id);
        self.watchdog.changed.notify_one();
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
        let fire = self.0.with_entry(|entry| {
            if entry.interrupt_registered {
                return Err(WorkflowServiceError::Conflict(
                    "execution interrupt already installed".into(),
                ));
            }
            entry.interrupt_registered = true;
            if entry.deadline <= Instant::now() {
                entry.cancelled = true;
            }
            if entry.cancelled {
                Ok(true)
            } else {
                entry.interrupt = Some(interrupt.clone());
                Ok(false)
            }
        })?;
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
                },
            );
            id
        };
        watchdog.changed.notify_one();
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
        self.budget.0.watchdog.changed.notify_one();
        if let Some(interrupt) = interrupt {
            interrupt();
        }
        result
    }

    pub(super) fn finish(&self) {
        let interrupt = self.budget.0.with_entry(|entry| {
            entry.finished = true;
            entry.cancel()
        });
        self.budget.0.watchdog.changed.notify_one();
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
