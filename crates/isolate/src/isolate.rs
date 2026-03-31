//! V8 isolate wrapper — creates a `JsRuntime` with plugins and handles RPC dispatch.

use appbase_core::plugin::{Plugin, PluginContext, PluginMeter, PluginQuota};
use appbase_core::types::RpcResult;
use deno_core::*;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::permissions::appbase_permissions_container;

// ---------------------------------------------------------------------------
// Concurrent RPC channel types (stored in OpState)
// ---------------------------------------------------------------------------

/// Shared receiver for incoming RPC requests (request_id, body_json).
/// Wrapped in `Rc<tokio::sync::Mutex>` so the async op can hold it across .await.
pub struct SharedRpcReceiver(pub Rc<tokio::sync::Mutex<mpsc::Receiver<(u64, String)>>>);

/// Pending reply senders keyed by request_id.
/// JS calls `op_rpc_respond(id, json)` which looks up and fires the oneshot.
pub struct RpcPendingReplies(pub Rc<RefCell<HashMap<u64, oneshot::Sender<Result<RpcResult, String>>>>>);

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

/// Stub: op_tls_peer_certificate is defined in deno_node but imported by deno_net's JS.
/// We don't include deno_node, so we stub it to avoid "does not provide an export" errors.
#[op2]
#[buffer]
pub fn op_tls_peer_certificate(_rid: u32, _detailed: bool) -> Vec<u8> {
    vec![] // stub — we don't support TLS peer certificate inspection
}

/// Async op: JS calls this to receive the next RPC request.
/// Returns [requestId, bodyJson]. Yields to event loop when no requests are pending.
/// Returns null when the channel is closed (shutdown).
///
/// Request IDs use `f64` (`#[number]`) to avoid truncation: `u64` values up to
/// 2^53 are represented exactly, which is sufficient for ~285 years at 1M req/s.
#[op2]
#[serde]
pub async fn op_rpc_recv(
    state: Rc<RefCell<OpState>>,
) -> Result<Option<(f64, String)>, deno_error::JsErrorBox> {
    let rx = {
        let s = state.borrow();
        s.borrow::<SharedRpcReceiver>().0.clone()
    };
    let mut guard = rx.lock().await;
    match guard.recv().await {
        Some((id, body)) => Ok(Some((id as f64, body))),
        None => Ok(None), // channel closed — shutdown
    }
}

/// Sync op: JS calls this to send a response for a specific request_id.
///
/// Uses `#[number]` (f64) to match `op_rpc_recv` — avoids `#[smi]` truncation at 2^31.
#[op2(fast)]
pub fn op_rpc_respond(state: &mut OpState, #[number] request_id: u64, #[string] result: &str) {
    let pending = state.borrow::<RpcPendingReplies>();
    if let Some(tx) = pending.0.borrow_mut().remove(&request_id) {
        let _ = tx.send(Ok(RpcResult {
            json: result.to_string(),
            cpu_time: Duration::ZERO, // per-request CPU not available in concurrent mode
        }));
    }
    // Notify the global watchdog that a request completed
    if let Some(entry) = state.try_borrow::<crate::watchdog::OpWatchdogEntry>() {
        entry.0.end_request();
    }
}

// ---------------------------------------------------------------------------
// Runtime JS + creation
// ---------------------------------------------------------------------------

/// Core runtime JS — console + RPC dispatch. Plugins' JS bridges are appended.
static CORE_RUNTIME_JS: &str = include_str!("embed/runtime.js");

/// Default V8 heap limit per isolate: 128MB.
/// Prevents a single app from consuming all memory and crashing the process.
const DEFAULT_HEAP_LIMIT_MB: usize = 128;

