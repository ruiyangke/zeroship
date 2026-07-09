//! Durable workflow plugin — `env.workflows`.
//!
//! `WorkflowPlugin::build_instance` mints a native `env.workflows` namespace
//! per isolate. The namespace carries only an app-scoped bearer token derived
//! in Rust as `HMAC-SHA256(control_key, app_id)`; the raw control key never
//! enters V8.

pub mod backend;
pub mod apply;
pub mod client;
pub mod dev;
pub mod engine;
pub mod errors;
pub mod store;
pub mod v8_class;

use std::path::Path;
use std::sync::Arc;

use backend::{HttpWorkflowBackend, SharedWorkflowBackend};
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub use backend::WorkflowBackend;
pub use client::{app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod, WorkflowHttpRequest};
pub use dev::DevWorkflowEngine;
pub use v8_class::{is_excluded_workflow_property, mint_workflows};

#[derive(Clone, Debug)]
enum WorkflowBackendFactory {
    Http {
        control_url: String,
        control_key: String,
    },
    DevSqlite {
        engine: Arc<DevWorkflowEngine>,
    },
}

#[derive(Clone, Debug)]
pub struct WorkflowPlugin {
    backend: WorkflowBackendFactory,
}

impl WorkflowPlugin {
    #[must_use]
    pub fn new(control_url: impl Into<String>, control_key: impl Into<String>) -> Self {
        Self {
            backend: WorkflowBackendFactory::Http {
                control_url: control_url.into(),
                control_key: control_key.into(),
            },
        }
    }

    /// Construct the local dev-tier workflow backend.
    ///
    /// This constructor is only called by `zeroship serve`. The production
    /// worker keeps using [`Self::new`], so the in-process engine is
    /// dev-only by construction.
    pub fn dev_sqlite(
        db_path: impl AsRef<Path>,
        modules: Vec<zeroship_runtime::ModuleEntry>,
        env_vars: std::collections::HashMap<String, String>,
        plugins: Vec<Arc<dyn NativePlugin>>,
    ) -> Result<Self, String> {
        let engine = DevWorkflowEngine::open(db_path, modules, env_vars, plugins)?;
        Ok(Self {
            backend: WorkflowBackendFactory::DevSqlite { engine },
        })
    }

    fn build_backend(&self, app_id: &str) -> SharedWorkflowBackend {
        match &self.backend {
            WorkflowBackendFactory::Http {
                control_url,
                control_key,
            } => {
                let token = client::app_scoped_token(control_key, app_id);
                Arc::new(HttpWorkflowBackend::new(WorkflowClientConfig::new(
                    control_url.clone(),
                    app_id.to_string(),
                    token,
                )))
            }
            WorkflowBackendFactory::DevSqlite { engine } => {
                engine.ensure_scheduler();
                Arc::new(engine.backend_for_app(app_id))
            }
        }
    }
}

impl NativePlugin for WorkflowPlugin {
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
