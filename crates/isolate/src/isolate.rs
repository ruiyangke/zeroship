//! V8 isolate wrapper — creates a `JsRuntime` with plugins and handles RPC dispatch.

use appbase_core::plugin::{Plugin, PluginContext, PluginMeter, PluginQuota};
use appbase_core::types::RpcResult;
use deno_core::*;
use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::cpu;
use crate::permissions::appbase_permissions_container;

/// Bidirectional string holder for Rust ↔ JS communication.
/// - Rust sets `request` before calling JS
/// - JS reads `request` via op, processes it, writes `response` via op
/// - Rust reads `response` after JS completes
/// This avoids embedding JSON in script strings (no compilation per request,
/// no template literal injection risk).
#[derive(Debug)]
pub struct RpcBridge {
    pub request: RefCell<String>,
    pub response: RefCell<String>,
}

/// Op: JS reads the RPC request JSON set by Rust.
#[op2]
#[string]
pub fn op_rpc_get_request(state: &mut OpState) -> String {
    let bridge = state.borrow::<Rc<RpcBridge>>();
    bridge.request.borrow().clone()
}

/// Op: JS writes the RPC response JSON back to Rust.
#[op2(fast)]
pub fn op_rpc_set_response(state: &mut OpState, #[string] result: &str) {
    let bridge = state.borrow::<Rc<RpcBridge>>();
    *bridge.response.borrow_mut() = result.to_string();
}

/// Core runtime JS — console + RPC dispatch. Plugins' JS bridges are appended.
static CORE_RUNTIME_JS: &str = include_str!("embed/runtime.js");

/// Default V8 heap limit per isolate: 128MB.
/// Prevents a single app from consuming all memory and crashing the process.
const DEFAULT_HEAP_LIMIT_MB: usize = 128;

/// Create a V8 isolate with plugins loaded.
///
/// Returns the `JsRuntime` and the `RpcBridge` for extracting RPC responses.
/// V8 heap is limited to 128MB by default to prevent runaway memory usage.
pub fn create(
    plugins: &[Box<dyn Plugin>],
    app_id: &str,
    data_dir: &Path,
    meter: Arc<dyn PluginMeter>,
    quota: Arc<dyn PluginQuota>,
) -> Result<(JsRuntime, Rc<RpcBridge>), String> {
    // Collect ops from all plugins + core ops
    let mut all_ops = vec![op_rpc_get_request(), op_rpc_set_response()];
    for plugin in plugins {
        all_ops.extend(plugin.ops());
    }

    // Build JS: core runtime + each plugin's bridge
    let mut js_parts = vec![CORE_RUNTIME_JS.to_string()];
    for plugin in plugins {
        let bridge = plugin.js_bridge();
        if !bridge.is_empty() {
            js_parts.push(format!("// Plugin: {}\n{}", plugin.name(), bridge));
        }
    }
    let full_js = js_parts.join("\n\n");

    let runtime_js = ExtensionFileSource::new_computed(
        "ext:appbase/runtime.js",
        Arc::from(full_js.as_str()),
    );
    let ext = Extension {
        name: "appbase_core_runtime",
        ops: Cow::Owned(all_ops),
        esm_files: Cow::Owned(vec![runtime_js]),
        esm_entry_point: Some("ext:appbase/runtime.js"),
        ..Default::default()
    };

    // Deno web platform extensions — order matters (deps before dependents).
    // These provide globalThis.fetch(), Request, Response, Headers, URL, etc.
    let mut extensions = vec![
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::init(
            Arc::new(deno_web::BlobStore::default()),
            None, // no base location URL
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        deno_net::deno_net::init(None, None),
        deno_fetch::deno_fetch::init(deno_fetch::Options::default()),
    ];
    // Our own extension must come after deno's since our JS may reference fetch.
    extensions.push(ext);

    let heap_limit = DEFAULT_HEAP_LIMIT_MB * 1024 * 1024;
    let create_params = v8::CreateParams::default()
        .heap_limits(0, heap_limit);

    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions,
        create_params: Some(create_params),
        ..Default::default()
    });

    // Register near-heap-limit callback: log warning before V8 crashes
    let app_id_for_cb = app_id.to_string();
    runtime.add_near_heap_limit_callback(move |current, initial| {
        eprintln!(
            "[isolate] [{app_id_for_cb}] WARN: V8 heap near limit — current={:.1}MB initial={:.1}MB",
            current as f64 / 1_048_576.0,
            initial as f64 / 1_048_576.0,
        );
        // Give V8 a small buffer to finish current operation before failing
        current + 5 * 1024 * 1024
    });

    let rpc_bridge = Rc::new(RpcBridge {
        request: RefCell::new(String::new()),
        response: RefCell::new(String::new()),
    });
    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        state.put(rpc_bridge.clone());
        // Provide an allow-all PermissionsContainer so deno_fetch ops can check permissions.
        state.put(appbase_permissions_container());

        // Initialize each plugin's state
        let mut ctx = PluginContext {
            op_state: &mut state,
            app_id,
            data_dir,
            meter,
            quota,
        };
        for plugin in plugins {
            plugin.init(&mut ctx);
        }
    }

    Ok((runtime, rpc_bridge))
}

/// Fixed RPC dispatch script — compiled ONCE by V8, reused for all requests.
/// Reads request JSON from Rust via op, dispatches, writes response back via op.
static RPC_DISPATCH_SCRIPT: &str = r#"(async () => {
    const requestJson = Deno.core.ops.op_rpc_get_request();
    const result = await globalThis.__handleRpc(requestJson);
    Deno.core.ops.op_rpc_set_response(result);
})()"#;

/// Execute an RPC request in the isolate and return the result with CPU timing.
///
/// Flow:
/// 1. Rust writes request JSON to RpcBridge.request
/// 2. V8 executes the fixed dispatch script (compiled once, reused)
/// 3. JS reads request via op_rpc_get_request, dispatches, writes via op_rpc_set_response
/// 4. Rust reads response from RpcBridge.response
///
/// No per-request script compilation. No JSON embedding in JS strings.
pub async fn handle_rpc(
    runtime: &mut JsRuntime,
    rpc_bridge: &Rc<RpcBridge>,
    request_json: &str,
    cpu_limit: Option<Duration>,
) -> Result<RpcResult, deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    // Step 1: Set request JSON in bridge (Rust → JS)
    *rpc_bridge.request.borrow_mut() = request_json.to_string();

    let cpu_before = cpu::thread_cpu_time();

    // Step 2: Execute the fixed dispatch script (no compilation — V8 caches it)
    runtime
        .execute_script("<rpc>", RPC_DISPATCH_SCRIPT)
        .map_err(err)?;
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(err)?;

    let cpu_time = cpu::thread_cpu_time().saturating_sub(cpu_before);

    // Step 3: Check CPU limit
    if let Some(max) = cpu_limit {
        if cpu_time > max {
            return Err(deno_error::JsErrorBox::generic(format!(
                "CPU time limit exceeded: {:.1}ms used, {:.1}ms allowed",
                cpu_time.as_secs_f64() * 1000.0,
                max.as_secs_f64() * 1000.0,
            )));
        }
    }

    // Step 4: Read response from bridge (JS → Rust)
    Ok(RpcResult {
        json: rpc_bridge.response.borrow().clone(),
        cpu_time,
    })
}
