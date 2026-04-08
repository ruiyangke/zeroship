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
use crate::state::{IncomingRequest, RequestKind, RequestReply};

// HTTP dispatch is now handled natively by Runtime.handle_http_request()
// which calls onRequest(Request) directly and inspects the V8 Response object.

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

    /// Lazy initialization — detect onRequest handler availability.
    fn ensure_initialized(&mut self) {
        if self.runtime.initialized {
            return;
        }

        self.runtime.ensure_initialized();

        // Runtime.ensure_initialized() already detects onRequest and caches
        // http_handler_fn. We just check whether it was found.
        self.has_http = Some(self.runtime.http_handler_fn.is_some());
    }

    /// Check if the app exports an onRequest handler.
    pub fn has_http_handler(&mut self) -> bool {
        self.ensure_initialized();
        self.has_http.unwrap_or(false)
    }

    /// Execute an HTTP request via the onRequest handler.
    /// Returns None if onRequest is not exported.
    ///
    /// Uses native HTTP dispatch: calls `onRequest(Request)` directly and
    /// inspects the returned V8 Response object in Rust.
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

        self.next_id += 1;
        let id = self.next_id;
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

        self.request_tx
            .try_send(IncomingRequest {
                id,
                kind: RequestKind::Http {
                    method: method.to_string(),
                    url: url.to_string(),
                    headers: headers_json.to_string(),
                    body: body.to_string(),
                },
                reply: reply_tx,
                cancel: CancellationToken::new(),
            })
            .ok()?;

        let runtime = &mut self.runtime;
        Some(self.local_rt.block_on(async {
            // Phase 1: drive runtime until we get the reply
            let reply = tokio::select! {
                _ = runtime.run() => {
                    return Err("runtime exited before reply".to_string());
                }
                result = reply_rx => {
                    result.map_err(|_| "reply channel dropped".to_string())?
                        .map_err(|e| e)?
                }
            };

            match reply {
                RequestReply::Complete(rr) => {
                    // The HTTP dispatch path wraps the result as JSON with {status, headers, body}
                    match serde_json::from_str::<serde_json::Value>(&rr.json) {
                        Ok(val) => {
                            let result = val.get("result").unwrap_or(&val);
                            let status = result.get("status")
                                .and_then(|s| s.as_u64())
                                .unwrap_or(200) as u16;
                            let body = result.get("body")
                                .and_then(|b| b.as_str())
                                .unwrap_or("")
                                .to_string();
                            let headers: Vec<(String, String)> = result
                                .get("headers")
                                .and_then(|h| h.as_array())
                                .map(|arr| {
                                    arr.iter().filter_map(|pair| {
                                        let a = pair.as_array()?;
                                        Some((
                                            a.first()?.as_str()?.to_string(),
                                            a.get(1)?.as_str()?.to_string(),
                                        ))
                                    }).collect()
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
                        }
                        Err(e) => Err(format!("failed to parse response: {e}")),
                    }
                }
                RequestReply::Stream(stream) => {
                    // Phase 2: streaming response — drive runtime while collecting body
                    let status = stream.status;
                    let headers = stream.headers;
                    let cpu_time = stream.cpu_time;
                    let logs = stream.logs;
                    let mut body_rx = stream.body_rx;
                    let wall_start = std::time::Instant::now();
                    let mut body_parts: Vec<bytes::Bytes> = Vec::new();

                    // Drive runtime to push stream chunks while collecting body
                    loop {
                        tokio::select! {
                            _ = runtime.run() => {
                                // Runtime exited — drain remaining chunks
                                while let Some(chunk) = body_rx.recv().await {
                                    body_parts.push(chunk);
                                }
                                break;
                            }
                            chunk = body_rx.recv() => {
                                match chunk {
                                    Some(data) => body_parts.push(data),
                                    None => break, // stream closed
                                }
                            }
                        }
                    }

                    let body: String = body_parts.iter()
                        .map(|b| String::from_utf8_lossy(b).to_string())
                        .collect();

                    Ok(HttpResult {
                        status,
                        headers,
                        body,
                        cpu_time,
                        wall_time: wall_start.elapsed(),
                        logs,
                    })
                }
            }
        }))
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
                kind: RequestKind::Rpc(request_json.to_string()),
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
                    match result.map_err(|_| "reply channel dropped".to_string())? {
                        Ok(RequestReply::Complete(r)) => Ok(r),
                        Ok(RequestReply::Stream(_)) => Err("Unexpected streaming response".into()),
                        Err(e) => Err(e),
                    }
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
