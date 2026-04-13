use std::collections::HashMap;

use zeroship_runtime::{init_v8, ModuleEntry, RequestResult};
use zeroship_runtime::runtime::{Runtime, DispatchOutcome};

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

/// Create a Runtime and dispatch a single RPC request.
pub fn dispatch(modules: Vec<ModuleEntry>, json: &str) -> Result<RequestResult, String> {
    init_v8();
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    runtime.dispatch_rpc(json)
}

/// Create a Runtime, dispatch multiple requests sequentially.
pub fn dispatch_multi(modules: Vec<ModuleEntry>, requests: &[&str]) -> Vec<Result<RequestResult, String>> {
    init_v8();
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    requests.iter().map(|json| runtime.dispatch_rpc(json)).collect()
}

/// Create a Runtime with env vars and dispatch a single RPC request.
pub fn dispatch_with_env(modules: Vec<ModuleEntry>, env: HashMap<String, String>, json: &str) -> Result<RequestResult, String> {
    init_v8();
    let mut runtime = Runtime::new_direct(modules, env, None, None);
    runtime.dispatch_rpc(json)
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
    let mut runtime = Runtime::new_direct(modules, no_env(), None, None);
    match runtime.dispatch_http(method, url, headers_json, body) {
        DispatchOutcome::HttpComplete { status, headers, body, .. } => {
            Some((status, headers, body))
        }
        DispatchOutcome::Complete(Err(e)) if e.contains("No onRequest") => None,
        _ => panic!("unexpected dispatch_http outcome"),
    }
}
