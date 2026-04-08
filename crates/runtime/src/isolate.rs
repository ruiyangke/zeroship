//! Per-request V8 isolate — thin wrapper around `Runtime`.
//!
//! `Isolate` owns a `Runtime` and a current-thread tokio runtime.
//! `execute_request` sends JSON-RPC through the Runtime's channel.
//! `execute_http` registers a JS wrapper on `__rpc.__httpDispatch` and
//! dispatches HTTP calls as JSON-RPC too.
//!
//! `IsolatePool` is an object pool of `Isolate`s for the per-request model.

use std::collections::HashMap;

use tokio_util::sync::CancellationToken;

use crate::init::{HttpResult, RequestResult};
use crate::modules::ModuleEntry;
use crate::runtime::Runtime;
use crate::state::IncomingRequest;

// ---------------------------------------------------------------------------
// HTTP dispatch JS wrapper
// ---------------------------------------------------------------------------

/// JS function registered as `__rpc.__httpDispatch`.
/// Takes (method, url, headers_json, body) and returns {status, headers, body}.
/// The dispatch function wraps this in JSON-RPC format automatically.
const HTTP_DISPATCH_JS: &str = r#"(function(method, url, headers_json, body) {
    function __ensureBody(resp) {
        if (resp && resp._isStreamBody && resp.body) {
            return resp.text().then(function(t) {
                resp._bodyText = t;
                resp._isStreamBody = false;
                return resp;
            });
        }
        return resp;
    }
    function __extract(resp) {
        if (!resp || resp.status === undefined) resp = new Response(String(resp), {status:200});
        var h = [];
        if (resp.headers && resp.headers._map) {
            var map = resp.headers._map;
            var keys = Object.keys(map);
            for (var i = 0; i < keys.length; i++) {
                var arr = map[keys[i]];
                for (var j = 0; j < arr.length; j++) h.push([keys[i], arr[j]]);
            }
        }
        return {status: resp.status, headers: h, body: resp._bodyText || ""};
    }
    try {
        var handler = __rpc.onRequest;
        var map = Object.create(null);
        if (headers_json) {
            var arr = JSON.parse(headers_json);
            for (var i = 0; i < arr.length; i++) {
                var k = arr[i][0].toLowerCase(), v = arr[i][1];
                if (map[k]) map[k].push(v); else map[k] = [v];
            }
        }
        var reqInit = { method: method, headers: Headers._fromTrusted(map) };
        if (body && method !== "GET" && method !== "HEAD") reqInit.body = body;
        var req = new Request(url, reqInit);
        var result = handler(req);
        if (result && typeof result.then === "function") {
            return result.then(function(resp) {
                return Promise.resolve(__ensureBody(resp)).then(__extract);
            }, function(e) {
                return __extract(new Response(e.message || String(e), {status:500}));
            });
        }
        var ensured = __ensureBody(result);
        if (ensured && typeof ensured.then === "function") {
            return ensured.then(__extract);
        }
        return __extract(result);
    } catch(e) {
        return __extract(new Response(e.message || String(e), {status:500}));
    }
})"#;

// ---------------------------------------------------------------------------
// Isolate
// ---------------------------------------------------------------------------

/// A V8 isolate with persistent context -- compiled code stays across requests.
/// Thin wrapper around `Runtime`: sends requests via channel and drives the
/// Runtime's event loop with a current-thread tokio runtime.
pub struct Isolate {
    runtime: Runtime,
    local_rt: tokio::runtime::Runtime,
    request_tx: tokio::sync::mpsc::Sender<IncomingRequest>,
    /// Kept alive so `runtime.run()` doesn't exit via the shutdown branch.
    _shutdown: CancellationToken,
    next_id: u64,
    has_http: Option<bool>,
}

impl Isolate {
    /// Create a new isolate for ES module format (`export function ...`).
    /// Call `init_v8()` before creating isolates.
    ///
    /// `env_vars` — per-app environment variables accessible via `env.get(key)`.
    pub fn new(modules: Vec<ModuleEntry>, env_vars: HashMap<String, String>) -> Self {
        let local_rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let (request_tx, request_rx) = tokio::sync::mpsc::channel(64);
        let shutdown = CancellationToken::new();

        let runtime = Runtime::new(
            modules,
            request_rx,
            shutdown.clone(),
            None,
            None,
            env_vars,
            None,
        );

        Self {
            runtime,
            local_rt,
            request_tx,
            _shutdown: shutdown,
            next_id: 0,
            has_http: None,
        }
    }

    /// Lazy initialization + register HTTP dispatch if onRequest is exported.
    fn ensure_initialized(&mut self) {
        if self.runtime.initialized {
            return;
        }

        self.runtime.ensure_initialized();

        // Check if onRequest is exported and register __httpDispatch
        let has_http = {
            v8::scope!(let handle_scope, &mut self.runtime.isolate);
            let context = v8::Local::new(handle_scope, &self.runtime.context);
            let scope = &mut v8::ContextScope::new(handle_scope, context);

            let global = context.global(scope);
            let rpc_key = v8::String::new(scope, "__rpc").unwrap();
            let has = global
                .get(scope, rpc_key.into())
                .and_then(|rpc| rpc.to_object(scope))
                .and_then(|obj| {
                    let key = v8::String::new(scope, "onRequest").unwrap();
                    obj.get(scope, key.into())
                })
                .map(|v| v.is_function())
                .unwrap_or(false);

            if has {
                // Compile and register __httpDispatch on __rpc
                let code = v8::String::new(scope, HTTP_DISPATCH_JS).unwrap();
                let script = v8::Script::compile(scope, code, None).unwrap();
                let func_val = script.run(scope).unwrap();

                let rpc_obj = global
                    .get(scope, rpc_key.into())
                    .unwrap()
                    .to_object(scope)
                    .unwrap();
                let dispatch_key = v8::String::new(scope, "__httpDispatch").unwrap();
                rpc_obj.set(scope, dispatch_key.into(), func_val);
            }

            has
        };

        self.has_http = Some(has_http);
    }