/// Install the default TLS crypto provider (rustls + ring).
/// Safe to call multiple times — only the first call has effect.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Create a V8 isolate with plugins loaded.
///
/// Returns the `JsRuntime`. RPC channel state (`SharedRpcReceiver`, `RpcPendingReplies`)
/// must be placed into OpState by the caller (actor_loop) before executing user code.
pub fn create(
    plugins: &[Box<dyn Plugin>],
    app_id: &str,
    data_dir: &Path,
    meter: Arc<dyn PluginMeter>,
    quota: Arc<dyn PluginQuota>,
) -> Result<JsRuntime, String> {
    ensure_crypto_provider();

    // Collect ops from all plugins + core ops
    let mut all_ops = vec![op_tls_peer_certificate(), op_rpc_recv(), op_rpc_respond()];
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

    // Deno web platform extensions — minimum 6 for fetch().
    // Order matters: ESM imports resolved against already-loaded modules.
    // See refs/deno/runtime/worker.rs for Deno's full registration order.
    let mut extensions = vec![
        deno_telemetry::deno_telemetry::init(),     // fetch's JS imports tracing from this
        deno_webidl::deno_webidl::init(),            // WebIDL type conversions
        deno_web::deno_web::init(                    // Event, Blob, URL, timers
            Arc::new(deno_web::BlobStore::default()),
            None,
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        deno_tls::deno_tls::init(),                  // TLS primitives
        deno_net::deno_net::init(None, None),         // fetch's 22_http_client.js imports from this
        deno_fetch::deno_fetch::init(deno_fetch::Options {
            user_agent: "appbase/0.1".to_string(),
            ..Default::default()
        }),
    ];
    // Our own extension must come after deno's since our JS may reference fetch.
    extensions.push(ext);

    let heap_limit = DEFAULT_HEAP_LIMIT_MB * 1024 * 1024;
    let create_params = v8::CreateParams::default()
        .heap_limits(0, heap_limit);

    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions,
        create_params: Some(create_params),
        extension_transpiler: Some(Rc::new(|name, source| {
            transpile_extension(name, source)
        })),
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

    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
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

    Ok(runtime)
}

/// Cache for transpiled TypeScript extension sources.
/// Keyed by module name; value is the transpiled JS text.
/// Extensions (e.g. deno_telemetry) ship static TS that never changes at
/// runtime, so caching across isolate creations is safe.
static TRANSPILE_CACHE: OnceLock<std::sync::Mutex<HashMap<String, String>>> = OnceLock::new();

/// Transpile TypeScript extension sources to JavaScript.
/// Used by deno_telemetry which ships .ts files.
/// Results are cached so that only the first isolate pays the SWC cost.
fn transpile_extension(
    name: ModuleName,
    source: ModuleCodeString,
) -> Result<(ModuleCodeString, Option<SourceMapData>), deno_error::JsErrorBox> {
    use deno_ast::{MediaType, ParseParams, SourceMapOption};

    let media_type = MediaType::from_path(std::path::Path::new(&*name));
    match media_type {
        MediaType::TypeScript | MediaType::Tsx => {}
        // JS/MJS pass through without transpilation
        _ => return Ok((source, None)),
    }

    // Check cache
    let cache = TRANSPILE_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let key = name.to_string();
    {
        let c = cache.lock().unwrap();
        if let Some(cached) = c.get(&key) {
            return Ok((cached.clone().into(), None));
        }
    }

    // Not cached — transpile
    let parsed = deno_ast::parse_module(ParseParams {
        specifier: deno_core::url::Url::parse(&name).unwrap(),
        text: source.into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;

    let transpiled = parsed
        .transpile(
            &deno_ast::TranspileOptions {
                imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
                ..Default::default()
            },
            &deno_ast::TranspileModuleOptions::default(),
            &deno_ast::EmitOptions {
                source_map: SourceMapOption::None,
                ..Default::default()
            },
        )
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?
        .into_source();

    // Cache the result
    let text = transpiled.text.to_string();
    {
        let mut c = cache.lock().unwrap();
        c.insert(key, text.clone());
    }

    Ok((text.into(), None))
}
