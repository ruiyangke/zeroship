//! Bind manager placements to independently authorized creator resources.

#![expect(
    clippy::future_not_send,
    reason = "creator bindings and metadata clients stay on their compio thread"
)]

use super::{
    consumer::{ConsumerBindings, ConsumerScope},
    delivery::bounded,
    TaskExecutor,
};
use crate::{
    service::{AppWorkflows, AssignedPolicies, HostPolicies, PolicyBinding},
    WorkflowServiceError,
};
use futures::{
    channel::oneshot,
    future::{Either, LocalBoxFuture, Shared},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, Assignment, ScopePage},
};
use zeroship_workflow_client::WorkerCoordinator;

/// Creator I/O and execution assembled by trusted deployment-host configuration.
#[derive(Clone)]
pub struct CreatorRuntime {
    pub app: AppWorkflows,
    pub executor: Rc<dyn TaskExecutor>,
}

impl std::fmt::Debug for CreatorRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreatorRuntime")
            .field("app", self.app.app_id())
            .finish_non_exhaustive()
    }
}

/// Resolve only resources this process is independently authorized to access.
/// Placement metadata supplies neither database credentials nor schema authority.
pub trait CreatorFactory {
    /// Use the supplied policy generation for the returned app. Opening must be
    /// cancellation safe: dropping this future must stop or quarantine its I/O.
    fn open(
        &self,
        scope: &AssignedScope,
        policy: &PolicyBinding,
    ) -> impl Future<Output = Result<CreatorRuntime, WorkflowServiceError>>;
}

/// Active placement and per-operation bounds. Policy grants retain their own
/// original monotonic deadlines throughout creator preparation.
#[derive(Clone, Copy, Debug)]
pub struct AssignmentOptions {
    pub max_scopes: usize,
    pub operation_timeout: Duration,
}

struct Ready {
    runtime: CreatorRuntime,
    consumer: ConsumerScope,
}

struct Entry {
    policies: AssignedPolicies,
    ready: RefCell<Option<Ready>>,
    busy: Cell<bool>,
    stop: RefCell<Option<oneshot::Sender<()>>>,
    stopped: Shared<LocalBoxFuture<'static, ()>>,
}

impl Entry {
    fn new(policies: AssignedPolicies) -> Self {
        let (stop, stopped) = oneshot::channel();
        Self {
            policies,
            ready: RefCell::new(None),
            busy: Cell::new(false),
            stop: RefCell::new(Some(stop)),
            stopped: async {
                let _ = stopped.await;
            }
            .boxed_local()
            .shared(),
        }
    }

    fn retire(&self) -> Result<(), WorkflowServiceError> {
        if let Some(stop) = self.stop.borrow_mut().take() {
            let _ = stop.send(());
        }
        self.policies.binding().revoke()
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        let _ = self.retire();
    }
}

struct Busy<'a>(&'a Cell<bool>);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// Owns the consumer's complete binding snapshot for a fixed enrolled signer.
///
/// Registration and periodic invocation belong to the host; scheduling and
/// durable recovery responsibility remain with the manager.
///
/// Use this as the sole writer of the supplied `ConsumerBindings`. Closing or
/// dropping it revokes admission synchronously; the host must separately await
/// the consumer's joined drain before discarding execution capacity.
pub struct AssignmentBindings<F> {
    client: WorkerCoordinator,
    policies: Arc<HostPolicies>,
    consumer: ConsumerBindings,
    factory: F,
    options: AssignmentOptions,
    entries: RefCell<BTreeMap<AppId, Rc<Entry>>>,
    scan: Cell<u64>,
    closed: Cell<bool>,
}

impl<F> std::fmt::Debug for AssignmentBindings<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssignmentBindings")
            .field("worker", self.client.worker_id())
            .field("options", &self.options)
            .field("closed", &self.closed.get())
            .finish_non_exhaustive()
    }
}

