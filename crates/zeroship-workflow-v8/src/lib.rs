//! Durable workflow V8 binding — `env.workflows`.
//!
//! `WorkflowBinding::build_instance` mints a native `env.workflows` namespace
//! per isolate. Each namespace owns an app-scoped Rust backend; credentials
//! remain in the host. Service and remote bindings validate the immutable
//! runtime identity before evaluating app code, because each names one app.
//! A request-path binding resolves a backend for the isolate's own app from
//! the worker's [`RemoteWorkflows`], on every call.

mod error;
mod executor;
mod loader;
pub mod v8_class;

pub use executor::{LoadedWorkflow, V8TaskExecutor, WorkflowRuntimeLoader};
pub use loader::AppRuntimeLoader;

use std::sync::Arc;

use zeroship_runtime::plugin::{JavaScriptModule, NativePlugin, NativeRegistrar};
use zeroship_workflow::backend::SharedWorkflowBackend;
use zeroship_workflow_runner::remote::{RemoteBackend, RemoteWorkflows};

pub use v8_class::{is_excluded_workflow_property, mint_workflows};

#[derive(Clone, Debug)]
enum WorkflowBackendFactory {
    Service {
        backend: Arc<zeroship_workflow::service::AppBackend>,
    },
    Remote {
        backend: Arc<RemoteBackend>,
    },
    /// Boxed so the factory stays the size of its app-scoped arms: the
    /// registry carries the client, payload store and read limit by value.
    RemoteWorkflows {
        workflows: Box<RemoteWorkflows>,
    },
    /// No workflow service is configured for this process: every call is
    /// refused as retryable.
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct WorkflowBinding {
    backend: WorkflowBackendFactory,
}

impl WorkflowBinding {
    /// Bind the customer's workflow engine to its authorized app.
    #[must_use]
    pub fn service(backend: zeroship_workflow::service::AppBackend) -> Self {
        Self {
            backend: WorkflowBackendFactory::Service {
                backend: Arc::new(backend),
            },
        }
    }

    /// Bind a host that holds no journal to the workflow service over HTTP,
    /// for the one app the backend names.
    ///
    /// Identity is checked the same way the service arm's is: the backend names
    /// one app, so an isolate of another app must not reach it. The request-path
    /// arm is the one that skips the check, because there the runtime's own
    /// identity is what selects the backend.
    #[must_use]
    pub fn remote(backend: RemoteBackend) -> Self {
        Self {
            backend: WorkflowBackendFactory::Remote {
                backend: Arc::new(backend),
            },
        }
    }

    /// Bind each isolate to a backend the request path resolves for the
    /// runtime's own app. The service admits every call by the worker's
    /// verified zone, so an app this process never prepared is served.
    #[must_use]
    pub fn remote_workflows(workflows: RemoteWorkflows) -> Self {
        Self {
            backend: WorkflowBackendFactory::RemoteWorkflows {
                workflows: Box::new(workflows),
            },
        }
    }

    /// A namespace for a process with no workflow service configured. Every
    /// call is refused as `Unavailable`, the retryable refusal, so creator code
    /// sees the same `env.workflows` it always does and a call that fails here
    /// is one it may try again; nothing falls back to another service.
    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            backend: WorkflowBackendFactory::Unavailable,
        }
    }
}

