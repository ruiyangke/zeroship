//! Connect authorized creator resources to the native worker and V8 executor.

#![expect(
    clippy::future_not_send,
    reason = "creator resources and V8 execution belong to their owning compio thread"
)]

use crate::workflow_runtime::{
    validate_context, WorkerWorkflowRuntimeLoader, WorkflowAppContext, WorkflowContextProvider,
};
use std::{future::Future, rc::Rc, sync::Arc};
use zeroship_core::{
    app_id::AppId,
    schema_name::SchemaName,
    workflow_coordination::{AssignedScope, WorkerId},
};
use zeroship_workflow::{
    service::{
        runner::{
            assignments::{CreatorFactory, CreatorRuntime},
            TaskPayloadLimits,
        },
        store::HostStorage,
        AppDeployments, HostPolicies, IngressEpochs, PolicyBinding, SignalAuthority,
        WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_v8::V8TaskExecutor;

/// Creator capabilities resolved independently of manager placement metadata.
///
/// Storage is already provisioned and authorized for the requested app. Native
/// runtime peers must use that same app and creator storage. Deployment clients
/// carry the current assignment's retention authority; no platform database
/// connection belongs in these resources. Contexts resolve fresh env, limits,
/// network policy and native peers for each execution, without replacing the
/// workflow backend or moving its physical schema.
#[derive(Clone)]
pub struct WorkflowResources {
    pub storage: HostStorage,
    pub deployments: AppDeployments,
    pub signal_authority: Arc<SignalAuthority>,
    pub contexts: Rc<dyn WorkflowContextProvider>,
}

impl std::fmt::Debug for WorkflowResources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowResources")
            .field("app", &self.storage.binding.app_id())
            .finish_non_exhaustive()
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
    worker: WorkerIdentity,
    payloads: TaskPayloadLimits,
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
    /// Supply the identity from the client used by `WorkerHost`, and the same
    /// `HostPolicies` registry as its reconciler.
    ///
    /// # Errors
    /// Rejects invalid payload limits before resolving creator resources.
    pub fn new(
        provider: P,
        policies: Arc<HostPolicies>,
        worker: &WorkerId,
        payloads: TaskPayloadLimits,
    ) -> Result<Self, WorkflowServiceError> {
        payloads.validate()?;
        Ok(Self {
            provider,
            policies,
            worker: WorkerIdentity::new(worker.as_str().to_owned())?,
            payloads,
        })
    }
}

impl<P: WorkflowResourceProvider> CreatorFactory for WorkflowCreatorFactory<P> {
    async fn open(
        &self,
        scope: &AssignedScope,
        policy: &PolicyBinding,
        ingress: Rc<dyn IngressEpochs>,
    ) -> Result<CreatorRuntime, WorkflowServiceError> {
        if policy.app_id() != &scope.app_id {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        self.policies
            .run_bound(policy, async {
                let resources = self.provider.resolve(scope).await?;
                if resources.storage.binding.app_id() != scope.app_id.as_str() {
                    return Err(WorkflowServiceError::PermissionDenied);
                }
                let contexts = Rc::new(BoundContexts {
                    app: scope.app_id.clone(),
                    schema: resources.storage.binding.schema().clone(),
                    source: resources.contexts,
                });
                contexts.resolve(&scope.app_id)?;
                let service = WorkflowService::open(
                    Rc::new(resources.storage.open().await?),
                    self.policies.clone(),
                )
                .await?
                .with_payload_storage(resources.storage.objects)?
                .with_deployments(resources.deployments)
                .with_signal_authority(resources.signal_authority);
                let app = service.register_app(policy).await?.with_ingress(ingress);
                let tasks = Rc::new(app.tasks(self.worker.clone()));
                let loader = Rc::new(WorkerWorkflowRuntimeLoader::new(
                    contexts,
                    app.clone().into_backend(self.payloads.max_payload_bytes)?,
                ));
                let executor = Rc::new(V8TaskExecutor::new(loader, tasks, self.payloads)?);
                Ok(CreatorRuntime { app, executor })
            })
            .await
    }
}

struct BoundContexts {
    app: AppId,
    schema: SchemaName,
    source: Rc<dyn WorkflowContextProvider>,
}

impl WorkflowContextProvider for BoundContexts {
    fn resolve(&self, app: &AppId) -> Result<WorkflowAppContext, WorkflowServiceError> {
        if app != &self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let context = self.source.resolve(app)?;
        if context.app != self.app || context.schema != self.schema {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        validate_context(&context)?;
        Ok(context)
    }
}

#[cfg(test)]
mod tests;