impl<F> AssignmentBindings<F> {
    /// # Errors
    /// Rejects empty bounds, limits exceeding the consumer's capacity, and
    /// durations that cannot be represented by the local monotonic clock.
    pub fn new(
        client: WorkerCoordinator,
        policies: Arc<HostPolicies>,
        consumer: ConsumerBindings,
        factory: F,
        options: AssignmentOptions,
    ) -> Result<Self, WorkflowServiceError> {
        if options.max_scopes == 0
            || options.max_scopes > consumer.limit()
            || options.operation_timeout.is_zero()
            || Instant::now()
                .checked_add(options.operation_timeout)
                .is_none()
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow assignment bounds".into(),
            ));
        }
        Ok(Self {
            client,
            policies,
            consumer,
            factory,
            options,
            entries: RefCell::new(BTreeMap::new()),
            scan: Cell::new(0),
            closed: Cell::new(false),
        })
    }

    /// Permanently close this host's bindings, including pending preparation.
    /// No network exchange or assignment release is needed to stop local work.
    ///
    /// # Errors
    /// Reports failed policy revocation after attempting to retire every entry.
    pub fn close(&self) -> Result<(), WorkflowServiceError> {
        self.closed.set(true);
        let entries = std::mem::take(&mut *self.entries.borrow_mut());
        let mut result = self.consumer.replace(Vec::new());
        for entry in entries.values() {
            let retired = entry.retire();
            if result.is_ok() {
                result = retired;
            }
        }
        result
    }

    fn current(&self, entry: &Rc<Entry>) -> Result<(), WorkflowServiceError> {
        if self.closed.get()
            || !self
                .entries
                .borrow()
                .get(&entry.policies.scope().app_id)
                .is_some_and(|current| Rc::ptr_eq(current, entry))
        {
            return Err(retired());
        }
        Ok(())
    }

    fn publish(&self) -> Result<(), WorkflowServiceError> {
        if self.closed.get() {
            return Err(retired());
        }
        self.consumer.replace(
            self.entries
                .borrow()
                .values()
                .filter_map(|entry| {
                    entry
                        .ready
                        .borrow()
                        .as_ref()
                        .map(|ready| ready.consumer.clone())
                })
                .collect(),
        )
    }
}

impl<F> Drop for AssignmentBindings<F> {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

impl<F: CreatorFactory> AssignmentBindings<F> {
    /// Scan to an empty page before applying any placement changes. Failed or
    /// superseded scans preserve the installed snapshot and original grants.
    /// A complete scan retires removals before any replacement preparation I/O.
    ///
    /// # Errors
    /// Rejects unavailable, oversized or superseded scans and failed preparation.
    /// Successful apps remain installed when another app fails preparation.
    pub async fn reconcile(&self) -> Result<(), WorkflowServiceError> {
        if self.closed.get() {
            return Err(retired());
        }
        let ticket = self.scan.get().checked_add(1).ok_or_else(retired)?;
        self.scan.set(ticket);
        let assignments = bounded(self.options.operation_timeout, self.scan_assignments()).await?;
        if self.closed.get() || self.scan.get() != ticket {
            return Err(retired());
        }
        let result = {
            let mut entries = self.entries.borrow_mut();
            let removed: Vec<_> = entries
                .iter()
                .filter(|(app, entry)| {
                    assignments.get(*app).is_none_or(|assignment| {
                        assignment.revision != entry.policies.scope().assignment_revision
                    })
                })
                .map(|(app, _)| app.clone())
                .collect();
            let mut result = Ok(());
            for app in removed {
                if let Some(entry) = entries.remove(&app) {
                    let retired = entry.retire();
                    if result.is_ok() {
                        result = retired;
                    }
                }
            }
            result
        };
        // Publish removals even if allocating a replacement generation fails.
        self.publish()?;
        result?;
        for (app, assignment) in assignments {
            if !self.entries.borrow().contains_key(&app) {
                let policies = AssignedPolicies::new(
                    &self.policies,
                    self.client.clone(),
                    AssignedScope {
                        app_id: app.clone(),
                        assignment_revision: assignment.revision,
                    },
                )?;
                self.entries
                    .borrow_mut()
                    .insert(app, Rc::new(Entry::new(policies)));
            }
        }
        self.refresh().await
    }

