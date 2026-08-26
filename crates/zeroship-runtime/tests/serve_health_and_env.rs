//! End-to-end tests for `zeroship serve` (single-tenant dev) gaps:
//!
//!   - ISS-58: `GET /health` must reach the user app's handler (not the
//!     kernel `{"status":"ok"}`); the platform health probe lives at the
//!     reserved `GET /__zeroship/health`.
//!   - ISS-56: app `env.*` vars must be injectable for local single-file
//!     testing via the `ZS_VAR_<NAME>` process-env prefix (`env.<NAME>`),
//!     and a non-prefixed host var must NOT leak into the app `env`.
//!
//! These drive the REAL serve path: `start_server` boots the actual compio
//! HTTP server on an ephemeral port in a background thread, and the test
//! hits it over raw TCP — the same `handle_connection` → `handle_request`
//! dispatch a `zeroship serve` invocation uses. No shim, no fake.

#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpStream, TcpListener};
use std::thread;
use std::time::{Duration, Instant};

use zeroship_runtime::serve::{start_server, ServerOptions};
use zeroship_runtime::ModuleEntry;

/// Grab an ephemeral port by binding :0 then dropping the listener. There's
/// an inherent TOCTOU race, but the window is tiny and the test server binds
/// immediately afterward; acceptable for a dev-tier integration test.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// A user app that:
///   - serves its OWN `GET /health` (proving the kernel no longer squats it),
///   - echoes `env.SECRET_TOKEN` / `env.HOME` at `GET /env-probe`,
///   - 404s everything else.
///
/// Written as a raw `export default { fetch }` so it flows through the same
/// bootstrap the production runtime wraps user code with.
const APP_SRC: &str = r#"
import { env } from "zeroship";
export default {
    fetch(request) {
        const url = new URL(request.url);
        if (url.pathname === "/health") {
            return new Response("app-health", {
                status: 200,
                headers: { "content-type": "text/plain", "x-served-by": "user-app" },
            });
        }
        if (url.pathname === "/env-probe") {
            return new Response(JSON.stringify({
                secret: env.SECRET_TOKEN ?? null,
                home: env.HOME ?? null,
            }), { status: 200, headers: { "content-type": "application/json" } });
        }
        return new Response("nope", { status: 404 });
    },
};
"#;

fn modules() -> Vec<ModuleEntry> {
    vec![ModuleEntry { specifier: "index.js".into(), source: APP_SRC.into() }]
}

/// Boot the real serve server on `port` (single worker) in a background
/// thread. `start_server` never returns, so the thread is intentionally
/// leaked for the lifetime of the test process.
fn boot_server(port: u16, env_vars: HashMap<String, String>) {
    thread::spawn(move || {
        start_server(
            modules(),
            ServerOptions { port, workers: 1, env_vars, ..Default::default() },
        );
    });
}

/// Poll the port until the server accepts a connection (or time out).
fn wait_until_ready(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if Instant::now() > deadline {
            panic!("serve server never became ready on port {port}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

/// Send one raw HTTP/1.1 GET and return the full response text.
fn http_get(port: u16, path: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Split an HTTP response text into (status_line, body).
fn split_response(resp: &str) -> (&str, &str) {
    let status_line = resp.lines().next().unwrap_or("");
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    (status_line, body)
}

#[test]
fn user_health_route_reaches_app_and_kernel_health_is_namespaced() {
    let port = free_port();
    boot_server(port, HashMap::new());
    wait_until_ready(port);

    // (a) GET /health must reach the USER app, not the kernel.
    let resp = http_get(port, "/health");
    let (status, body) = split_response(&resp);
    assert!(status.contains("200"), "user /health status: {status:?}");
    assert!(
        body.contains("app-health"),
        "GET /health must reach the user handler, got body: {body:?}\nfull: {resp}"
    );
    assert!(
        !body.contains(r#"{"status":"ok"}"#),
        "GET /health must NOT return the kernel health body; got: {resp}"
    );
    assert!(
        resp.to_ascii_lowercase().contains("x-served-by: user-app"),
        "user app response header must be present; got: {resp}"
    );

    // (b) The kernel/platform health probe lives at the reserved namespaced
    // path and still answers for tooling.
    let resp = http_get(port, "/__zeroship/health");
    let (status, body) = split_response(&resp);
    assert!(status.contains("200"), "kernel health status: {status:?}");
    assert!(
        body.contains(r#"{"status":"ok"}"#),
        "GET /__zeroship/health must return the kernel health body; got: {resp}"
    );
}

#[test]
fn app_env_populated_from_prefixed_process_vars_only() {
    let port = free_port();
    let mut env_vars = HashMap::new();
    // Prefixed → should surface as env.SECRET_TOKEN (prefix stripped).
    env_vars.insert("ZS_VAR_SECRET_TOKEN".to_string(), "s3cr3t".to_string());
    // Non-prefixed host var → must NOT leak into the app `env`.
    env_vars.insert("HOME".to_string(), "/home/should-not-leak".to_string());
    boot_server(port, env_vars);
    wait_until_ready(port);

    let resp = http_get(port, "/env-probe");
    let (status, body) = split_response(&resp);
    assert!(status.contains("200"), "env-probe status: {status:?}");

    let v: serde_json::Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("env-probe body not JSON ({e}): {body:?}\nfull: {resp}"));

    assert_eq!(
        v.get("secret").and_then(|x| x.as_str()),
        Some("s3cr3t"),
        "ZS_VAR_SECRET_TOKEN must surface as env.SECRET_TOKEN; got: {body}"
    );
    assert!(
        v.get("home").map(|x| x.is_null()).unwrap_or(false),
        "non-prefixed host var HOME must NOT leak into app env; got: {body}"
    );
}
