use std::sync::atomic::{AtomicUsize, Ordering};

use compio::buf::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::TcpStream;
use ntex::web::HttpResponse;
use uuid::Uuid;

static NEXT_WORKER: AtomicUsize = AtomicUsize::new(0);

pub async fn forward(
    worker_urls: &[String],
    app_id: &Uuid,
    plan_id: &str,
    request_id: &Uuid,
    body: &[u8],
) -> Result<HttpResponse, String> {
    if worker_urls.is_empty() {
        return Err("no workers configured".into());
    }

    // Round-robin worker selection
    let idx = NEXT_WORKER.fetch_add(1, Ordering::Relaxed) % worker_urls.len();
    let worker_url = &worker_urls[idx];

    let parsed = url::Url::parse(&format!("{worker_url}/dispatch/{app_id}"))
        .map_err(|e| e.to_string())?;
    let host = parsed.host_str().ok_or("no host")?;
    let port = parsed.port().unwrap_or(80);
    let path = parsed.path();

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr).await.map_err(|e| e.to_string())?;

    // Build request with all required headers (spec: X-App-Id, X-Plan-Id, X-Request-Id)
    let header = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         X-App-Id: {app_id}\r\n\
         X-Plan-Id: {plan_id}\r\n\
         X-Request-Id: {request_id}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );

    let mut request_bytes = header.into_bytes();
    request_bytes.extend_from_slice(body);

    let BufResult(r, _) = stream.write_all(request_bytes).await;
    r.map_err(|e| e.to_string())?;

    // Read response
    let mut response = Vec::new();
    loop {
        let buf = vec![0u8; 8192];
        let BufResult(r, returned) = stream.read(buf).await;
        let n = r.map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        response.extend_from_slice(&returned[..n]);
    }

    // Parse response — find header/body boundary
    let header_end = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no header end")?;

    let header = std::str::from_utf8(&response[..header_end]).map_err(|e| e.to_string())?;
    let body_bytes = &response[header_end + 4..];

    // Extract status code
    let status = header
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(502);

    let mut builder = HttpResponse::build(
        ntex::http::StatusCode::from_u16(status)
            .unwrap_or(ntex::http::StatusCode::BAD_GATEWAY),
    );
    builder.content_type("application/json");

    // Forward headers from worker response
    for line in header.lines().skip(1) {
        if let Some((name, value)) = line.split_once(": ") {
            let lname = name.to_ascii_lowercase();
            if lname == "x-cpu-time-ms" || lname == "x-wall-time-ms" {
                builder.set_header(name, value.to_string());
            }
        }
    }

    Ok(builder.body(body_bytes.to_vec()))
}
