//! Connect authorized creator resources to the native worker and V8 executor.

#![expect(
    clippy::future_not_send,
    reason = "creator resources and V8 execution belong to their owning compio thread"
)]

use crate::workflow_runtime::{
    validate_context, WorkerWorkflowRuntimeLoader, WorkflowAppContext, WorkflowContextProvider,
};
use std::{future::Future, rc::Rc, sync::Arc, time::Duration};
use zeroship_bundle::BlobStore;
use zeroship_core::{app_id::AppId, workflow_coordination::AssignedScope};
use zeroship_workflow_runner::{
    assignments::{CreatorFactory, CreatorRuntime},
    remote::RemoteBackend,
    remote_tasks::RemoteTasks,
    PayloadObjects, TaskPayloadLimits,
};
use zeroship_workflow::{
    service::{HostPolicies, IngressEpochs, PolicyBinding, WorkerIdentity},
    WorkflowServiceError,
};
use zeroship_workflow_client::WorkerCoordinator;
use zeroship_workflow_v8::V8TaskExecutor;

/// Creator capabilities resolved independently of manager placement metadata.
///
/// NO CREATOR DATABASE APPEARS HERE. The journal this app's workflows live in
/// belongs to the workflow service, and this host reaches it over the same
/// enrolled client it registers and claims with, so what a placement needs on
/// this side is the payload object store, the app artifact store, and the
/// per-execution context. Contexts resolve fresh env, limits, network policy and
/// native peers for each execution.
///
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
}

impl std::fmt::Debug for WorkflowResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowResources").finish_non_exhaustive()
    }
}

/// Host-owned resource resolution, separate from enrollment and placement.
pub trait WorkflowResourceProvider {
    /// Resolve independently authorized deployment-host resources.
    ///
    /// Scope IDs select that authority; they cannot create it.
    /// The revision can select a bound retention client, never database access.
    /// Dropping the future must stop or quarantine any pending native operation.
    ///
    /// # Errors
    /// Refuses unauthorized scopes and unavailable creator resources.
    fn resolve(
        &self,
        scope: &AssignedScope,
    ) -> impl Future<Output = Result<WorkflowResources, WorkflowServiceError>>;
}

/// Build creator execution under the host's exact signer and policy registry.
pub struct WorkflowCreatorFactory<P> {
    provider: P,
    policies: Arc<HostPolicies>,
    /// The enrolled client every creator call and every task payload operation
    /// crosses on. It is the one this host registers and claims with, so a
    /// creator call costs one authenticated request and lands in the journal the
    /// service owns.
    client: WorkerCoordinator,
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
    /// Bind creator assembly to the enrolled worker and its policy registry.
    ///
    /// Supply the SAME client `WorkerHost` coordinates through, and the same
    /// `HostPolicies` registry as its reconciler. The worker identity is taken
    /// from that client rather than passed alongside it, so the identity a
    /// payload operation is authorized under cannot disagree with the one the
    /// request is signed by.
    ///
    /// # Errors
    /// Rejects invalid payload limits and an empty write budget before resolving
    /// creator resources.
    pub fn new(
        provider: P,
        policies: Arc<HostPolicies>,
        client: WorkerCoordinator,
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
            policies,
            client,
            worker,
            payloads,
            write_budget,
        })
    }
}

impl<P: WorkflowResourceProvider> CreatorFactory for WorkflowCreatorFactory<P> {
    /// No journal handle, because the journal is the workflow service's.
    ///
    /// The consumer hands this to the transport on every claim, and the host's
    /// transport crosses to the service, so there is nothing of this shape to
    /// hold. `JournalDuties for ()` states which duties that removes.
    type Journal = ();

    async fn open(
        &self,
        scope: &AssignedScope,
        policy: &PolicyBinding,
        _ingress: Rc<dyn IngressEpochs>,
    ) -> Result<CreatorRuntime<()>, WorkflowServiceError> {
        if policy.app_id() != &scope.app_id {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        // NO INGRESS IS ATTACHED HERE. `AppWorkflows::with_ingress` gives a local
        // journal an epoch source, so a fenced acceptance can establish a newer
        // one inside the caller's own transaction. This host holds no journal to
        // attach it to, and the service applies its own epoch on each request, so
        // an epoch asserted from this side would be one nothing rechecks. The
        // placement's policy lease still establishes: `AssignedPolicies` is the
        // ingress source, and the host's reconciler drives it.
        self.policies
            .run_bound(policy, async {
                let resources = self.provider.resolve(scope).await?;
                let contexts = Rc::new(BoundContexts {
                    app: scope.app_id.clone(),
                    source: resources.contexts,
                });
                contexts.resolve(&scope.app_id)?;
                // Workflow and request isolates share one client of this app's
                // engine, bound to the placement being prepared. Its journal is
                // the service's, and the far end admits each call under the
                // generation it observes for this placement.
                let backend = RemoteBackend::new(
                    self.client.clone(),
                    scope.clone(),
                    resources.objects.clone(),
                    self.payloads.max_payload_bytes,
                )?;
                let tasks = Rc::new(RemoteTasks::new(
                    self.client.clone(),
                    scope.app_id.clone(),
                    resources.objects.clone(),
                    resources.artifacts.clone(),
                    resources.max_source_bytes,
                    self.payloads.max_payload_bytes,
                    self.write_budget,
                )?);
                let loader = Rc::new(WorkerWorkflowRuntimeLoader::new(contexts, backend.clone()));
                let executor = Rc::new(V8TaskExecutor::new(loader, tasks, self.payloads)?);
                Ok(CreatorRuntime {
                    app: (),
                    executor,
                    backend,
                })
            })
            .await
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