    async fn scan_assignments(&self) -> Result<BTreeMap<AppId, Assignment>, WorkflowServiceError> {
        let mut assignments = BTreeMap::new();
        let mut page = ScopePage { after: None };
        loop {
            let rows = self
                .client
                .assignments(&page)
                .await
                .map_err(transport_error)?;
            if rows.is_empty() {
                return Ok(assignments);
            }
            if rows.len() > self.options.max_scopes.saturating_sub(assignments.len()) {
                return Err(WorkflowServiceError::ResourceExhausted(
                    "workflow assignment capacity exceeded".into(),
                ));
            }
            // The client validates strict ordering and exact worker identity.
            // Assignment timestamps are manager metadata, never local authority.
            for assignment in rows {
                page.after = Some(assignment.app_id.clone());
                assignments.insert(assignment.app_id.clone(), assignment);
            }
        }
    }

    /// Renew installed associations independently. Slow creator setup cannot
    /// prevent other apps from refreshing; each entry has its own operation bound.
    /// Failed refreshes retain only the previous grant's original deadline.
    ///
    /// # Errors
    /// Reports closed bindings and renewal/preparation failures after progressing
    /// every available entry. A concurrent caller reports pending entries as
    /// unavailable instead of duplicating their I/O or claiming they are ready.
    pub async fn refresh(&self) -> Result<(), WorkflowServiceError> {
        if self.closed.get() {
            return Err(retired());
        }
        let entries: Vec<_> = self.entries.borrow().values().cloned().collect();
        let mut pending: FuturesUnordered<_> = entries
            .iter()
            .map(|entry| self.refresh_entry(entry))
            .collect();
        let mut result = Ok(());
        while let Some(refreshed) = pending.next().await {
            if result.is_ok() {
                result = refreshed;
            }
        }
        result
    }

    async fn refresh_entry(&self, entry: &Rc<Entry>) -> Result<(), WorkflowServiceError> {
        self.current(entry)?;
        if entry.busy.replace(true) {
            return Err(WorkflowServiceError::Unavailable(
                "workflow assignment refresh already in progress".into(),
            ));
        }
        let _busy = Busy(&entry.busy);
        match futures::future::select(
            entry.stopped.clone(),
            bounded(self.options.operation_timeout, self.prepare(entry)).boxed_local(),
        )
        .await
        {
            Either::Left(_) => Err(retired()),
            Either::Right((result, _)) => result,
        }
    }

    async fn prepare(&self, entry: &Rc<Entry>) -> Result<(), WorkflowServiceError> {
        self.client
            .renew(entry.policies.scope())
            .await
            .map_err(transport_error)?;
        entry.policies.refresh().await?;
        let binding = entry.policies.binding();
        let authority = binding.authority()?;
        let runtime = entry
            .ready
            .borrow()
            .as_ref()
            .map(|ready| ready.runtime.clone());
        let runtime = match runtime {
            Some(runtime) => runtime,
            None => {
                authority
                    .run(self.factory.open(entry.policies.scope(), binding))
                    .await?
            }
        };
        if runtime.app.app_id() != binding.app_id() || !runtime.app.binding.same_binding(binding) {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        authority.check()?;
        self.current(entry)?;
        let consumer = entry
            .ready
            .borrow()
            .as_ref()
            .filter(|ready| !ready.consumer.is_retired())
            .map(|ready| ready.consumer.clone());
        let consumer = match consumer {
            Some(consumer) => consumer,
            None => ConsumerScope::new(
                runtime.app.clone(),
                entry.policies.scope().clone(),
                runtime.executor.clone(),
            )?,
        };
        *entry.ready.borrow_mut() = Some(Ready { runtime, consumer });
        self.publish()
    }
}

fn retired() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow assignment binding retired or superseded".into())
}

fn transport_error(error: zeroship_workflow_client::Error) -> WorkflowServiceError {
    match error {
        zeroship_workflow_client::Error::Timeout => WorkflowServiceError::Timeout,
        _ => WorkflowServiceError::Unavailable("workflow assignment metadata unavailable".into()),
    }
}

#[cfg(test)]
mod tests;
