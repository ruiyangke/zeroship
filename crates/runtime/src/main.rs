use deno_core::*;
use rusqlite::Connection;
use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;

mod ops;

pub struct RpcResult(pub RefCell<String>);

#[op2(fast)]
fn op_set_rpc_result(state: &mut OpState, #[string] result: &str) {
    let rpc = state.borrow::<Rc<RpcResult>>();
    *rpc.0.borrow_mut() = result.to_string();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("help");

    match command {
        "serve" => {
            // Production: serve pre-compiled files
            let script = args.get(2).expect("Usage: appbase-rt serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
            let port = parse_flag(&args, "--port=").unwrap_or(3000);
            let db_path = parse_flag_str(&args, "--db=").unwrap_or("appbase.db".into());
            let static_file = parse_flag_str(&args, "--static=");

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            if let Err(e) = rt.block_on(serve(script, &db_path, port, static_file.as_deref())) {
                eprintln!("[appbase-rt] Error: {}", e);
                std::process::exit(1);
            }
        }
        "dev" => {
            // Dev mode: compile + serve + watch + WS reload
            let entry = args.get(2).expect("Usage: appbase-rt dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
            let port = parse_flag(&args, "--port=").unwrap_or(3000);
            let compiler = parse_flag_str(&args, "--compiler=").unwrap_or("appbase-compile".into());
            let minify = args.iter().any(|a| a == "--minify");

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            if let Err(e) = rt.block_on(dev(entry, port, &compiler, minify)) {
                eprintln!("[appbase-rt] Error: {}", e);
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("Usage:");
            eprintln!("  appbase-rt serve <server.js> [--static=index.html] [--port=3000] [--db=appbase.db]");
            eprintln!("  appbase-rt dev <app.jsx> [--port=3000] [--compiler=appbase-compile]");
        }
    }
}

fn parse_flag(args: &[String], prefix: &str) -> Option<u16> {
    args.iter().find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .and_then(|s| s.parse().ok())
}

fn parse_flag_str(args: &[String], prefix: &str) -> Option<String> {
    args.iter().find(|a| a.starts_with(prefix))
        .and_then(|a| a.strip_prefix(prefix))
        .map(|s| s.to_string())
}

// --- Dev mode ---

async fn dev(entry: &str, port: u16, compiler_bin: &str, minify: bool) -> Result<(), deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    let outdir = ".dist";

    // Initial compile
    eprintln!("[appbase] Compiling {}...", entry);
    run_compiler(compiler_bin, entry, outdir, minify).map_err(err)?;

    let server_js = std::fs::read_to_string(format!("{}/server.js", outdir)).unwrap_or_default();
    let html = Arc::new(Mutex::new(
        std::fs::read(format!("{}/index.html", outdir)).map_err(err)?
    ));

    // V8 runtime
    let (mut runtime, rpc_result) = create_v8_runtime("appbase.db").map_err(err)?;

    if !server_js.is_empty() {
        runtime.execute_script("<server>", server_js).map_err(err)?;
        runtime.run_event_loop(Default::default()).await.map_err(err)?;
    }

    eprintln!("[appbase] Server loaded");

    // Broadcast channel for reload notifications
    let (reload_tx, _) = broadcast::channel::<()>(16);
    let reload_tx2 = reload_tx.clone();

    // Source storage for save endpoint
    let source = Arc::new(Mutex::new(std::fs::read_to_string(entry).map_err(err)?));
    let entry_path = Arc::new(entry.to_string());

    // HTTP + WS server
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await.map_err(err)?;
    eprintln!("[appbase] Dev server: http://localhost:{}", port);

    loop {
        let (mut stream, _) = listener.accept().await.map_err(err)?;

        use tokio::io::AsyncWriteExt;

        let mut buf = vec![0u8; 65536];
        stream.readable().await.map_err(err)?;
        let n = stream.try_read(&mut buf).map_err(err)?;
        if n == 0 { continue; }
        let request_str = String::from_utf8_lossy(&buf[..n]);

        // Check for WebSocket upgrade
        if request_str.contains("Upgrade: websocket") || request_str.contains("upgrade: websocket") {
            // Handle WebSocket connection
            let stream = tokio::net::TcpStream::from_std(stream.into_std().map_err(err)?).map_err(err)?;
            let mut rx = reload_tx.subscribe();
            tokio::spawn(async move {
                if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                    use futures_util::{SinkExt, StreamExt};
                    use tokio_tungstenite::tungstenite::Message;
                    let (mut write, _read) = ws.split();
                    while rx.recv().await.is_ok() {
                        let _ = write.send(Message::Text("{\"type\":\"reload\"}".into())).await;
                    }
                }
            });
            continue;
        }

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
            } else if headers.starts_with("POST /__dev/save") {
                // Save + recompile
                match handle_save(body, &entry_path, compiler_bin, outdir, minify, &html, &mut runtime, &rpc_result, &reload_tx2).await {
                    Ok(resp) => http_response(200, "application/json", resp.as_bytes()),
                    Err(e) => http_response(400, "application/json",
                        format!(r#"{{"error":"{}"}}"#, e).as_bytes()),
                }
            } else if headers.starts_with("GET /__dev/source") {
                let src = source.lock().unwrap().clone();
                let json = serde_json::json!({ "source": src, "filename": entry_path.as_str() });
                http_response(200, "application/json", json.to_string().as_bytes())
            } else {
                // Serve HTML with WS reload script injected
                let h = html.lock().unwrap().clone();
                let with_ws = inject_ws_reload(&h, port);
                http_response(200, "text/html", &with_ws)
            }
        } else {
            http_response(400, "text/plain", b"Bad Request")
        };

        stream.write_all(&response).await.map_err(err)?;
    }
}

async fn handle_save(
    body: &str,
    entry_path: &str,
    compiler_bin: &str,
    outdir: &str,
    minify: bool,
    html: &Arc<Mutex<Vec<u8>>>,
    runtime: &mut JsRuntime,
    rpc_result: &Rc<RpcResult>,
    reload_tx: &broadcast::Sender<()>,
) -> Result<String, String> {
    // Parse body to get source
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let new_source = parsed["source"].as_str().ok_or("missing source field")?;

    // Write to disk
    std::fs::write(entry_path, new_source).map_err(|e| e.to_string())?;

    // Recompile
    run_compiler(compiler_bin, entry_path, outdir, minify).map_err(|e| e.to_string())?;

    // Reload HTML
    let new_html = std::fs::read(format!("{}/index.html", outdir)).map_err(|e| e.to_string())?;
    *html.lock().unwrap() = new_html;

    // Reload server functions in V8
    let server_js = std::fs::read_to_string(format!("{}/server.js", outdir)).unwrap_or_default();
    if !server_js.is_empty() {
        runtime.execute_script("<reload>", server_js).map_err(|e| e.to_string())?;
        runtime.run_event_loop(Default::default()).await.map_err(|e| e.to_string())?;
    }

    // Read metadata
    let meta = std::fs::read_to_string(format!("{}/meta.json", outdir)).unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or(serde_json::json!({}));

    // Notify WebSocket clients to reload
    let _ = reload_tx.send(());

    eprintln!("[appbase] Recompiled + reloaded");
    Ok(serde_json::json!({
        "ok": true,
        "functions": meta["server_functions"],
    }).to_string())
}

fn run_compiler(compiler_bin: &str, entry: &str, outdir: &str, minify: bool) -> Result<(), String> {
    let mut cmd_args = vec![entry.to_string(), "--target=rust".to_string(), format!("--outdir={}", outdir)];
    if minify {
        cmd_args.push("--minify".to_string());
    }
    let output = std::process::Command::new(compiler_bin)
        .args(&cmd_args)
        .output()
        .map_err(|e| format!("Failed to run compiler '{}': {}", compiler_bin, e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Compiler failed: {}", stderr));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        eprintln!("{}", line);
    }

    Ok(())
}

fn inject_ws_reload(html: &[u8], port: u16) -> Vec<u8> {
    let html_str = String::from_utf8_lossy(html);
    let ws_script = format!(r#"
<script>
(function() {{
  const ws = new WebSocket('ws://' + location.host);
  ws.onmessage = (e) => {{
    if (JSON.parse(e.data).type === 'reload') location.reload();
  }};
  ws.onclose = () => setTimeout(() => location.reload(), 1000);
}})();
</script>"#);

    let injected = html_str.replace("</body>", &format!("{}\n</body>", ws_script));
    injected.into_bytes()
}

// --- Serve mode (production) ---

async fn serve(
    script_path: &str,
    db_path: &str,
    port: u16,
    static_file: Option<&str>,
) -> Result<(), deno_error::JsErrorBox> {
    fn err(e: impl std::fmt::Display) -> deno_error::JsErrorBox {
        deno_error::JsErrorBox::generic(e.to_string())
    }

    let static_html: Option<Vec<u8>> = static_file.map(|path| {
        std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("[appbase-rt] Warning: could not read {}: {}", path, e);
            b"<html><body>appbase</body></html>".to_vec()
        })
    });

    let (mut runtime, rpc_result) = create_v8_runtime(db_path).map_err(err)?;

    let user_code = std::fs::read_to_string(script_path)
        .map_err(|e| err(format!("Failed to read {}: {}", script_path, e)))?;
    runtime.execute_script("<user>", user_code).map_err(err)?;
    runtime.run_event_loop(Default::default()).await.map_err(err)?;

    eprintln!("[appbase-rt] Loaded: {}", script_path);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await.map_err(err)?;
    eprintln!("[appbase-rt] http://localhost:{}", port);

    loop {
        let (mut stream, _) = listener.accept().await.map_err(err)?;
        use tokio::io::AsyncWriteExt;

        let mut buf = vec![0u8; 65536];
        stream.readable().await.map_err(err)?;
        let n = stream.try_read(&mut buf).map_err(err)?;
        if n == 0 { continue; }
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

// --- Shared utilities ---

fn create_v8_runtime(db_path: &str) -> Result<(JsRuntime, Rc<RpcResult>), String> {
    let conn = Rc::new(Connection::open(db_path).map_err(|e| e.to_string())?);
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;").map_err(|e| e.to_string())?;

    let runtime_code: Arc<str> = Arc::from(include_str!("embed/runtime.js"));
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

    Ok((runtime, rpc_result))
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
