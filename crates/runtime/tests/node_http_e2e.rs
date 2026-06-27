#![allow(unsafe_code)]

#[path = "support/node_realworld.rs"]
mod node_realworld;

use std::net::SocketAddr;
use std::time::Duration;

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use serde_json::json;

use node_realworld::{EnvGuard, allowlist, lock_env, run_js};

async fn spawn_split_http_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind raw HTTP test server");
    let addr = listener.local_addr().expect("raw HTTP server local_addr");
    compio::runtime::spawn(async move {
        let Ok((stream, _peer)) = listener.accept().await else {
            return;
        };
        handle_http_connection(stream).await;
    })
    .detach();
    addr
}

async fn handle_http_connection(mut stream: TcpStream) {
    let mut request = Vec::new();
    let mut buf = vec![0u8; 1024];
    loop {
        let compio::BufResult(read, next_buf) = stream.read(buf).await;
        buf = next_buf;
        let n = match read {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        request.extend_from_slice(&buf[..n]);
        if request.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }

    let head = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\nhello ";
    if stream.write_all(head.to_vec()).await.0.is_err() {
        return;
    }
    compio::time::sleep(Duration::from_millis(25)).await;
    let _ = stream.write_all(b"world!".to_vec()).await;
    let _ = stream.shutdown().await;
}

#[test]
fn raw_http_over_net_socket_parses_split_response() {
    let _lock = lock_env();
    let _env = EnvGuard::set_dev();

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_split_http_server().await;
        run_js(
            format!(
                r#"
import net from "node:net";

async function main() {{
  return await new Promise((resolve, reject) => {{
    const chunks = [];
    const socket = net.createConnection({{ host: "127.0.0.1", port: {port} }});
    socket.on("connect", () => {{
      socket.write("GET / HTTP/1.1\r\nHost: local.test\r\nConnection: close\r\n\r\n");
    }});
    socket.on("data", (chunk) => chunks.push(chunk.toString("utf8")));
    socket.on("end", () => {{
      const raw = chunks.join("");
      const split = raw.indexOf("\r\n\r\n");
      const headers = raw.slice(0, split);
      const body = raw.slice(split + 4);
      resolve({{
        statusLine: headers.split("\r\n")[0],
        hasLength: headers.includes("Content-Length: 12"),
        body,
        chunkCount: chunks.length,
      }});
    }});
    socket.on("error", reject);
    setTimeout(() => reject(new Error("raw HTTP socket timed out")), 5000);
  }});
}}

export default {{
  async fetch() {{
    try {{
      return Response.json(await main());
    }} catch (err) {{
      return new Response(JSON.stringify({{
        name: err?.name ?? "Error",
        code: err?.code ?? null,
        message: err?.message ?? String(err),
        stack: err?.stack ?? null,
      }}), {{ status: 500, headers: {{ "content-type": "application/json" }} }});
    }}
  }},
}};
"#,
                port = addr.port(),
            ),
            Vec::new(),
            allowlist("127.0.0.1", addr.port(), 4, 1024 * 1024),
            Duration::from_secs(10),
        )
        .await
    });

    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    let body: serde_json::Value =
        serde_json::from_str(&result.body).expect("raw HTTP e2e response must be JSON");
    assert_eq!(
        body.get("statusLine"),
        Some(&json!("HTTP/1.1 200 OK")),
        "status line mismatch: {body}"
    );
    assert_eq!(
        body.get("hasLength"),
        Some(&json!(true)),
        "headers mismatch: {body}"
    );
    assert_eq!(
        body.get("body"),
        Some(&json!("hello world!")),
        "body mismatch: {body}"
    );
    assert!(
        body.get("chunkCount").and_then(|v| v.as_u64()).unwrap_or(0) >= 2,
        "response was not delivered across multiple data events: {body}"
    );
}

