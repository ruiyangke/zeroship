#![allow(dead_code)]

use std::collections::HashMap;

use zeroship_runtime::{init_v8, ModuleEntry, RequestResult};
use zeroship_runtime::runtime::{Runtime, DispatchOutcome, RuntimeLimits};

/// Create a module list from a single JS source string.
pub fn m(source: &str) -> Vec<ModuleEntry> {
    vec![ModuleEntry {
        specifier: "index.js".into(),
        source: source.into(),
    }]
}

/// Shorthand: empty env vars for tests.
pub fn no_env() -> HashMap<String, String> {
    HashMap::new()
}

/// Create a Runtime and dispatch a single RPC request (URL-path wire).
pub fn dispatch(modules: Vec<ModuleEntry>, method: &str, args_json: &str) -> Result<RequestResult, String> {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    runtime.dispatch_rpc(method, args_json)
}

/// Create a Runtime, dispatch multiple requests sequentially (URL-path wire).
pub fn dispatch_multi(
    modules: Vec<ModuleEntry>,
    requests: &[(&str, &str)],
) -> Vec<Result<RequestResult, String>> {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    requests
        .iter()
        .map(|(method, args)| runtime.dispatch_rpc(method, args))
        .collect()
}

/// Create a Runtime with env vars and dispatch a single RPC request.
pub fn dispatch_with_env(
    modules: Vec<ModuleEntry>,
    env: HashMap<String, String>,
    method: &str,
    args_json: &str,
) -> Result<RequestResult, String> {
    init_v8();
    let runtime = Runtime::builder().modules(modules).env_vars(env).build();
    runtime.dispatch_rpc(method, args_json)
}

/// Helper: dispatch an HTTP request and extract the body from a sync HttpComplete outcome.
pub fn dispatch_http_sync(
    modules: Vec<ModuleEntry>,
    method: &str,
    url: &str,
    headers_json: &str,
    body: &str,
) -> Option<(u16, Vec<(String, String)>, String)> {
    init_v8();
    let runtime = Runtime::builder().modules(modules).build();
    match runtime.dispatch_http(method, url, headers_json, body, None) {
        DispatchOutcome::HttpComplete { status, headers, body, .. } => {
            Some((status, headers, body))
        }
        DispatchOutcome::Complete(Err(e)) if e.message.contains("No onRequest") => None,
        _ => panic!("unexpected dispatch_http outcome"),
    }
}

// Keep limits alias accessible from tests in case a future test wants it.
#[allow(dead_code)]
pub fn default_limits() -> RuntimeLimits {
    RuntimeLimits::default()
}
