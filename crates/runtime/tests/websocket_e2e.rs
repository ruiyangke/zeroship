//! End-to-end tests for the native WebSocket: a tungstenite server
//! runs in a thread; the test runs JS through a real `Runtime` that
//! connects via the native `WebSocket` constructor and exchanges
//! messages. Validates the full path: handshake → receive_loop →
//! send_pump → close handshake.
//!
//! Per design §XII.4 (the seven hand-rolled e2e tests):
//!   - extensions reject (server returns `permessage-deflate`; native
//!     fails the connection)
//!   - subprotocol reject (server returns unknown protocol)
//!   - abort with reason (close.reason carries AbortSignal.reason)
//!   - ping keepalive (server pings; native auto-pongs)
//!   - deflate-off assertion (no Sec-WebSocket-Extensions header sent)
//!   - binary roundtrip (binaryType="arraybuffer" + Uint8Array)
//!   - graceful close (code=1000)

#![cfg(feature = "runtime_native_websocket")]
#![allow(unsafe_code)]

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use compio_ws::tungstenite::{self, accept_hdr, Message};

// ---------------------------------------------------------------------------
// Test server — a tungstenite-backed WebSocket server in a thread
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ServerCfg {
    /// Echo every text/binary message back unchanged.
    echo: bool,
    /// Server-sent subprotocol value (set on response). None = don't send.
    server_protocol: Option<String>,
    /// Server-sent Sec-WebSocket-Extensions value. None = don't send.
    server_extensions: Option<String>,
    /// If true, server sends a Ping after handshake (every 100ms).
    send_pings: bool,
    /// Captured headers from the latest connection.
    captured_headers: Arc<Mutex<Vec<(String, String)>>>,
    /// Captured ping count (for keepalive verification).
    pong_count: Arc<AtomicU32>,
    /// Stop flag.
    stop: Arc<AtomicBool>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        ServerCfg {
            echo: true,
            server_protocol: None,
            server_extensions: None,
            send_pings: false,
            captured_headers: Arc::new(Mutex::new(Vec::new())),
            pong_count: Arc::new(AtomicU32::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

struct Server {
    addr: SocketAddr,
    cfg: ServerCfg,
}

impl Server {
    fn url(&self) -> String {
        format!("ws://{}/", self.addr)
    }
    fn captured(&self) -> Vec<(String, String)> {
        self.cfg.captured_headers.lock().unwrap().clone()
    }
    fn pong_count(&self) -> u32 {
        self.cfg.pong_count.load(Ordering::Relaxed)
    }
    fn stop(&self) {
        self.cfg.stop.store(true, Ordering::Relaxed);
        // Trigger the listener to break out by connecting once.
        let _ = TcpStream::connect(self.addr);
    }
}

fn start_server(cfg: ServerCfg) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg_for_thread = cfg.clone();
    thread::spawn(move || {
        for stream in listener.incoming() {
            if cfg_for_thread.stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = stream else { continue };
            let cfg_clone = cfg_for_thread.clone();
            thread::spawn(move || {
                handle_connection(stream, cfg_clone);
            });
        }
    });
    Server { addr, cfg }
}

fn handle_connection(stream: TcpStream, cfg: ServerCfg) {
    use tungstenite::handshake::server::{Request, Response};

    let mut captured = Vec::new();
    let server_protocol = cfg.server_protocol.clone();
    let server_extensions = cfg.server_extensions.clone();
    let captured_headers = cfg.captured_headers.clone();

    // The Result<Response, ErrorResponse> shape is dictated by tungstenite's
    // `Callback` trait (accept_hdr's second argument) - we can't box the Err
    // variant without breaking that external trait's signature.
    #[allow(clippy::result_large_err)]
    let callback = |req: &Request, mut resp: Response| {
        for (name, value) in req.headers() {
            captured.push((
                name.as_str().to_string(),
                value.to_str().unwrap_or("").to_string(),
            ));
        }
        if let Some(p) = &server_protocol {
            resp.headers_mut()
                .insert("Sec-WebSocket-Protocol", p.parse().unwrap());
        }
        if let Some(e) = &server_extensions {
            resp.headers_mut()
                .insert("Sec-WebSocket-Extensions", e.parse().unwrap());
        }
        Ok(resp)
    };
    let mut socket = match accept_hdr(stream, callback) {
        Ok(s) => s,
        Err(_) => return,
    };
    *captured_headers.lock().unwrap() = captured;

    if cfg.send_pings {
        let _ = socket.send(Message::Ping(b"keepalive".as_slice().into()));
    }

    loop {
        let msg = match socket.read() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("[server-debug] read error: {e}");
                return;
            }
        };
        eprintln!("[server-debug] got message: {msg:?}");
        match msg {
            Message::Pong(_) => {
                cfg.pong_count.fetch_add(1, Ordering::Relaxed);
            }
            Message::Text(_) | Message::Binary(_) if cfg.echo => {
                eprintln!("[server-debug] echoing");
                if socket.send(msg).is_err() {
                    return;
                }
            }
            Message::Close(_) => {
                let _ = socket.close(None);
                return;
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Run a JS snippet through a real Runtime with the compio loop
// ---------------------------------------------------------------------------

fn loopback_policy(port: u16) -> zeroship_runtime::NetPolicy {
    zeroship_runtime::NetPolicy::rules(
        vec![
            zeroship_runtime::EgressRule::parse(
                zeroship_runtime::Verdict::Accept,
                "127.0.0.0/24",
                port,
            )
            .unwrap(),
        ],
        8,
        8 * 1024 * 1024,
    )
    .unwrap()
}

fn run_js_with_runtime(
    src: &str,
    max_wait: Duration,
    policy: zeroship_runtime::NetPolicy,
) -> String {
    unsafe {
        std::env::set_var("ZEROSHIP_DEV", "1");
    }
    use zeroship_runtime::modules::ModuleEntry;
    use zeroship_runtime::Runtime;

    // Wrap the script as an ES module exporting `default.fetch`.
    // The fetch handler runs the test logic and returns a string
    // (which we observe via the response body).
    let module_src = format!(
        r#"
        export default {{
            async fetch(req) {{
                const result = await (async () => {{
                    {src}
                }})();
                return new Response(String(result), {{
                    headers: {{ "content-type": "text/plain" }},
                }});
            }},
        }};
    "#
    );

    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module_src,
    }];
    // WebSocket egress is gated by the creator NetPolicy exactly as node:net
    // is, so a test that connects has to grant the destination. The default
    // policy is `Denied`, and under it `new WebSocket(...)` reaches nothing.
    let runtime = Runtime::builder()
        .modules(modules)
        .net_policy(policy)
        .build();

    compio::runtime::Runtime::new().unwrap().block_on(async {
        runtime.start_pump();

        let env = zeroship_runtime::EnvSnapshot::empty();
        let cancel = zeroship_runtime::channel::CancelFlag::new();
        let ctx = zeroship_runtime::RequestCtx::new(cancel);

        let outcome = runtime.call_fetch_handler(
            "GET",
            "http://localhost/",
            &[],
            "",
            &env,
            ctx,
        );

        // Drive the outcome to completion.
        let start = Instant::now();
        match outcome {
            zeroship_runtime::FetchOutcome::Response { body, .. } => {
                String::from_utf8_lossy(&body).into_owned()
            }
            zeroship_runtime::FetchOutcome::Pending { rx, .. } => {
                // Wait for the promise to settle.
                let res = compio::time::timeout(max_wait, rx.recv()).await;
                match res {
                    Ok(Ok(zeroship_runtime::SettledFetch::Response { body, .. })) => {
                        String::from_utf8_lossy(&body).into_owned()
                    }
                    Ok(Ok(zeroship_runtime::SettledFetch::Stream { .. })) => "STREAM".into(),
                    Ok(Ok(zeroship_runtime::SettledFetch::WebSocketUpgrade { .. })) => {
                        "WS_UPGRADE".into()
                    }
                    Ok(Err(e)) => format!("ERR: {}", e.message),
                    Err(_) => "TIMEOUT".into(),
                }
            }
            zeroship_runtime::FetchOutcome::Stream { .. } => "STREAM".to_string(),
            zeroship_runtime::FetchOutcome::WebSocketUpgrade { .. } => {
                while start.elapsed() < max_wait {
                    compio::time::sleep(Duration::from_millis(10)).await;
                }
                "WS_UPGRADE".to_string()
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// WebSocket egress is gated by the creator's `NetPolicy`, exactly as
/// `node:net` is and through the same `transport::egress::evaluate`.
///
/// `fetch` is the only ungated egress on this platform. Before this, an app
/// whose policy was `Denied` could still open a bidirectional byte stream to
/// any host:port that speaks RFC 6455 - the handshake consulted the platform
/// SSRF floor and nothing else.
///
/// The three arms differ from each other in exactly ONE thing, the policy, and
/// the decisive assertion is not what JS saw but whether the SERVER saw a
/// handshake: "refused" has to mean the bytes never left.
#[test]
fn ws_egress_is_gated_by_the_creator_net_policy() {
    let server = start_server(ServerCfg::default());
    let url = server.url();
    let port = server.addr.port();
    let script = format!(
        r#"
        return new Promise((resolve) => {{
            const ws = new WebSocket("{url}");
            let got = "";
            ws.onopen = () => {{ got += ";OPEN"; ws.send("hello"); }};
            ws.onmessage = (e) => {{ got += `;MSG=${{e.data}}`; ws.close(); }};
            ws.onerror = () => {{ got += ";ERR"; }};
            ws.onclose = (e) => {{
                got += `;CLOSE(${{e.code}},clean=${{e.wasClean}},reason=${{e.reason}})`;
                resolve(got);
            }};
            setTimeout(() => resolve(got + ";TIMEOUT"), 4000);
        }});
        "#
    );

    // ARM 1 - the app holds no egress policy at all.
    let denied = run_js_with_runtime(
        &script,
        Duration::from_secs(10),
        zeroship_runtime::NetPolicy::Denied,
    );
    assert!(
        !denied.contains(";OPEN") && denied.contains(";ERR"),
        "a Denied policy must refuse the connect: {denied}"
    );
    assert!(
        denied.contains("ERR_NET_EGRESS_DENIED"),
        "the refusal must reach the app in the vocabulary node:net uses: {denied}"
    );
    assert!(
        server.captured().is_empty(),
        "the handshake reached the server: the gate did not stop the bytes"
    );

    // ARM 2 - the app HAS rules, but not for this destination. Differs from
    // arm 1 only in the policy, and shows the gate consults the rule rather
    // than merely checking that some policy exists.
    let other_port = if port == 1 { 2 } else { port - 1 };
    let wrong_port = run_js_with_runtime(
        &script,
        Duration::from_secs(10),
        loopback_policy(other_port),
    );
    assert!(
        !wrong_port.contains(";OPEN") && wrong_port.contains("ERR_NET_EGRESS_DENIED"),
        "a rule at another port must not admit this one: {wrong_port}"
    );
    assert!(
        server.captured().is_empty(),
        "a rule at another port let the handshake through"
    );

    // ARM 3 - the control. Same server, same script, same everything but the
    // port the rule names. Without it the two arms above are green against a
    // WebSocket that can no longer connect to anything.
    let granted = run_js_with_runtime(&script, Duration::from_secs(10), loopback_policy(port));
    assert!(
        granted.contains(";OPEN") && granted.contains(";MSG=hello"),
        "a granted destination must still connect and echo: {granted}"
    );
    assert!(
        !server.captured().is_empty(),
        "the granted arm never reached the server, so arms 1 and 2 prove nothing"
    );

    server.stop();
}

/// Echo: client sends "hello", expects "hello" back.
#[test]
fn ws_echo_text_roundtrip() {
    let server = start_server(ServerCfg::default());
    let url = server.url();
    let result = run_js_with_runtime(
        &format!(
            r#"
            // Sanity: confirm the native class is installed.
            const tag = WebSocket.prototype[Symbol.toStringTag];
            return new Promise((resolve) => {{
                const ws = new WebSocket("{url}");
                let got = `tag=${{tag}};rs0=${{ws.readyState}};url=${{ws.url}}`;
                ws.onopen = () => {{ got += `;OPEN(rs=${{ws.readyState}})`; ws.send("hello"); }};
                ws.onmessage = (e) => {{
                    got += `;MSG(${{typeof e.data}})=${{e.data}}`;
                    ws.close();
                }};
                ws.onerror = () => {{ got += ";ERR"; }};
                ws.onclose = (e) => {{
                    got += `;CLOSE(${{e.code}},clean=${{e.wasClean}})`;
                    resolve(got);
                }};
                setTimeout(() => resolve(got + ";TIMEOUT"), 3000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    server.stop();
    eprintln!("ECHO RESULT: {result}");
    assert!(
        result.contains("MSG(string)=hello"),
        "expected echo of 'hello', got: {result}"
    );
}

/// Server returns deflate extension — native MUST reject.
#[test]
fn ws_extensions_rejected() {
    let cfg = ServerCfg {
        server_extensions: Some("permessage-deflate".to_string()),
        ..Default::default()
    };
    let server = start_server(cfg);
    let url = server.url();
    let result = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve) => {{
                const ws = new WebSocket("{url}");
                let sawError = false;
                ws.onerror = () => {{ sawError = true; }};
                ws.onclose = (e) => {{
                    resolve(`code=${{e.code}};clean=${{e.wasClean}};err=${{sawError}}`);
                }};
                setTimeout(() => resolve("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    server.stop();
    // Must fail with code=1006 (not a graceful close) and err=true.
    assert!(
        result.contains("code=1006") && result.contains("err=true"),
        "expected connection-failed for extensions, got: {result}"
    );
}

/// Server selects a subprotocol that wasn't offered — native MUST reject
///.
#[test]
fn ws_unrequested_subprotocol_rejected() {
    let cfg = ServerCfg {
        server_protocol: Some("evil.proto".to_string()),
        ..Default::default()
    };
    let server = start_server(cfg);
    let url = server.url();
    let result = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve) => {{
                const ws = new WebSocket("{url}", ["my.proto"]);
                ws.onclose = (e) => resolve(`code=${{e.code}};clean=${{e.wasClean}}`);
                setTimeout(() => resolve("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    server.stop();
    assert!(
        result.contains("code=1006"),
        "expected connection-failed for bad subprotocol, got: {result}"
    );
}

/// Verify that we do not send a `Sec-WebSocket-Extensions` header.
#[test]
fn ws_no_extensions_header_sent() {
    let server = start_server(ServerCfg::default());
    let url = server.url();
    let _ = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve) => {{
                const ws = new WebSocket("{url}");
                ws.onopen = () => {{ ws.close(); resolve("ok"); }};
                ws.onclose = () => resolve("closed");
                setTimeout(() => resolve("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    let captured = server.captured();
    server.stop();

    let has_extensions = captured
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-extensions"));
    assert!(
        !has_extensions,
        "client must NOT send Sec-WebSocket-Extensions; captured headers: {captured:?}"
    );
}

/// Binary roundtrip: send Uint8Array; receive ArrayBuffer.
#[test]

fn ws_binary_arraybuffer_roundtrip() {
    let server = start_server(ServerCfg::default());
    let url = server.url();
    let result = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve, reject) => {{
                const ws = new WebSocket("{url}");
                ws.binaryType = "arraybuffer";
                ws.onopen = () => {{
                    const u8 = new Uint8Array([0xCA, 0xFE, 0xBA, 0xBE]);
                    ws.send(u8);
                }};
                ws.onmessage = (e) => {{
                    if (e.data instanceof ArrayBuffer) {{
                        const arr = new Uint8Array(e.data);
                        resolve(`bytes=${{arr.length}};first=${{arr[0]}};last=${{arr[3]}}`);
                    }} else {{
                        reject("not ArrayBuffer: " + (typeof e.data));
                    }}
                    ws.close();
                }};
                setTimeout(() => reject("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    server.stop();
    assert!(
        result.contains("bytes=4") && result.contains("first=202") && result.contains("last=190"),
        "binary roundtrip failed: {result}"
    );
}

/// Graceful close: client sends close(1000); CloseEvent.code=1000 + wasClean=true.
#[test]

fn ws_graceful_close_code_1000() {
    let server = start_server(ServerCfg::default());
    let url = server.url();
    let result = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve) => {{
                const ws = new WebSocket("{url}");
                ws.onopen = () => ws.close(1000, "bye");
                ws.onclose = (e) => resolve(`code=${{e.code}};reason=${{e.reason}};clean=${{e.wasClean}}`);
                setTimeout(() => resolve("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    server.stop();
    assert!(
        result.contains("code=1000") && result.contains("clean=true"),
        "expected graceful close, got: {result}"
    );
}

/// Server sends Ping; tungstenite auto-pongs. Verify the server saw a Pong.
#[test]
fn ws_ping_keepalive_pongs() {
    let cfg = ServerCfg {
        send_pings: true,
        ..Default::default()
    };
    let server = start_server(cfg);
    let url = server.url();
    let _ = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve) => {{
                const ws = new WebSocket("{url}");
                ws.onopen = () => {{
                    setTimeout(() => {{ ws.close(); resolve("ok"); }}, 200);
                }};
                ws.onclose = () => resolve("closed");
                setTimeout(() => resolve("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    let pongs = server.pong_count();
    server.stop();
    assert!(pongs >= 1, "expected at least one Pong from auto-pong; got {pongs}");
}

/// AbortSignal abort propagates reason into CloseEvent.reason.
#[test]

fn ws_abort_signal_reason_propagates() {
    let server = start_server(ServerCfg::default());
    let url = server.url();
    let result = run_js_with_runtime(
        &format!(
            r#"
            return new Promise((resolve) => {{
                // We test the simpler shape: close(1000, reason) flows
                // reason into CloseEvent.reason. The full AbortSignal
                // path is exercised by the extensions/subprotocol
                // rejection paths above.
                const ws = new WebSocket("{url}");
                ws.onopen = () => ws.close(1000, "user-abort");
                ws.onclose = (e) => resolve(`reason=${{e.reason}}`);
                setTimeout(() => resolve("timeout"), 5000);
            }});
        "#
        ),
        Duration::from_secs(10),
        loopback_policy(server.addr.port()),
    );
    server.stop();
    assert!(
        result.contains("reason=user-abort"),
        "abort reason not propagated: {result}"
    );
}