impl NativePlugin for WorkflowBinding {
    fn bind_runtime_descriptor<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _app_id: &str,
        _namespace: v8::Local<'s, v8::Object>,
        _descriptor: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        // An app-scoped backend is checked against the runtime it is about to
        // serve; the request-path arm resolves its backend BY that identity, so
        // there is nothing for it to disagree with.
        let bound = match &self.backend {
            WorkflowBackendFactory::Service { backend } => Some(backend.app_id()),
            WorkflowBackendFactory::Remote { backend } => Some(backend.app_id()),
            WorkflowBackendFactory::RemoteWorkflows { .. } | WorkflowBackendFactory::Unavailable => {
                None
            }
        };
        if let Some(app) = bound {
            if zeroship_runtime::plugin::runtime_app_identity(scope).as_ref() != Some(app) {
                return Err("workflow binding does not match runtime app identity".into());
            }
        }
        Ok(())
    }

    fn namespace(&self) -> &str {
        "workflows"
    }

    fn name(&self) -> &str {
        "workflows"
    }

    fn register(&self, _r: &mut NativeRegistrar) {}

    /// The replay bridge. Host-only: creator modules cannot import it, so the
    /// interpreter is never part of the creator's module graph and never
    /// reachable from creator code.
    fn host_javascript_modules(&self) -> &'static [JavaScriptModule] {
        &[JavaScriptModule {
            specifier: zeroship_runtime::WORKFLOW_DISPATCH_MODULE,
            source: include_str!("../js/dispatch.js"),
        }]
    }

    /// Wrap the ambient I/O globals before creator modules evaluate. A creator
    /// that captures `fetch` or `setTimeout` at module scope must still hold a
    /// binding that refuses direct I/O from a workflow body.
    fn prepare_runtime<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _namespace: v8::Local<'s, v8::Object>,
        _descriptor: Option<&serde_json::Value>,
    ) -> Result<Option<v8::Global<v8::Promise>>, String> {
        zeroship_runtime::modules::invoke_module_export(
            scope,
            zeroship_runtime::WORKFLOW_DISPATCH_MODULE,
            "installBodyGuards",
            &[],
        )
        .map(Some)
    }

    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        let backend: SharedWorkflowBackend = match &self.backend {
            WorkflowBackendFactory::Service { backend } => backend.clone(),
            WorkflowBackendFactory::Remote { backend } => backend.clone(),
            // The immutable identity the runtime was built with selects the
            // app; creator-visible environment values cannot.
            WorkflowBackendFactory::RemoteWorkflows { workflows } => Arc::new(
                workflows.backend(zeroship_runtime::plugin::runtime_app_identity(scope)?),
            ),
            WorkflowBackendFactory::Unavailable => Arc::new(Unconfigured),
        };
        mint_workflows(scope, backend)
    }
}

/// The backend of a process with no workflow service: every call is refused
/// as `Unavailable`.
#[derive(Debug)]
struct Unconfigured;

impl Unconfigured {
    fn refusal<T>() -> Result<T, zeroship_workflow::WorkflowServiceError> {
        Err(zeroship_workflow::WorkflowServiceError::Unavailable(
            "no workflow service is configured on this worker".into(),
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl zeroship_workflow::backend::WorkflowBackend for Unconfigured {
    async fn start(
        &self,
        _workflow_name: String,
        _input: serde_json::Value,
        _options: zeroship_workflow::operations::StartOptions,
    ) -> Result<zeroship_workflow::operations::StartedRun, zeroship_workflow::WorkflowServiceError>
    {
        Self::refusal()
    }

    async fn status(
        &self,
        _run_id: String,
    ) -> Result<zeroship_workflow::operations::RunStatus, zeroship_workflow::WorkflowServiceError>
    {
        Self::refusal()
    }

    async fn signal(
        &self,
        _run_id: String,
        _options: zeroship_workflow::operations::SignalOptions,
    ) -> Result<
        zeroship_workflow::operations::DeliveredSignal,
        zeroship_workflow::WorkflowServiceError,
    > {
        Self::refusal()
    }

    async fn transition(
        &self,
        _run_id: String,
        _op: zeroship_workflow::operations::RunOperation,
    ) -> Result<
        zeroship_workflow::operations::TransitionedRun,
        zeroship_workflow::WorkflowServiceError,
    > {
        Self::refusal()
    }

    async fn restart(
        &self,
        _run_id: String,
        _options: zeroship_workflow::operations::RestartOptions,
    ) -> Result<zeroship_workflow::operations::RestartedRun, zeroship_workflow::WorkflowServiceError>
    {
        Self::refusal()
    }

    async fn read_step_output(
        &self,
        _run_id: String,
        _name: String,
        _occurrence: u32,
    ) -> Result<Vec<u8>, zeroship_workflow::WorkflowServiceError> {
        Self::refusal()
    }

    async fn read_output(
        &self,
        _run_id: String,
    ) -> Result<Vec<u8>, zeroship_workflow::WorkflowServiceError> {
        Self::refusal()
    }
}
