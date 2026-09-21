//! Bind manager placements to independently authorized creator resources.

#![expect(
    clippy::future_not_send,
    reason = "creator bindings and metadata clients stay on their compio thread"
)]

use crate::{
    consumer::{ConsumerBindings, ConsumerScope},
    delivery::bounded,
    publication::{self, PublicationWait, PublicationWake},
    ready::ReadyApps,
    PayloadObjects, TaskExecutor,
};
use zeroship_workflow::{
    service::{
        publication::AssignedPublisher, AppBackend, AppWorkflows, AssignedPolicies, HostPolicies,
        IngressEpochs, PolicyBinding,
    },
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
    collections::{BTreeMap, BTreeSet},
    future::Future,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, Assignment, FailureCode, ReleaseReason, ReleaseScope, RequestId, ScopePage,
    },
};
use zeroship_workflow_client::WorkerCoordinator;

/// Creator I/O and execution assembled by trusted deployment-host configuration.
///
/// `backend` is the app's workflow client for request isolates. It must come
/// from `app` itself, so it carries the same policy generation. `objects` is
/// the payload store this host resolved for the app; payload bytes move only
/// through it.
#[derive(Clone)]
pub struct CreatorRuntime {
    pub app: AppWorkflows,
    pub executor: Rc<dyn TaskExecutor>,
    pub backend: AppBackend,
    pub objects: PayloadObjects,
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
    /// Use the supplied policy generation for the returned app, and attach
    /// `ingress` to it with [`AppWorkflows::with_ingress`] before creating any
    /// backend, so a fenced acceptance establishes a newer epoch through this
    /// placement's policy lease. Opening must be cancellation safe: dropping
    /// this future must stop or quarantine its I/O.
    fn open(
        &self,
        scope: &AssignedScope,
        policy: &PolicyBinding,
        ingress: Rc<dyn IngressEpochs>,
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
    /// The request-path backend published while this generation is ready.
    backend: AppBackend,
}

struct Entry {
    policies: AssignedPolicies,
    ready: RefCell<Option<Ready>>,
    published: ReadyApps,
    busy: Cell<bool>,
    stop: RefCell<Option<oneshot::Sender<()>>>,
    stopped: Shared<LocalBoxFuture<'static, ()>>,
}

impl Entry {
    fn new(policies: AssignedPolicies, published: ReadyApps) -> Self {
        let (stop, stopped) = oneshot::channel();
        Self {
            policies,
            ready: RefCell::new(None),
            published,
            busy: Cell::new(false),
            stop: RefCell::new(Some(stop)),
            stopped: async {
                let _ = stopped.await;
            }
            .boxed_local()
            .shared(),
        }
    }

    /// Withdraw request ingress before revoking the generation, so no request
    /// thread can resolve this backend once retirement begins.
    fn retire(&self) -> Result<(), WorkflowServiceError> {
        if let Some(stop) = self.stop.borrow_mut().take() {
            let _ = stop.send(());
        }
        self.published.retire(self.policies.binding());
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
/// Use this as the sole writer of the supplied `ConsumerBindings` and
/// [`ReadyApps`]. An app's backend is published only once its preparation
/// passes the final current-entry and original-authority checks. Closing or
/// dropping this withdraws every published backend and revokes admission
/// synchronously; the host must separately await the consumer's joined drain
/// before discarding execution capacity.
pub struct AssignmentBindings<F> {
    client: WorkerCoordinator,
    policies: Arc<HostPolicies>,
    consumer: ConsumerBindings,
    factory: F,
    options: AssignmentOptions,
    ready: ReadyApps,
    wake: PublicationWake,
    marked: PublicationWait,
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
        ready: ReadyApps,
        options: AssignmentOptions,
    ) -> Result<Self, WorkflowServiceError> {
        Self::with_publication(
            client,
            policies,
            consumer,
            factory,
            ready,
            options,
            publication::channel(),
        )
    }

    /// As [`Self::new`], sharing the publication marks a host's transport sets.
    pub(crate) fn with_publication(
        client: WorkerCoordinator,
        policies: Arc<HostPolicies>,
        consumer: ConsumerBindings,
        factory: F,
        ready: ReadyApps,
        options: AssignmentOptions,
        (wake, marked): (PublicationWake, PublicationWait),
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
            ready,
            wake,
            marked,
            entries: RefCell::new(BTreeMap::new()),
            scan: Cell::new(0),
            closed: Cell::new(false),
        })
    }

    /// Wait until a request-path mutation or a settled delivery marks apps
    /// with intents to publish, then take those apps.
    pub(crate) async fn marked(&self) -> BTreeSet<AppId> {
        self.marked.next().await
    }

