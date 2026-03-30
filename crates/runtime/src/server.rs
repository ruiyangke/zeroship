use crate::v8::{create_v8_runtime, handle_rpc};

pub fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
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
