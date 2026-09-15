//! Durable workflow V8 binding — `env.workflows`.
//!
//! `WorkflowBinding::build_instance` mints a native `env.workflows` namespace
//! per isolate. Each namespace owns an app-scoped Rust backend; credentials
//! remain in the host. Service bindings validate the immutable runtime identity
//! before evaluating app code. Ready bindings resolve the backend a workflow
//! host published for the isolate's own app, on every call.

mod error;
mod executor;
mod loader;
pub mod v8_class;

pub use executor::{LoadedWorkflow, V8TaskExecutor, WorkflowRuntimeLoader};
pub use loader::AppRuntimeLoader;

use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_workflow::backend::SharedWorkflowBackend;
use zeroship_workflow::service::runner::ready::ReadyApps;

pub use v8_class::{is_excluded_workflow_property, mint_workflows};

#[derive(Clone, Debug)]
enum WorkflowBackendFactory {
    Service {
        backend: Arc<zeroship_workflow::service::AppBackend>,
    },
    Ready {
        apps: ReadyApps,
    },
    /// Control's workflow API, reached with an app-scoped token derived from
    /// the shared control key. Only the Control-driven advance path's replay
    /// isolates use it; slice 6 of the workflow worker proposal deletes both.
    Control {
        control_url: String,
        control_key: String,
    },
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

    /// Bind each isolate to the backend a workflow host published for the
    /// runtime's own app. An isolate of an app that is unknown or not ready
    /// on this process receives a retryable refusal from every call.
    #[must_use]
    pub fn ready(apps: ReadyApps) -> Self {
        Self {
            backend: WorkflowBackendFactory::Ready { apps },
        }
    }

    /// Bind each isolate to Control's workflow API under a token derived for
    /// its own app. The raw key stays in the host and never reaches V8.
    #[must_use]
    pub fn new(control_url: impl Into<String>, control_key: impl Into<String>) -> Self {
        Self {
            backend: WorkflowBackendFactory::Control {
                control_url: control_url.into(),
                control_key: control_key.into(),
            },
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
        if let WorkflowBackendFactory::Service { backend } = &self.backend {
            if zeroship_runtime::plugin::runtime_app_identity(scope).as_ref()
                != Some(backend.app_id())
            {
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

    fn build_instance<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        _app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        let backend: SharedWorkflowBackend = match &self.backend {
            WorkflowBackendFactory::Service { backend } => backend.clone(),
            // The immutable identity the runtime was built with selects the
            // app; creator-visible environment values cannot.
            WorkflowBackendFactory::Ready { apps } => {
                apps.backend(zeroship_runtime::plugin::runtime_app_identity(scope)?)
            }
            WorkflowBackendFactory::Control {
                control_url,
                control_key,
            } => {
                let app = zeroship_runtime::plugin::runtime_app_identity(scope)?;
                let token = zeroship_workflow::app_scoped_token(control_key, app.as_str());
                Arc::new(zeroship_workflow::backend::HttpWorkflowBackend::new(
                    zeroship_workflow::WorkflowClientConfig::new(
                        control_url.clone(),
                        app.as_str().to_owned(),
                        token,
                    ),
                ))
            }
        };
        mint_workflows(scope, backend)
    }
}
