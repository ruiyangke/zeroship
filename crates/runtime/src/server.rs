use crate::cpu_timer::{CpuLimits, CpuUsage};
use crate::v8::{create_v8_runtime, handle_rpc};

pub fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        500 => "Internal Server Error",
        _ => "Unknown",
    };
    let header = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let mut resp = header.into_bytes();
    resp.extend_from_slice(body);
    resp
}

pub fn sanitize_error(msg: &str) -> String {
    // Strip file paths, stack traces, and internal details
    let sanitized = msg
        .lines()
        .next()
        .unwrap_or("Internal error");
    // Remove absolute paths
    let sanitized = if sanitized.contains('/') {
        sanitized
            .split('/')
            .last()
            .unwrap_or(sanitized)
    } else {
        sanitized
    };
    sanitized.to_string()
}

pub fn rpc_error_response(status: u16, message: &str, id: Option<&str>) -> Vec<u8> {
    let safe_msg = sanitize_error(message);
    let id_value = id.map_or("null".to_string(), |i| format!("\"{i}\""));
    let body = format!(
        r#"{{"jsonrpc":"2.0","error":{{"code":-32603,"message":"{safe_msg}"}},"id":{id_value}}}"#
    );
    http_response(status, "application/json", body.as_bytes())
}

pub async fn serve(
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
            eprintln!("[appbase-rt] Warning: could not read {path}: {e}");
            b"<html><body>appbase</body></html>".to_vec()
        })
    });

    let (mut runtime, rpc_result) = create_v8_runtime(db_path).map_err(err)?;
    let cpu_limits = CpuLimits::default(); // 50ms per request
    let mut cpu_usage = CpuUsage::default();

    let user_code = std::fs::read_to_string(script_path)
        .map_err(|e| err(format!("Failed to read {script_path}: {e}")))?;
    runtime.execute_script("<user>", user_code).map_err(err)?;
    runtime.run_event_loop(Default::default()).await.map_err(err)?;

    eprintln!("[appbase-rt] Loaded: {script_path}");

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.map_err(err)?;
    eprintln!("[appbase-rt] http://localhost:{port}");

    loop {
        let (mut stream, addr) = listener.accept().await.map_err(err)?;
        use tokio::io::AsyncWriteExt;

        let mut buf = vec![0u8; 65536];
        stream.readable().await.map_err(err)?;
        let n = stream.try_read(&mut buf).map_err(err)?;
        if n == 0 { continue; }
        let request_str = String::from_utf8_lossy(&buf[..n]);

        // Parse request line for logging
        let request_line = request_str.lines().next().unwrap_or("");

        let response = if let Some(body_start) = request_str.find("\r\n\r\n") {
            let headers = &request_str[..body_start];
            let body = &request_str[body_start + 4..];

            // Handle CORS preflight
            if headers.starts_with("OPTIONS ") {
                http_response(204, "text/plain", b"")
            } else if headers.starts_with("POST /rpc") && !body.is_empty() {
                match handle_rpc(&mut runtime, &rpc_result, body, &cpu_limits).await {
                    Ok(rpc_resp) => {
                        cpu_usage.record(rpc_resp.cpu_time);
                        eprintln!("[appbase-rt] {addr} {request_line} cpu={:.2}ms total={:.2}ms reqs={}",
                            rpc_resp.cpu_time.as_secs_f64() * 1000.0,
                            cpu_usage.total.as_secs_f64() * 1000.0,
                            cpu_usage.request_count);
                        http_response(200, "application/json", rpc_resp.json.as_bytes())
                    }
                    Err(e) => rpc_error_response(500, &e.to_string(), None),
                }
            } else if let Some(ref html) = static_html {
                http_response(200, "text/html", html)
            } else {
                http_response(200, "application/json", br#"{"status":"appbase-rt running"}"#)
            }
        } else {
            http_response(400, "text/plain", b"Bad Request")
        };

        eprintln!("[appbase-rt] {addr} {request_line}");
        stream.write_all(&response).await.map_err(err)?;
    }
}
