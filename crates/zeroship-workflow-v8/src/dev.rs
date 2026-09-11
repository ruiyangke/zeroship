//! V8 executor for the local SQLite workflow engine.

use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::NativePlugin;
use zeroship_runtime::{
    EnvSnapshot, ModuleEntry, RequestCtx, Runtime, SettledWorkflow, WorkflowOutcome,
};

use zeroship_workflow::{WorkflowExecutor, WorkflowRpcError};

const DEV_DISPATCH_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct V8WorkflowExecutor {
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    plugins: Vec<Arc<dyn NativePlugin>>,
}

impl V8WorkflowExecutor {
    pub(crate) fn new(
        modules: Vec<ModuleEntry>,
        env_vars: HashMap<String, String>,
        plugins: Vec<Arc<dyn NativePlugin>>,
    ) -> Self {
        Self {
            modules,
            env_vars,
            plugins,
        }
    }
}

#[async_trait(?Send)]
impl WorkflowExecutor for V8WorkflowExecutor {
    async fn dispatch(&self, envelope: &str) -> Result<String, WorkflowRpcError> {
        zeroship_runtime::init_v8();
        let runtime = Runtime::builder()
            .modules(self.modules.clone())
            .env_vars(self.env_vars.clone())
            .plugins(self.plugins.clone())
            .build();
        runtime.start_pump();
        let env = env_snapshot_from_prefixed_vars(&self.env_vars);
        let ctx = RequestCtx::new(CancelFlag::new());
        match runtime.call_workflow_dispatch(envelope, &env, ctx) {
            WorkflowOutcome::Response { json, .. } => Ok(json),
            WorkflowOutcome::Pending { rx, cancel } => {
                runtime.notify_pump();
                match compio::time::timeout(DEV_DISPATCH_TIMEOUT, rx.recv()).await {
                    Ok(Ok(SettledWorkflow { json, .. })) => Ok(json),
                    Ok(Err(e)) => Err(WorkflowRpcError::Transport(e.message)),
                    Err(_) => {
                        cancel.cancel();
                        runtime.notify_pump();
                        Err(WorkflowRpcError::Timeout)
                    }
                }
            }
        }
    }
}

fn env_snapshot_from_prefixed_vars(env_vars: &HashMap<String, String>) -> EnvSnapshot {
    let mut vars = serde_json::Map::new();
    for (key, value) in env_vars {
        if let Some(name) = key.strip_prefix("ZS_VAR_").filter(|s| !s.is_empty()) {
            vars.insert(name.to_string(), Value::String(value.clone()));
        }
    }
    EnvSnapshot::vars_only(Value::Object(vars))
}
