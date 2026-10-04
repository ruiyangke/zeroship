//! Apps prepared on demand when a delivery arrives, kept in a bounded cache.
//!
//! A worker pulls any claimable job in its execution zone, so the app a delivery
//! names is known only once the claim answers. Preparing it is the first thing
//! the delivery's slot does, bounded by the delivery's own remaining lease; the
//! prepared app is kept because its objects are reusable, not to make anything
//! warm.

#![expect(
    clippy::future_not_send,
    reason = "creator resources stay on their owning compio thread"
)]

use crate::TaskExecutor;
use futures::future::LocalBoxFuture;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fmt::Debug,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::app_id::AppId;
use zeroship_workflow::WorkflowServiceError;

/// A lifetime guard for credentials and environment attached to an app.
pub trait Residency: Debug {}
impl<T: Debug> Residency for T {}

/// Creator resources assembled by trusted host configuration.
pub struct CreatorRuntime<J> {
    pub app: J,
    pub executor: Rc<dyn TaskExecutor>,
    pub residency: Rc<dyn Residency>,
}

impl<J: Debug> Debug for CreatorRuntime<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("CreatorRuntime").finish_non_exhaustive()
    }
}

/// Resolve independently authorized creator resources for one app.
pub trait CreatorFactory {
    type Journal: Clone;

    fn open<'a>(
        &'a self,
        app: &'a AppId,
    ) -> LocalBoxFuture<'a, Result<CreatorRuntime<Self::Journal>, WorkflowServiceError>>;
}

/// Whether the host's version feed still lists an app.
///
/// An app the feed omits is deleted. A host without a feed of its own,
/// such as the local host serving its one configured app, lists exactly the apps
/// its factory opens.
pub type AppFeed = Rc<dyn Fn(&AppId) -> bool>;

#[derive(Clone, Copy, Debug)]
pub struct PreparedOptions {
    pub capacity: usize,
    pub operation_timeout: Duration,
}

impl PreparedOptions {
    fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.capacity == 0
            || self.operation_timeout.is_zero()
            || Instant::now()
                .checked_add(self.operation_timeout)
                .is_none()
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid prepared app bounds".into(),
            ));
        }
        Ok(())
    }
}

/// One prepared app. Executions retain this object across map eviction.
pub struct Prepared<J> {
    app_id: AppId,
    journal: J,
    executor: Rc<dyn TaskExecutor>,
    _residency: Rc<dyn Residency>,
    used: Cell<u64>,
}

impl<J> Debug for Prepared<J> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Prepared")
            .field("app_id", &self.app_id.as_str())
            .finish_non_exhaustive()
    }
}

impl<J> Prepared<J> {
    #[must_use]
    pub const fn app_id(&self) -> &AppId {
        &self.app_id
    }

    #[must_use]
    pub const fn journal(&self) -> &J {
        &self.journal
    }

    #[must_use]
    pub fn executor(&self) -> Rc<dyn TaskExecutor> {
        self.executor.clone()
    }
}

/// A host-thread LRU whose misses are bounded by the delivery that asked.
pub struct PreparedApps<F: CreatorFactory> {
    factory: F,
    feed: AppFeed,
    options: PreparedOptions,
    entries: RefCell<BTreeMap<AppId, Rc<Prepared<F::Journal>>>>,
    clock: Cell<u64>,
}

impl<F: CreatorFactory> Debug for PreparedApps<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedApps")
            .field("options", &self.options)
            .field("prepared", &self.entries.borrow().len())
            .finish_non_exhaustive()
    }
}

impl<F: CreatorFactory> PreparedApps<F> {
    /// # Errors
    /// Refuses an empty capacity and an empty or unrepresentable operation bound.
    pub fn new(
        factory: F,
        feed: AppFeed,
        options: PreparedOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            factory,
            feed,
            options,
            entries: RefCell::new(BTreeMap::new()),
            clock: Cell::new(0),
        })
    }

    /// The prepared entry for `app`, opening it on a miss.
    ///
    /// A MISS RUNS UNDER `remaining`, the delivery's own remaining lease, and
    /// never longer than the host's operation bound. A preparation that outlives
    /// it is dropped, so the factory's pending work is cancelled and nothing is
    /// cached for the app.
    ///
    /// # Errors
    /// Refuses with `Timeout` when the bound ends first, with
    /// `ResourceExhausted` when every cached entry is executing, and passes the
    /// factory's own refusals through.
    pub async fn get_or_prepare(
        &self,
        app: &AppId,
        remaining: Duration,
    ) -> Result<Rc<Prepared<F::Journal>>, WorkflowServiceError> {
        let used = self.clock.get().checked_add(1).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("prepared app clock exhausted".into())
        })?;
        self.clock.set(used);
        if let Some(entry) = self.entries.borrow().get(app).cloned() {
            entry.used.set(used);
            return Ok(entry);
        }
        let bound = remaining.min(self.options.operation_timeout);
        if bound.is_zero() {
            return Err(WorkflowServiceError::Timeout);
        }
        let runtime = compio::time::timeout(bound, self.factory.open(app))
            .await
            .map_err(|_| WorkflowServiceError::Timeout)??;
        let entry = Rc::new(Prepared {
            app_id: app.clone(),
            journal: runtime.app,
            executor: runtime.executor,
            _residency: runtime.residency,
            used: Cell::new(used),
        });
        let mut entries = self.entries.borrow_mut();
        // ONLY AN IDLE ENTRY IS EVICTED. An entry an execution holds is shared
        // with that execution, so evicting it would free nothing: the execution's
        // reference keeps its residency until it finishes either way.
        while entries.len() >= self.options.capacity {
            let candidate = entries
                .iter()
                .filter(|(_, held)| Rc::strong_count(held) == 1)
                .min_by_key(|(_, held)| held.used.get())
                .map(|(app, _)| app.clone())
                .ok_or_else(|| {
                    WorkflowServiceError::ResourceExhausted(
                        "every prepared app is executing".into(),
                    )
                })?;
            entries.remove(&candidate);
        }
        entries.insert(app.clone(), entry.clone());
        Ok(entry)
    }

    /// Drop every entry whose app is absent from the version feed.
    ///
    /// The map's reference goes even while an execution holds the entry: that
    /// execution keeps its own reference, and with it the app's residency, until
    /// it finishes.
    pub fn prune(&self) {
        let feed = &self.feed;
        self.entries.borrow_mut().retain(|app, _| feed(app));
    }
}

#[cfg(test)]
mod tests;
