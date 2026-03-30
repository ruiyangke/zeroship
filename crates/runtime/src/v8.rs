use deno_core::*;
use rusqlite::Connection;
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::ops;

// Holds the RPC result string, set by JS via op
pub struct RpcResult(pub RefCell<String>);

#[op2(fast)]
pub fn op_set_rpc_result(state: &mut OpState, #[string] result: &str) {
    let rpc = state.borrow::<Rc<RpcResult>>();
    *rpc.0.borrow_mut() = result.to_string();
}

// Static runtime JS — loaded once, shared across all runtime instances
static RUNTIME_JS: &str = include_str!("embed/runtime.js");

pub fn create_v8_runtime(db_path: &str) -> Result<(JsRuntime, Rc<RpcResult>), String> {
    let conn = Rc::new(Connection::open(db_path).map_err(|e| e.to_string())?);
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|e| e.to_string())?;

    let runtime_code: Arc<str> = Arc::from(RUNTIME_JS);
    let runtime_js = ExtensionFileSource::new_computed("ext:appbase/runtime.js", runtime_code);

    let ext = Extension {
        name: "appbase",
        ops: Cow::Owned(vec![
            ops::op_db_ensure_table(),
            ops::op_db_insert(),
            ops::op_db_find(),
            ops::op_db_update(),
            ops::op_db_delete(),
            op_set_rpc_result(),
        ]),
        esm_files: Cow::Owned(vec![runtime_js]),
        esm_entry_point: Some("ext:appbase/runtime.js"),
        ..Default::default()
    };

    let runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![ext],
        ..Default::default()
    });

    let rpc_result = Rc::new(RpcResult(RefCell::new(String::new())));
    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        state.put(conn);
        state.put(rpc_result.clone());
    }

    Ok((runtime, rpc_result))
}

/// Handle an RPC request by calling the pre-compiled __handleRpc function in JS.
/// The JS function parses the JSON, dispatches to user functions, and returns
/// the result via op_set_rpc_result (avoiding V8 value extraction complexity).
///
/// This is faster than execute_script because __handleRpc is already compiled
/// by V8 — we only compile a minimal call expression, not the full dispatch logic.
pub async fn handle_rpc(
    runtime: &mut JsRuntime,
    rpc_result: &Rc<RpcResult>,
    request_json: &str,
) -> Result<String, deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    // Escape the JSON string for embedding in JS — only need to handle
    // backslashes and backticks since we use template literals
    let escaped = request_json.replace('\\', "\\\\").replace('`', "\\`");

    // Minimal JS: call the pre-compiled function with the request string
    // __handleRpc is defined in runtime.js and already compiled by V8
    let script = format!(
        r#"(async () => {{
            const result = await globalThis.__handleRpc(`{escaped}`);
            Deno.core.ops.op_set_rpc_result(result);
        }})()"#
    );

    runtime.execute_script("<rpc>", script).map_err(err)?;
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(err)?;

    Ok(rpc_result.0.borrow().clone())
}
