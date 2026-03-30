use deno_core::*;
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// Core runtime result holder — used to pass RPC results from JS to Rust.
#[derive(Debug)]
pub struct RpcResult(pub RefCell<String>);

#[op2(fast)]
pub fn op_set_rpc_result(state: &mut OpState, #[string] result: &str) {
    let rpc = state.borrow::<Rc<RpcResult>>();
    *rpc.0.borrow_mut() = result.to_string();
}

/// Core runtime JS — console + RPC dispatch. No db or other primitives.
static CORE_RUNTIME_JS: &str = include_str!("embed/runtime.js");

/// Plugin interface: ops + JS bridge + state initialization.
pub trait Plugin: Send {
    fn name(&self) -> &str;
    fn ops(&self) -> Vec<OpDecl>;
    /// JS code injected into the V8 global scope (defines globals like `db`, `auth`, etc.)
    fn js_bridge(&self) -> &str;
    /// Called after runtime creation to inject plugin state into OpState.
    fn init_state(&self, state: &mut OpState);
}

/// Create a V8 runtime with the core RPC infrastructure + any plugins.
pub fn create_v8_runtime(
    plugins: &[Box<dyn Plugin>],
) -> Result<(JsRuntime, Rc<RpcResult>), String> {
    create_v8_runtime_inner(plugins, None)
}

/// Create a V8 runtime with a pre-built snapshot.
pub fn create_v8_runtime_with_snapshot(
    plugins: &[Box<dyn Plugin>],
    snapshot: &'static [u8],
) -> Result<(JsRuntime, Rc<RpcResult>), String> {
    create_v8_runtime_inner(plugins, Some(snapshot))
}

fn create_v8_runtime_inner(
    plugins: &[Box<dyn Plugin>],
    snapshot: Option<&'static [u8]>,
) -> Result<(JsRuntime, Rc<RpcResult>), String> {
    // Collect ops from all plugins + core op
    let mut all_ops = vec![op_set_rpc_result()];
    for plugin in plugins {
        all_ops.extend(plugin.ops());
    }

    // Build JS bridge: core runtime + each plugin's JS
    let mut js_parts = vec![CORE_RUNTIME_JS.to_string()];
    for plugin in plugins {
        let bridge = plugin.js_bridge();
        if !bridge.is_empty() {
            js_parts.push(format!("// Plugin: {}\n{}", plugin.name(), bridge));
        }
    }
    let full_js = js_parts.join("\n\n");

    let runtime = if let Some(snapshot_data) = snapshot {
        let ext = Extension {
            name: "appbase",
            ops: Cow::Owned(all_ops),
            ..Default::default()
        };
        JsRuntime::new(RuntimeOptions {
            startup_snapshot: Some(snapshot_data),
            extensions: vec![ext],
            ..Default::default()
        })
    } else {
        let runtime_js = ExtensionFileSource::new_computed(
            "ext:appbase/runtime.js",
            Arc::from(full_js.as_str()),
        );
        let ext = Extension {
            name: "appbase",
            ops: Cow::Owned(all_ops),
            esm_files: Cow::Owned(vec![runtime_js]),
            esm_entry_point: Some("ext:appbase/runtime.js"),
            ..Default::default()
        };
        JsRuntime::new(RuntimeOptions {
            extensions: vec![ext],
            ..Default::default()
        })
    };

    let rpc_result = Rc::new(RpcResult(RefCell::new(String::new())));
    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        state.put(rpc_result.clone());
        // Initialize each plugin's state
        for plugin in plugins {
            plugin.init_state(&mut state);
        }
    }

    Ok((runtime, rpc_result))
}

/// Create a snapshot with core + plugins baked in.
#[cfg(feature = "snapshot")]
pub fn create_snapshot(plugins: &[Box<dyn Plugin>]) -> Box<[u8]> {
    let mut all_ops = vec![op_set_rpc_result()];
    for plugin in plugins {
        all_ops.extend(plugin.ops());
    }

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
        name: "appbase",
        ops: Cow::Owned(all_ops),
        esm_files: Cow::Owned(vec![runtime_js]),
        esm_entry_point: Some("ext:appbase/runtime.js"),
        ..Default::default()
    };

    let runtime = JsRuntimeForSnapshot::new(RuntimeOptions {
        extensions: vec![ext],
        ..Default::default()
    });
    runtime.snapshot()
}

/// Result of an RPC call including CPU time consumed.
#[derive(Debug)]
pub struct RpcResponse {
    pub json: String,
    pub cpu_time: std::time::Duration,
}

pub async fn handle_rpc(
    runtime: &mut JsRuntime,
    rpc_result: &Rc<RpcResult>,
    request_json: &str,
    cpu_limits: &crate::cpu_timer::CpuLimits,
) -> Result<RpcResponse, deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    let escaped = request_json.replace('\\', "\\\\").replace('`', "\\`");

    let script = format!(
        r#"(async () => {{
            const result = await globalThis.__handleRpc(`{escaped}`);
            Deno.core.ops.op_set_rpc_result(result);
        }})()"#
    );

    let cpu_before = crate::cpu_timer::thread_cpu_time();

    runtime.execute_script("<rpc>", script).map_err(err)?;
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(err)?;

    let cpu_after = crate::cpu_timer::thread_cpu_time();
    let cpu_time = cpu_after.saturating_sub(cpu_before);

    if let Some(max) = cpu_limits.max_cpu_per_request {
        if cpu_time > max {
            return Err(deno_error::JsErrorBox::generic(format!(
                "CPU time limit exceeded: {:.1}ms used, {:.1}ms allowed",
                cpu_time.as_secs_f64() * 1000.0,
                max.as_secs_f64() * 1000.0,
            )));
        }
    }

    Ok(RpcResponse {
        json: rpc_result.0.borrow().clone(),
        cpu_time,
    })
}
