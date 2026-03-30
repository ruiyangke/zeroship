use deno_core::*;
use rusqlite::Connection;
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

mod ops;

pub struct RpcResult(pub RefCell<String>);

#[op2(fast)]
fn op_set_rpc_result(state: &mut OpState, #[string] result: &str) {
    let rpc = state.borrow::<Rc<RpcResult>>();
    *rpc.0.borrow_mut() = result.to_string();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let script_path = args.get(1).map(|s| s.as_str()).unwrap_or("server.js");
    let port: u16 = args.iter()
        .find(|a| a.starts_with("--port="))
        .and_then(|a| a.strip_prefix("--port="))
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let db_path = args.iter()
        .find(|a| a.starts_with("--db="))
        .and_then(|a| a.strip_prefix("--db="))
        .unwrap_or("appbase.db");
    let static_file = args.iter()
        .find(|a| a.starts_with("--static="))
        .and_then(|a| a.strip_prefix("--static="))
        .map(|s| s.to_string());

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    if let Err(error) = rt.block_on(run(script_path, db_path, port, static_file)) {
        eprintln!("[appbase-rt] Error: {}", error);
        std::process::exit(1);
    }
}

async fn run(
    script_path: &str,
    db_path: &str,
    port: u16,
    static_file: Option<String>,
) -> Result<(), deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    // Load static HTML if provided
    let static_html: Option<Vec<u8>> = static_file.as_ref().map(|path| {
        std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("[appbase-rt] Warning: could not read {}: {}", path, e);
            b"<html><body>appbase</body></html>".to_vec()
        })
    });

    // SQLite
    let conn = Rc::new(Connection::open(db_path).map_err(err)?);
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").map_err(err)?;

    // V8 runtime
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

    // Load server script
    let user_code = std::fs::read_to_string(script_path)
        .map_err(|e| err(format!("Failed to read {}: {}", script_path, e)))?;
    runtime.execute_script("<user>", user_code).map_err(err)?;
    runtime.run_event_loop(Default::default()).await.map_err(err)?;

    eprintln!("[appbase-rt] Loaded: {}", script_path);

    // HTTP server
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await.map_err(err)?;
    eprintln!("[appbase-rt] http://localhost:{}", port);

    loop {
        let (mut stream, _) = listener.accept().await.map_err(err)?;

        use tokio::io::AsyncWriteExt;

        let mut buf = vec![0u8; 65536];
        stream.readable().await.map_err(err)?;
        let n = stream.try_read(&mut buf).map_err(err)?;
        let request_str = String::from_utf8_lossy(&buf[..n]);

        let response = if let Some(body_start) = request_str.find("\r\n\r\n") {
            let headers = &request_str[..body_start];
            let body = &request_str[body_start + 4..];

            if headers.starts_with("POST /rpc") && !body.is_empty() {
                match handle_rpc(&mut runtime, &rpc_result, body).await {
                    Ok(json) => http_response(200, "application/json", json.as_bytes()),
                    Err(e) => {
                        let err_body = format!(
                            r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{}"}},"id":null}}"#, e
                        );
                        http_response(500, "application/json", err_body.as_bytes())
                    }
                }
            } else if let Some(ref html) = static_html {
                http_response(200, "text/html", html)
            } else {
                http_response(200, "application/json", br#"{"status":"appbase-rt running"}"#)
            }
        } else {
            http_response(400, "text/plain", b"Bad Request")
        };

        stream.write_all(&response).await.map_err(err)?;
    }
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status, status_text, content_type, body.len()
    );
    let mut resp = header.into_bytes();
    resp.extend_from_slice(body);
    resp
}

async fn handle_rpc(
    runtime: &mut JsRuntime,
    rpc_result: &Rc<RpcResult>,
    request_json: &str,
) -> Result<String, deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

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

    runtime.execute_script("<rpc>", script).map_err(err)?;
    runtime.run_event_loop(Default::default()).await.map_err(err)?;

    Ok(rpc_result.0.borrow().clone())
}
