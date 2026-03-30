use deno_core::JsRuntime;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

use crate::server::{http_response, rpc_error_response};
use crate::v8::{RpcResult, create_v8_runtime, handle_rpc};

pub async fn dev(entry: &str, port: u16, compiler_bin: &str, minify: bool) -> Result<(), deno_error::JsErrorBox> {
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
    let (mut runtime, mut rpc_result) = create_v8_runtime("appbase.db").map_err(err)?;

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
                    Err(e) => rpc_error_response(500, &e.to_string(), None),
                }
            } else if headers.starts_with("POST /__dev/save") {
                // Save + recompile
                match handle_save(body, &entry_path, compiler_bin, outdir, minify, &html, &mut runtime, &mut rpc_result, &reload_tx2, &source).await {
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
    rpc_result: &mut Rc<RpcResult>,
    reload_tx: &broadcast::Sender<()>,
    source: &Arc<Mutex<String>>,
) -> Result<String, String> {
    let parsed: serde_json::Value = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let new_source = parsed["source"].as_str().ok_or("missing source field")?;

    // Write to disk + update source cache
    std::fs::write(entry_path, new_source).map_err(|e| e.to_string())?;
    *source.lock().unwrap() = new_source.to_string();

    // Recompile
    run_compiler(compiler_bin, entry_path, outdir, minify).map_err(|e| e.to_string())?;

    // Reload HTML
    let new_html = std::fs::read(format!("{}/index.html", outdir)).map_err(|e| e.to_string())?;
    *html.lock().unwrap() = new_html;

    // Create fresh V8 isolate (don't re-use old one with stale state)
    let (new_runtime, new_rpc_result) = create_v8_runtime("appbase.db").map_err(|e| e.to_string())?;
    *runtime = new_runtime;
    *rpc_result = new_rpc_result;

    let server_js = std::fs::read_to_string(format!("{}/server.js", outdir)).unwrap_or_default();
    if !server_js.is_empty() {
        runtime.execute_script("<reload>", server_js).map_err(|e| e.to_string())?;
        runtime.run_event_loop(Default::default()).await.map_err(|e| e.to_string())?;
    }

    let meta = std::fs::read_to_string(format!("{}/meta.json", outdir)).unwrap_or_default();
    let meta: serde_json::Value = serde_json::from_str(&meta).unwrap_or(serde_json::json!({}));

    let _ = reload_tx.send(());

    eprintln!("[appbase] Recompiled + reloaded (fresh isolate)");
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

fn inject_ws_reload(html: &[u8], _port: u16) -> Vec<u8> {
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