    /// Submit the pending intents of each marked app that is ready here,
    /// under its current assignment. Retirement stops an app's pass. Failures
    /// leave intents for the next pass or the manager's reconciliation job.
    pub(crate) async fn publish_marked(&self, apps: BTreeSet<AppId>) {
        let entries: Vec<_> = {
            let current = self.entries.borrow();
            apps.iter()
                .filter_map(|app| current.get(app).cloned())
                .collect()
        };
        let mut passes: FuturesUnordered<_> = entries
            .iter()
            .map(|entry| self.publish_entry(entry))
            .collect();
        while let Some(result) = passes.next().await {
            if let Err(error) = result {
                tracing::warn!(
                    code = error.code(),
                    "workflow publication left to manager reconciliation"
                );
            }
        }
    }

    async fn publish_entry(&self, entry: &Rc<Entry>) -> Result<(), WorkflowServiceError> {
        let app = entry
            .ready
            .borrow()
            .as_ref()
            .map(|ready| ready.runtime.app.clone());
        // An app that is not ready yet is marked again when it becomes ready.
        let Some(app) = app else {
            return Ok(());
        };
        let publisher = AssignedPublisher::new(&self.client, entry.policies.scope().clone());
        // A local binding, so the unfinished half drops before what it borrows.
        let selected = futures::future::select(
            entry.stopped.clone(),
            publication::publish_pending(&app, &publisher, self.options.operation_timeout)
                .boxed_local(),
        )
        .await;
        match selected {
            Either::Left(_) => Err(retired()),
            Either::Right((result, _)) => result,
        }
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
                    .insert(app, Rc::new(Entry::new(policies, self.ready.clone())));
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
        // Prefer a SUBSTANTIVE refusal over the contention answer. Entries
        // refresh concurrently, and a caller arriving while one is already
        // running reports it unavailable by design (see above) - so taking
        // whichever error finished first could report that benign answer and
        // discard a real refusal from another entry. A revoked placement then
        // reads in the log as nothing worse than a busy refresh.
        let mut result = Ok(());
        while let Some(refreshed) = pending.next().await {
            let Err(error) = refreshed else { continue };
            let supersedes = match &result {
                Ok(()) => true,
                Err(held) => is_refresh_contention(held) && !is_refresh_contention(&error),
            };
            if supersedes {
                result = Err(error);
            }
        }
        result
    }

    async fn refresh_entry(&self, entry: &Rc<Entry>) -> Result<(), WorkflowServiceError> {
        self.current(entry)?;
        if entry.busy.replace(true) {
            return Err(WorkflowServiceError::Unavailable(
                REFRESH_IN_PROGRESS.into(),
            ));
        }
        let _busy = Busy(&entry.busy);
        let outcome = match futures::future::select(
            entry.stopped.clone(),
            bounded(self.options.operation_timeout, self.prepare(entry)).boxed_local(),
        )
        .await
        {
            Either::Left(_) => Err(retired()),
            Either::Right((result, _)) => result,
        };
        if matches!(outcome, Err(WorkflowServiceError::PermissionDenied)) {
            self.refuse(entry).await;
        }
        outcome
    }

    /// Give up a placement this process is not permitted to serve.
    ///
    /// Refusal is for the one failure a retry cannot change: the manager's
    /// grant does not authorize this process for this app, so preparing it
    /// again here would fail the same way forever. Every other failure -
    /// unavailable storage, a timeout, an internal fault - keeps the placement
    /// and retries. The release carries the closed reason and no hint: it
    /// discharges no recovery responsibility, and the manager both stops
    /// offering this pair to this instance and places the app elsewhere.
    ///
    /// A release that does not reach the manager changes nothing: the
    /// placement stands and the next refresh refuses again.
    async fn refuse(&self, entry: &Rc<Entry>) {
        let scope = entry.policies.scope().clone();
        let released = self
            .client
            .release(&ReleaseScope {
                request_id: RequestId::mint(),
                app_id: scope.app_id.clone(),
                assignment_revision: scope.assignment_revision,
                reason: ReleaseReason::Refused,
            })
            .await;
        if let Err(error) = released {
            tracing::warn!(
                app_id = %scope.app_id.as_str(),
                %error,
                "workflow host could not release a placement it cannot serve"
            );
            return;
        }
        tracing::warn!(
            app_id = %scope.app_id.as_str(),
            "workflow host refused a placement it is not permitted to serve"
        );
        if let Some(entry) = self.entries.borrow_mut().remove(&scope.app_id) {
            let _ = entry.retire();
        }
        let _ = self.publish();
    }

