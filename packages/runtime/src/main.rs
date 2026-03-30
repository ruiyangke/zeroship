use deno_core::*;
use rusqlite::Connection;
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

mod ops;

/// Holds the last RPC result string, set by JS via op_set_rpc_result
pub struct RpcResult(pub RefCell<String>);

#[op2(fast)]
fn op_set_rpc_result(state: &mut OpState, #[string] result: &str) {
    let rpc = state.borrow::<Rc<RpcResult>>();
    *rpc.0.borrow_mut() = result.to_string();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let script_path = args.get(1).map(|s| s.as_str()).unwrap_or("app.js");
    let db_path = args.get(2).map(|s| s.as_str()).unwrap_or("appbase.db");
    let port: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    if let Err(error) = rt.block_on(run(script_path, db_path, port)) {
        eprintln!("[appbase-rt] Error: {}", error);
        std::process::exit(1);
    }
}

async fn run(script_path: &str, db_path: &str, port: u16) -> Result<(), deno_error::JsErrorBox> {
    let conn = Rc::new(
        Connection::open(db_path)
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?,
    );
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;

    let runtime_code: Arc<str> = Arc::from(include_str!("runtime.js"));
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

    let mut runtime = JsRuntime::new(RuntimeOptions {
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

    let user_code = std::fs::read_to_string(script_path)
        .map_err(|e| deno_error::JsErrorBox::generic(format!("Failed to read {}: {}", script_path, e)))?;
    runtime
        .execute_script("<user>", user_code)
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;

    eprintln!("[appbase-rt] Loaded: {}", script_path);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    eprintln!("[appbase-rt] Listening on http://localhost:{}", port);

    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;

        use tokio::io::AsyncReadExt;
        use tokio::io::AsyncWriteExt;

        let mut buf = vec![0u8; 65536];
        let n = stream
            .readable()
            .await
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
        let n = stream
            .try_read(&mut buf)
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
        let request_str = String::from_utf8_lossy(&buf[..n]);

        let response = if let Some(body_start) = request_str.find("\r\n\r\n") {
            let body = &request_str[body_start + 4..];

            if request_str.starts_with("POST /rpc") && !body.is_empty() {
                match handle_rpc(&mut runtime, &rpc_result, body).await {
                    Ok(json) => http_response(200, &json),
                    Err(e) => {
                        let err = format!(
                            r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{}"}},"id":null}}"#,
                            e
                        );
                        http_response(500, &err)
                    }
                }
            } else {
                http_response(200, r#"{"status":"appbase-rt running"}"#)
            }
        } else {
            "HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n".to_string()
        };

        stream
            .write_all(response.as_bytes())
            .await
            .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    }
}

fn http_response(status: u16, body: &str) -> String {
    let status_text = if status == 200 { "OK" } else { "Internal Server Error" };
    format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status, status_text, body.len(), body
    )
}

async fn handle_rpc(
    runtime: &mut JsRuntime,
    rpc_result: &Rc<RpcResult>,
    request_json: &str,
) -> Result<String, deno_error::JsErrorBox> {
    // JS dispatches the RPC call and stores result via op_set_rpc_result
    let script = format!(
        r#"(async () => {{
            const request = {};
            let result;
            if (Array.isArray(request)) {{
                result = JSON.stringify(await Promise.all(request.map(__dispatch)));
            }} else {{
                result = JSON.stringify(await __dispatch(request));
            }}
            Deno.core.ops.op_set_rpc_result(result);
        }})()"#,
        request_json
    );

    runtime
        .execute_script("<rpc>", script)
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;
    // Run event loop to resolve the async IIFE promise
    runtime
        .run_event_loop(Default::default())
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))?;

    // Read result that was stored by op_set_rpc_result
    let result = rpc_result.0.borrow().clone();
    Ok(result)
}
