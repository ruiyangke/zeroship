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
use zeroship_workflow_runner::{
    assignments::{CreatorFactory, CreatorRuntime},
    ObjectStepOutputs, PayloadObjects, TaskPayloadLimits, WorkerBinding,
};
use zeroship_workflow::{
    service::{
        store::HostStorage,
        AppDeployments, HostPolicies, IngressEpochs, PolicyBinding, SignalAuthority,
        WorkerIdentity, WorkflowService,
    },
    WorkflowServiceError,
};
use zeroship_workflow_client::WorkerCoordinator;
use zeroship_workflow_v8::V8TaskExecutor;

/// Creator capabilities resolved independently of manager placement metadata.
///
/// Storage is already provisioned and authorized for the requested app. Native
/// runtime peers must use that same app and creator storage. Deployment clients
/// carry the current assignment's retention authority; no platform database
/// connection belongs in these resources. Contexts resolve fresh env, limits,
/// network policy and native peers for each execution, without replacing the
/// workflow backend or moving its physical schema.
///
/// `signal_authority` signs and verifies signal capabilities. Without one the
/// app's capability issuance and ingestion refuse as unavailable; every other
/// operation is unaffected.
#[derive(Clone)]
pub struct WorkflowResources {
    pub storage: HostStorage,
    pub objects: PayloadObjects,
    pub deployments: AppDeployments,
    pub signal_authority: Option<Arc<SignalAuthority>>,
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
    /// Asks the manager to bring a refused journal to the version this build
    /// expects. `None` leaves a refusal terminal, which is what it was before.
    repair: Option<Rc<WorkerCoordinator>>,
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
            repair: None,
        })
    }

    /// Turn a refused journal into a repair request rather than a dead end.
    ///
    /// The host holds no DDL authority - privilege follows the process - so the
    /// repair is a request to the manager, which owns the journal artifacts and
    /// sends them to the migration service.
    #[must_use]
    pub fn with_journal_repair(mut self, client: Rc<WorkerCoordinator>) -> Self {
        self.repair = Some(client);
        self
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
        let first = self.build(scope, policy, ingress.clone()).await;
        let Err(refusal) = first else {
            return first;
        };
        // THE REPAIR PATH. A host refuses a journal whose fingerprint or version
        // is not the one it was built against, and until this existed that was
        // terminal: nothing in the system could bring the journal forward, so an
        // app whose journal predated a schema change simply stopped running.
        //
        // The retry is ONCE. A second refusal after a successful provision means
        // the journal is not merely out of date, and looping would turn a
        // reportable fault into a hot loop against a creator database.
        let Some(repair) = self.repair.as_ref() else {
            return Err(refusal);
        };
        let schema = self.provider.resolve(scope).await?.storage.binding.schema().clone();
        tracing::warn!(
            app = scope.app_id.as_str(),
            schema = schema.as_str(),
            error = %refusal,
            "workflow host refused the creator journal; asking the manager to provision it"
        );
        repair.ensure_journal(schema.as_str()).await.map_err(|error| {
            tracing::error!(
                schema = schema.as_str(),
                %error,
                "workflow journal repair refused; the original refusal stands"
            );
            refusal
        })?;
        self.build(scope, policy, ingress).await
    }
}

impl<P: WorkflowResourceProvider> WorkflowCreatorFactory<P> {
    async fn build(
        &self,
        scope: &AssignedScope,
        policy: &PolicyBinding,
        ingress: Rc<dyn IngressEpochs>,
    ) -> Result<CreatorRuntime, WorkflowServiceError> {
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
                let mut service = WorkflowService::open(
                    Rc::new(resources.storage.open().await?),
                    self.policies.clone(),
                )
                .await?
                .with_deployments(resources.deployments);
                if let Some(authority) = resources.signal_authority {
                    service = service.with_signal_authority(authority);
                }
                let app = service.register_app(policy).await?.with_ingress(ingress);
                let tasks = Rc::new(app.tasks(self.worker.clone(), resources.objects.clone()));
                // Workflow and request isolates share one client of this app's
                // engine, bound to the generation being prepared. Its journal
                // is the creator database this service was opened over.
                let backend = app.clone().into_backend(
                    &service,
                    Arc::new(ObjectStepOutputs::new(
                        resources.objects.clone(),
                        self.payloads.max_payload_bytes,
                    )?),
                    Arc::new(resources.objects.clone()),
                )?;
                let loader = Rc::new(WorkerWorkflowRuntimeLoader::new(contexts, backend.clone()));
                let executor = Rc::new(V8TaskExecutor::new(loader, tasks, self.payloads)?);
                Ok(CreatorRuntime {
                    app,
                    executor,
                    backend,
                    objects: resources.objects,
                })
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
