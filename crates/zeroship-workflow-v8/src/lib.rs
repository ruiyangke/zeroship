//! Durable workflow V8 binding — `env.workflows`.
//!
//! `WorkflowBinding::build_instance` mints a native `env.workflows` namespace
//! per isolate. Each namespace owns an app-scoped Rust backend; credentials
//! remain in the host. Service bindings validate the immutable runtime identity
//! before evaluating app code.

mod error;
mod executor;
mod loader;
pub mod v8_class;

pub use executor::{LoadedWorkflow, V8TaskExecutor, WorkflowRuntimeLoader};
pub use loader::AppRuntimeLoader;

use std::sync::Arc;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_workflow::backend::{HttpWorkflowBackend, SharedWorkflowBackend};

use zeroship_workflow::WorkflowClientConfig;

pub use v8_class::{is_excluded_workflow_property, mint_workflows};

#[derive(Clone, Debug)]
enum WorkflowBackendFactory {
    Service {
        backend: Arc<zeroship_workflow::service::AppBackend>,
    },
    Http {
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

    #[must_use]
    pub fn new(control_url: impl Into<String>, control_key: impl Into<String>) -> Self {
        Self {
            backend: WorkflowBackendFactory::Http {
                control_url: control_url.into(),
                control_key: control_key.into(),
            },
        }
    }

    fn build_backend(&self, app_id: &str) -> SharedWorkflowBackend {
        match &self.backend {
            WorkflowBackendFactory::Service { backend } => backend.clone(),
            WorkflowBackendFactory::Http {
                control_url,
                control_key,
            } => {
                let token = zeroship_workflow::app_scoped_token(control_key, app_id);
                Arc::new(HttpWorkflowBackend::new(WorkflowClientConfig::new(
                    control_url.clone(),
                    app_id.to_string(),
                    token,
                )))
            }
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
            if zeroship_runtime::plugin::runtime_app_uuid(scope) != Some(backend.app_id().uuid()) {
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
        app_id: &str,
    ) -> Option<v8::Local<'s, v8::Object>> {
        mint_workflows(scope, self.build_backend(app_id))
    }
}
