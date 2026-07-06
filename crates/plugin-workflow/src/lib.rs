//! Durable workflow plugin — `env.workflows`.
//!
//! `WorkflowPlugin::build_instance` mints a native `env.workflows` namespace
//! per isolate. The namespace carries only an app-scoped bearer token derived
//! in Rust as `HMAC-SHA256(control_key, app_id)`; the raw control key never
//! enters V8.

pub mod client;
pub mod v8_class;

use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};

pub use client::{app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod, WorkflowHttpRequest};
pub use v8_class::{is_excluded_workflow_property, mint_workflows};

#[derive(Clone, Debug)]
pub struct WorkflowPlugin {
    control_url: String,
    control_key: String,
}

impl WorkflowPlugin {
    #[must_use]
    pub fn new(control_url: impl Into<String>, control_key: impl Into<String>) -> Self {
        Self {
            control_url: control_url.into(),
            control_key: control_key.into(),
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
        let token = client::app_scoped_token(&self.control_key, app_id);
        let cfg = WorkflowClientConfig::new(self.control_url.clone(), app_id.to_string(), token);
        mint_workflows(scope, cfg)
    }
}
