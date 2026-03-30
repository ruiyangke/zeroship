//! V8 isolate wrapper — creates a `JsRuntime` with plugins and handles RPC dispatch.

use appbase_core::plugin::{Plugin, PluginContext};
use appbase_core::types::RpcResult;
use deno_core::*;
use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::cpu;

/// Internal holder for RPC result strings.
/// Used to pass results from JS to Rust via an op (avoids V8 HandleScope complexity).
#[derive(Debug)]
pub struct RpcResultHolder(pub RefCell<String>);

/// Op that JS calls to pass the RPC result back to Rust.
#[op2(fast)]
pub fn op_set_rpc_result(state: &mut OpState, #[string] result: &str) {
    let holder = state.borrow::<Rc<RpcResultHolder>>();
    *holder.0.borrow_mut() = result.to_string();
}

/// Core runtime JS — console + RPC dispatch. Plugins' JS bridges are appended.
static CORE_RUNTIME_JS: &str = include_str!("embed/runtime.js");

/// Create a V8 isolate with plugins loaded.
///
/// Returns the `JsRuntime` and the `RpcResultHolder` for extracting RPC responses.
pub fn create(
    plugins: &[Box<dyn Plugin>],
    app_id: &str,
    data_dir: &Path,
) -> Result<(JsRuntime, Rc<RpcResultHolder>), String> {
    // Collect ops from all plugins + core op
    let mut all_ops = vec![op_set_rpc_result()];
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
        name: "appbase",
        ops: Cow::Owned(all_ops),
        esm_files: Cow::Owned(vec![runtime_js]),
        esm_entry_point: Some("ext:appbase/runtime.js"),
        ..Default::default()
    };

    let runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![ext],
        ..Default::default()
    });

    let rpc_holder = Rc::new(RpcResultHolder(RefCell::new(String::new())));
    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        state.put(rpc_holder.clone());

        // Initialize each plugin's state
        let mut ctx = PluginContext {
            op_state: &mut state,
            app_id,
            data_dir,
        };
        for plugin in plugins {
            plugin.init(&mut ctx);
        }
    }

    Ok((runtime, rpc_holder))
}

/// Execute an RPC request in the isolate and return the result with CPU timing.
pub async fn handle_rpc(
    runtime: &mut JsRuntime,
    rpc_holder: &Rc<RpcResultHolder>,
    request_json: &str,
    cpu_limit: Option<Duration>,
) -> Result<RpcResult, deno_error::JsErrorBox> {
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

    let cpu_before = cpu::thread_cpu_time();

    runtime.execute_script("<rpc>", script).map_err(err)?;
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(err)?;

    let cpu_time = cpu::thread_cpu_time().saturating_sub(cpu_before);

    // Check CPU limit
    if let Some(max) = cpu_limit {
        if cpu_time > max {
            return Err(deno_error::JsErrorBox::generic(format!(
                "CPU time limit exceeded: {:.1}ms used, {:.1}ms allowed",
                cpu_time.as_secs_f64() * 1000.0,
                max.as_secs_f64() * 1000.0,
            )));
        }
    }

    Ok(RpcResult {
        json: rpc_holder.0.borrow().clone(),
        cpu_time,
    })
}
