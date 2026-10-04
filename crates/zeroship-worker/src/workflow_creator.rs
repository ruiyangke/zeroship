//! Connect authorized creator resources to the native worker and V8 executor.

use crate::workflow_runtime::{
    validate_context, WorkerWorkflowRuntimeLoader, WorkflowAppContext, WorkflowContextProvider,
};
use std::{future::Future, rc::Rc, sync::Arc, time::Duration};
use zeroship_bundle::BlobStore;
use zeroship_core::app_id::AppId;
use zeroship_workflow_runner::{
    prepared::{CreatorFactory, CreatorRuntime, Residency},
    remote::RemoteWorkflows,
    remote_tasks::RemoteTasks,
    PayloadObjects, TaskPayloadLimits,
};
use zeroship_workflow::{service::WorkerIdentity, WorkflowServiceError};
use zeroship_workflow_client::WorkerCoordinator;
use zeroship_workflow_v8::V8TaskExecutor;

/// Creator capabilities a claimed app is prepared from, resolved by this host
/// rather than taken from the claim.
///
/// NO CREATOR DATABASE APPEARS HERE. The journal this app's workflows live in
/// belongs to the workflow service, and this host reaches it over the same
/// enrolled client it claims with, so what a prepared app needs on this side
/// is the payload object store, the app artifact store, and the per-execution
/// context. Contexts resolve fresh env, limits, network policy and native peers
/// for each execution.
#[derive(Clone)]
pub struct WorkflowResources {
    pub objects: PayloadObjects,
    /// The artifact store a pinned deployment's bytes are read from. The
    /// service resolves WHICH deployment a claim is pinned to; this host loads
    /// it, so the bytes never cross the transport.
    pub artifacts: Arc<dyn BlobStore>,
    /// Ceiling on one deployment's source bytes, applied where they are loaded.
    pub max_source_bytes: usize,
    pub contexts: Rc<dyn WorkflowContextProvider>,
    /// The app's hold on its credentials, taken before the provider checked
    /// whether they were supplied. The prepared app keeps it.
    pub residency: Rc<dyn Residency>,
}

impl std::fmt::Debug for WorkflowResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowResources").finish_non_exhaustive()
    }
}

/// Host-owned resource resolution, separate from enrollment and the claim.
pub trait WorkflowResourceProvider {
    /// Resolve independently authorized deployment-host resources for `app`.
    ///
    /// The app id selects that authority; it cannot create it. Dropping the
    /// future must stop or quarantine any pending native operation.
    ///
    /// # Errors
    /// Refuses apps this worker is not authorized for and unavailable creator
    /// resources.
    fn resolve(
        &self,
        app: &AppId,
    ) -> impl Future<Output = Result<WorkflowResources, WorkflowServiceError>>;
}

/// Build creator execution under the host's exact signer.
pub struct WorkflowCreatorFactory<P> {
    provider: P,
    /// The enrolled client every creator call and every task payload operation
    /// crosses on. It is the one this host claims with, so a creator call costs
    /// one authenticated request and lands in the journal the service owns.
    client: WorkerCoordinator,
    /// The request path's backend source. Workflow-execution isolates resolve
    /// their `env.workflows` from it, so an app this host never prepared -- as
    /// when another worker started its run -- can still execute it.
    workflows: RemoteWorkflows,
    worker: WorkerIdentity,
    payloads: TaskPayloadLimits,
    /// Bound on one payload object write, the same one the host's other
    /// per-operation I/O answers to.
    write_budget: Duration,
}

impl<P> std::fmt::Debug for WorkflowCreatorFactory<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowCreatorFactory")
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl<P> WorkflowCreatorFactory<P> {
    /// Bind creator assembly to the enrolled worker.
    ///
    /// Supply the SAME client `WorkerHost` claims through. The worker identity
    /// is taken from that client rather than passed alongside it, so the
    /// identity a payload operation is authorized under cannot disagree with
    /// the one the request is signed by.
    ///
    /// # Errors
    /// Rejects invalid payload limits and an empty write budget before resolving
    /// creator resources.
    pub fn new(
        provider: P,
        client: WorkerCoordinator,
        workflows: RemoteWorkflows,
        payloads: TaskPayloadLimits,
        write_budget: Duration,
    ) -> Result<Self, WorkflowServiceError> {
        payloads.validate()?;
        if write_budget.is_zero() {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow payload write budget must be positive".into(),
            ));
        }
        let worker = WorkerIdentity::new(client.worker_id().as_str().to_owned())?;
        Ok(Self {
            provider,
            client,
            workflows,
            worker,
            payloads,
            write_budget,
        })
    }
}

impl<P: WorkflowResourceProvider> CreatorFactory for WorkflowCreatorFactory<P> {
    /// No journal handle, because the journal is the workflow service's.
    ///
    /// The consumer hands this to the transport on every exchange, and the
    /// host's transport crosses to the service, so there is nothing of this
    /// shape to hold.
    type Journal = ();

    fn open<'a>(
        &'a self,
        app: &'a AppId,
    ) -> futures::future::LocalBoxFuture<'a, Result<CreatorRuntime<()>, WorkflowServiceError>> {
        Box::pin(async move {
            let resources = self.provider.resolve(app).await?;
            let contexts = Rc::new(BoundContexts {
                app: app.clone(),
                source: resources.contexts,
            });
            contexts.resolve(app)?;
            // Workflow and request isolates share one backend for this app,
            // resolved from the request path's registry. Its journal is the
            // service's, and the far end admits each call by the worker's
            // verified zone against the app's frozen zone.
            let backend = self.workflows.backend(app.clone());
            let tasks = Rc::new(RemoteTasks::new(
                self.client.clone(),
                app.clone(),
                resources.objects.clone(),
                resources.artifacts.clone(),
                resources.max_source_bytes,
                self.payloads.max_payload_bytes,
                self.write_budget,
            )?);
            let loader = Rc::new(WorkerWorkflowRuntimeLoader::new(contexts, backend));
            let executor = Rc::new(V8TaskExecutor::new(loader, tasks, self.payloads)?);
            Ok(CreatorRuntime {
                app: (),
                executor,
                residency: resources.residency,
            })
        })
    }
}

struct BoundContexts {
    app: AppId,
    source: Rc<dyn WorkflowContextProvider>,
}

impl WorkflowContextProvider for BoundContexts {
    fn resolve(&self, app: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError> {
        if app != &self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let context = self.source.resolve(app)?;
        if context.app != self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        validate_context(&context)?;
        Ok(context)
    }
}

#[cfg(test)]
mod tests;