    /// Check if the app exports an onRequest handler.
    pub fn has_http_handler(&mut self) -> bool {
        self.ensure_initialized();
        self.has_http.unwrap_or(false)
    }

    /// Execute an HTTP request via the onRequest handler.
    /// Returns None if onRequest is not exported.
    pub fn execute_http(
        &mut self,
        method: &str,
        url: &str,
        headers_json: &str,
        body: &str,
    ) -> Option<Result<HttpResult, String>> {
        self.ensure_initialized();

        if !self.has_http.unwrap_or(false) {
            return None;
        }

        // Encode as JSON-RPC call to __httpDispatch
        let request_json = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "__httpDispatch",
            "params": [method, url, headers_json, body],
            "id": 0
        })
        .to_string();

        let result = self.execute_request(&request_json);

        Some(match result {
            Err(e) => Err(e),
            Ok(rr) => {
                // Parse the JSON-RPC result to extract {status, headers, body}
                match serde_json::from_str::<serde_json::Value>(&rr.json) {
                    Ok(val) => {
                        if let Some(err) = val.get("error") {
                            let msg = err
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown error");
                            Err(msg.to_string())
                        } else if let Some(result) = val.get("result") {
                            let status = result
                                .get("status")
                                .and_then(|s| s.as_u64())
                                .unwrap_or(200) as u16;
                            let body = result
                                .get("body")
                                .and_then(|b| b.as_str())
                                .unwrap_or("")
                                .to_string();
                            let headers: Vec<(String, String)> = result
                                .get("headers")
                                .and_then(|h| h.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|pair| {
                                            let a = pair.as_array()?;
                                            Some((
                                                a.first()?.as_str()?.to_string(),
                                                a.get(1)?.as_str()?.to_string(),
                                            ))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();

                            Ok(HttpResult {
                                status,
                                headers,
                                body,
                                cpu_time: rr.cpu_time,
                                wall_time: rr.wall_time,
                                logs: rr.logs,
                            })
                        } else {
                            Err("missing result in JSON-RPC response".to_string())
                        }
                    }
                    Err(e) => Err(format!("failed to parse response: {e}")),
                }
            }
        })
    }

    /// Execute a single RPC request. Returns the JSON response + timing info.
    pub fn execute_request(&mut self, request_json: &str) -> Result<RequestResult, String> {
        self.ensure_initialized();

        self.next_id += 1;
        let id = self.next_id;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        self.request_tx
            .try_send(IncomingRequest {
                id,
                body: request_json.to_string(),
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .map_err(|e| format!("failed to send request: {e}"))?;

        // Drive the Runtime event loop until the reply arrives.
        // `runtime.run()` processes ops/timers/requests; reply_rx resolves
        // when the Runtime finishes handling our request.
        let runtime = &mut self.runtime;
        self.local_rt.block_on(async {
            tokio::select! {
                _ = runtime.run() => {
                    // Runtime exited (shutdown or channel closed).
                    // The reply should have been sent before exit.
                    Err("runtime exited before reply".to_string())
                }
                result = reply_rx => {
                    result.map_err(|_| "reply channel dropped".to_string())?
                }
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Isolate pool
// ---------------------------------------------------------------------------

/// Pool of V8 isolates for per-request model.
/// Each isolate has a persistent context with pre-compiled handlers.
pub struct IsolatePool {
    available: std::sync::Mutex<Vec<Isolate>>,
    modules: Vec<ModuleEntry>,
    env_vars: HashMap<String, String>,
    max_size: usize,
}

// SAFETY: Isolates are only accessed by one thread at a time via the Mutex.
#[allow(unsafe_code)]
unsafe impl Send for IsolatePool {}
#[allow(unsafe_code)]
unsafe impl Sync for IsolatePool {}

impl IsolatePool {
    pub fn new(
        modules: Vec<ModuleEntry>,
        env_vars: HashMap<String, String>,
        max_size: usize,
    ) -> Self {
        Self {
            available: std::sync::Mutex::new(Vec::new()),
            modules,
            env_vars,
            max_size,
        }
    }

    pub fn execute(&self, request_json: &str) -> Result<RequestResult, String> {
        let mut isolate = {
            let mut pool = self.available.lock().unwrap();
            pool.pop()
        }
        .unwrap_or_else(|| Isolate::new(self.modules.clone(), self.env_vars.clone()));

        let result = isolate.execute_request(request_json);

        {
            let mut pool = self.available.lock().unwrap();
            if pool.len() < self.max_size {
                pool.push(isolate);
            }
        }

        result
    }
}