    async fn prepare(&self, entry: &Rc<Entry>) -> Result<(), WorkflowServiceError> {
        self.client
            .renew(entry.policies.scope())
            .await
            .map_err(transport_error)?;
        let runtime = entry
            .ready
            .borrow()
            .as_ref()
            .map(|ready| ready.runtime.clone());
        if runtime.is_some() || entry.policies.binding().ingress_epoch().is_some() {
            entry.policies.refresh().await?;
        } else {
            // Establish responsibility before the app can accept ingress. A
            // policy that refuses establishment, as archive does, or an app
            // not yet activated still serves delivered work, including its own
            // closure, under a plain lease. Later refreshes never reopen it; a
            // fenced acceptance establishes on demand.
            match entry.policies.establish(None).await {
                Err(WorkflowServiceError::PermissionDenied | WorkflowServiceError::Conflict(_)) => {
                    entry.policies.refresh().await?;
                }
                established => established?,
            }
        }
        let binding = entry.policies.binding();
        let authority = binding.authority()?;
        let runtime = if let Some(runtime) = runtime {
            runtime
        } else {
            let ingress: Rc<dyn IngressEpochs> = Rc::new(entry.policies.clone());
            authority
                .run(self.factory.open(entry.policies.scope(), binding, ingress))
                .await?
        };
        if runtime.app.app_id() != binding.app_id()
            || !runtime.app.binding().same_binding(binding)
            || runtime.backend.app_id() != binding.app_id()
            || !runtime.backend.binding().same_binding(binding)
        {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        authority.check()?;
        self.current(entry)?;
        // Nothing below awaits, so retirement cannot interleave between these
        // final checks and publication.
        let previous = entry
            .ready
            .borrow()
            .as_ref()
            .map(|ready| (ready.consumer.clone(), ready.backend.clone()));
        let consumer = match previous
            .as_ref()
            .map(|(consumer, _)| consumer)
            .filter(|consumer| !consumer.is_retired())
        {
            Some(consumer) => consumer.clone(),
            None => ConsumerScope::new(
                runtime.app.clone(),
                entry.policies.scope().clone(),
                runtime.executor.clone(),
                runtime.objects.clone(),
            )?,
        };
        let app = binding.app_id().clone();
        let (backend, first) = match previous {
            Some((_, backend)) => (backend, false),
            None => (
                runtime
                    .backend
                    .clone()
                    .with_commit_hint(self.wake.hint(app.clone())),
                true,
            ),
        };
        *entry.ready.borrow_mut() = Some(Ready {
            runtime,
            consumer,
            backend: backend.clone(),
        });
        self.publish()?;
        // Admit request ingress last, once delivered work can also be claimed.
        self.ready.install(backend);
        if first {
            // A previous process may have committed intents it never published.
            self.wake.mark(&app);
        }
        Ok(())
    }
}

fn retired() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow assignment binding retired or superseded".into())
}

/// The contention answer a caller gets while another refresh holds the entry.
///
/// Named once, because [`Assignments::refresh`] has to recognise it in order to
/// stop it superseding a real refusal, and a literal repeated at both ends
/// would let the two drift apart silently.
const REFRESH_IN_PROGRESS: &str = "workflow assignment refresh already in progress";

/// Whether this failure is the contention answer rather than a refusal.
fn is_refresh_contention(error: &WorkflowServiceError) -> bool {
    matches!(error, WorkflowServiceError::Unavailable(message) if message == REFRESH_IN_PROGRESS)
}

/// Carry a coordinator answer without flattening a refusal into an outage.
///
/// Same reasoning as the policy lease's mapping: a refusal is durable and wants
/// an operator, an outage is transient and wants a retry, and reporting both as
/// `workflow_unavailable` left a worker whose assignment was refused logging
/// the same line as one that could not reach the manager. `refuse` above
/// already keys off `PermissionDenied`, which this function could never
/// produce.
fn transport_error(error: zeroship_workflow_client::Error) -> WorkflowServiceError {
    use zeroship_workflow_client::Error as Wire;
    match error {
        Wire::Timeout => WorkflowServiceError::Timeout,
        Wire::Unauthenticated | Wire::Refused(FailureCode::Unauthenticated) => {
            WorkflowServiceError::Unauthenticated
        }
        Wire::Refused(FailureCode::Denied) => WorkflowServiceError::PermissionDenied,
        Wire::Refused(FailureCode::Conflict) => WorkflowServiceError::Conflict(
            "workflow assignment conflicts with the manager's record".into(),
        ),
        Wire::Refused(FailureCode::Capacity) => WorkflowServiceError::ResourceExhausted(
            "workflow assignment refused for capacity".into(),
        ),
        Wire::Refused(FailureCode::Invalid) | Wire::Refused(FailureCode::RequestTooLarge) => {
            WorkflowServiceError::InvalidRequest("workflow assignment request refused".into())
        }
        Wire::Refused(FailureCode::Unavailable)
        | Wire::Unavailable
        | Wire::InvalidConfig
        | Wire::InvalidResponse
        | Wire::RequestTooLarge
        | Wire::ResponseTooLarge => {
            WorkflowServiceError::Unavailable("workflow assignment metadata unavailable".into())
        }
    }
}

#[cfg(test)]
mod tests;
