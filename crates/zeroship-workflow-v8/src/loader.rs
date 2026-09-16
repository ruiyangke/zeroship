//! Build task isolates from retained code and a separately bound app context.

use crate::{LoadedWorkflow, WorkflowBinding, WorkflowRuntimeLoader};
use std::{collections::HashMap, sync::Arc};
use zeroship_bundle::LoadedWorker;
use zeroship_runtime::{EnvSnapshot, ModuleEntry, NativePlugin, Runtime, RuntimeLimits};
use zeroship_workflow::{
    service::{AppBackend, TaskAssignment},
    WorkflowServiceError,
};

pub struct AppRuntimeLoader {
    backend: AppBackend,
    env_vars: HashMap<String, String>,
    env: EnvSnapshot,
    plugins: Vec<Arc<dyn NativePlugin>>,
    limits: RuntimeLimits,
}
impl std::fmt::Debug for AppRuntimeLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppRuntimeLoader").finish_non_exhaustive()
    }
}
impl AppRuntimeLoader {
    /// Bind the customer's workflow backend, native peers and runtime variables.
    /// Executable modules and schema descriptors come from each retained task.
    ///
    /// # Errors
    /// Rejects a peer that would replace the bound workflow namespace.
    pub fn new(
        backend: AppBackend,
        env_vars: HashMap<String, String>,
        env: EnvSnapshot,
        mut plugins: Vec<Arc<dyn NativePlugin>>,
        limits: RuntimeLimits,
    ) -> Result<Self, WorkflowServiceError> {
        if plugins
            .iter()
            .any(|plugin| plugin.namespace() == "workflows")
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow loader received a conflicting native binding".into(),
            ));
        }
        plugins.push(Arc::new(WorkflowBinding::service(backend.clone())));
        Ok(Self {
            backend,
            env_vars,
            env,
            plugins,
            limits,
        })
    }
}

impl WorkflowRuntimeLoader for AppRuntimeLoader {
    fn load(
        &self,
        assignment: &TaskAssignment,
        executable: &LoadedWorker,
    ) -> Result<LoadedWorkflow, WorkflowServiceError> {
        if assignment.invocation.app_id != self.backend.app_id().as_str() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let modules = std::iter::once(executable.entry())
            .chain(
                executable
                    .modules()
                    .keys()
                    .map(String::as_str)
                    .filter(|name| *name != executable.entry()),
            )
            .map(|name| ModuleEntry {
                specifier: name.into(),
                source: executable.modules()[name].clone(),
            })
            .collect();
        let descriptor = executable
            .runtime_descriptor()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| {
                WorkflowServiceError::InvalidRequest("invalid workflow runtime descriptor".into())
            })?;
        let runtime = Runtime::builder()
            .app_id(self.backend.app_id().clone())
            .modules(modules)
            .runtime_descriptor(descriptor)
            .env_vars(self.env_vars.clone())
            .plugins(self.plugins.clone())
            .limits(self.limits)
            .build();
        Ok(LoadedWorkflow::new(runtime, self.env.clone()))
    }
}
