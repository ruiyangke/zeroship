//! Request-path access to the app backends a workflow host has made ready.
//!
//! The host publishes an app's [`AppBackend`] only after assignment preparation
//! passes its final checks, and retires it synchronously when the assignment is
//! removed, replaced or closed. Request isolates, on threads other than the
//! host's, resolve the currently published backend for every call: an unknown
//! or unready app receives a retryable refusal, and a retired generation is
//! never reached through this registry again.

use zeroship_workflow::{
    backend::{SharedWorkflowBackend, WorkflowBackend},
    operations::{
        DeliveredSignal, RestartOptions, RestartedRun, RunOperation, RunStatus, SignalOptions,
        StartOptions, StartedRun, TransitionedRun,
    },
    service::{AppBackend, PolicyBinding},
    WorkflowServiceError,
};
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    sync::{Arc, PoisonError, RwLock},
};
use zeroship_core::app_id::AppId;

/// Apps whose workflow backend the host has published. Clones share one registry.
///
/// Only the owning host installs and retires entries: [`crate::host::WorkerHost`]
/// after an assignment's final checks. Request threads resolve a backend through
/// [`Self::backend`]; they cannot obtain an [`AppBackend`] from the registry, and
/// installing one requires already holding it.
#[derive(Clone, Default)]
pub struct ReadyApps(Arc<RwLock<BTreeMap<AppId, AppBackend>>>);

impl std::fmt::Debug for ReadyApps {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ReadyApps").finish_non_exhaustive()
    }
}

impl ReadyApps {
    /// The `env.workflows` backend for request isolates of `app`.
    ///
    /// Every call resolves the backend published at that moment, so an isolate
    /// built before its app became ready starts working once it is, and stops
    /// reaching a generation as soon as the host retires it.
    #[must_use]
    pub fn backend(&self, app: AppId) -> SharedWorkflowBackend {
        Arc::new(ReadyBackend {
            apps: self.clone(),
            app,
        })
    }

    /// Whether `app` has a published backend.
    #[must_use]
    pub fn is_ready(&self, app: &AppId) -> bool {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(app)
    }

    /// Publish a prepared backend, replacing any earlier generation for its app.
    /// Call only after the preparation that produced it passed its final checks.
    pub fn install(&self, backend: AppBackend) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(backend.app_id().clone(), backend);
    }

    /// Withdraw `binding`'s generation if it is the one published. A retired
    /// generation cannot withdraw its replacement.
    pub fn retire(&self, binding: &PolicyBinding) {
        let mut apps = self.0.write().unwrap_or_else(PoisonError::into_inner);
        if apps
            .get(binding.app_id())
            .is_some_and(|backend| backend.binding().same_binding(binding))
        {
            apps.remove(binding.app_id());
        }
    }

    fn current(&self, app: &AppId) -> Result<AppBackend, WorkflowServiceError> {
        self.0
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(app)
            .cloned()
            .ok_or_else(|| {
                WorkflowServiceError::Unavailable(
                    "workflow app is not ready on this worker".into(),
                )
            })
    }
}

#[derive(Debug)]
struct ReadyBackend {
    apps: ReadyApps,
    app: AppId,
}

#[async_trait(?Send)]
impl WorkflowBackend for ReadyBackend {
    async fn start(
        &self,
        workflow_name: String,
        options: StartOptions,
    ) -> Result<StartedRun, WorkflowServiceError> {
        self.apps
            .current(&self.app)?
            .start(workflow_name, options)
            .await
    }

    async fn status(&self, run_id: String) -> Result<RunStatus, WorkflowServiceError> {
        self.apps.current(&self.app)?.status(run_id).await
    }

    async fn signal(
        &self,
        run_id: String,
        options: SignalOptions,
    ) -> Result<DeliveredSignal, WorkflowServiceError> {
        self.apps.current(&self.app)?.signal(run_id, options).await
    }

    async fn transition(
        &self,
        run_id: String,
        op: RunOperation,
    ) -> Result<TransitionedRun, WorkflowServiceError> {
        self.apps.current(&self.app)?.transition(run_id, op).await
    }

    async fn restart(
        &self,
        run_id: String,
        options: RestartOptions,
    ) -> Result<RestartedRun, WorkflowServiceError> {
        self.apps.current(&self.app)?.restart(run_id, options).await
    }

    async fn read_step_output(
        &self,
        run_id: String,
        name: String,
        occurrence: u32,
    ) -> Result<Vec<u8>, WorkflowServiceError> {
        self.apps
            .current(&self.app)?
            .read_step_output(run_id, name, occurrence)
            .await
    }

    async fn read_output(&self, run_id: String) -> Result<Vec<u8>, WorkflowServiceError> {
        self.apps.current(&self.app)?.read_output(run_id).await
    }
}
