#![allow(unsafe_code)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EnvSnapshot, FetchOutcome, HostPort, ModuleEntry, NetPolicy, RequestCtx, Runtime, SettledFetch,
};

struct EnvGuard {
    prev_dev: Option<std::ffi::OsString>,
    prev_global_cap: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn set(dev: bool, global_cap: Option<usize>) -> Self {
        let prev_dev = std::env::var_os("ZEROSHIP_DEV");
        let prev_global_cap = std::env::var_os("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS");
        unsafe {
            if dev {
                std::env::set_var("ZEROSHIP_DEV", "1");
            } else {
                std::env::remove_var("ZEROSHIP_DEV");
            }
            match global_cap {
                Some(cap) => std::env::set_var("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS", cap.to_string()),
                None => std::env::remove_var("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS"),
            }
        }
        Self {
            prev_dev,
            prev_global_cap,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.prev_dev {
                Some(v) => std::env::set_var("ZEROSHIP_DEV", v),
                None => std::env::remove_var("ZEROSHIP_DEV"),
            }
            match &self.prev_global_cap {
                Some(v) => std::env::set_var("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS", v),
                None => std::env::remove_var("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS"),
            }
        }
    }
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

#[derive(Clone, Copy)]
enum ServerMode {
    Echo,
    Idle,
    SendOnConnect(&'static [u8]),
}

async fn spawn_tcp_server(mode: ServerMode) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tcp test server");
    let addr = listener.local_addr().expect("tcp test server local_addr");
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            compio::runtime::spawn(handle_test_connection(stream, mode)).detach();
        }
    })
    .detach();
    addr
}

async fn handle_test_connection(mut stream: TcpStream, mode: ServerMode) {
    match mode {
        ServerMode::Echo => {
            let mut buf = vec![0u8; 4096];
            loop {
                let compio::BufResult(read, next_buf) = stream.read(buf).await;
                buf = next_buf;
                let n = match read {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                let out = buf[..n].to_vec();
                if stream.write_all(out).await.0.is_err() {
                    return;
                }
            }
        }
        ServerMode::Idle => {
            compio::time::sleep(Duration::from_secs(30)).await;
        }
        ServerMode::SendOnConnect(bytes) => {
            let _ = stream.write_all(bytes.to_vec()).await;
            compio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

fn unused_loopback_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind unused port");
    listener.local_addr().expect("unused port addr").port()
}

struct JsResult {
    status: u16,
    body: String,
}

fn wrap_module(preamble: &str, body: &str) -> String {
    format!(
        r#"
{preamble}
export default {{
    async fetch() {{
        const result = await (async () => {{
{body}
        }})();
        return new Response(String(result), {{
            headers: {{ "content-type": "text/plain" }},
        }});
    }},
}};
"#
    )
}

async fn run_js_module(module_src: String, policy: NetPolicy, max_wait: Duration) -> JsResult {
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module_src,
    }];
    let runtime = Runtime::builder()
        .modules(modules)
        .net_policy(policy)
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    drive_fetch_outcome(outcome, max_wait).await
}

async fn run_net_js(body: &str, policy: NetPolicy, max_wait: Duration) -> JsResult {
    run_js_module(wrap_module(r#"import net from "node:net";"#, body), policy, max_wait).await
}

async fn drive_fetch_outcome(outcome: FetchOutcome, max_wait: Duration) -> JsResult {
    match outcome {
        FetchOutcome::Response { status, body, .. } => JsResult { status, body },
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(max_wait, rx.recv()).await {
            Ok(Ok(SettledFetch::Response { status, body, .. })) => JsResult { status, body },
            Ok(Ok(SettledFetch::Stream { status, body_reader, .. })) => {
                let mut body = Vec::new();
                for chunk in body_reader.drain() {
                    body.extend_from_slice(&chunk);
                }
                JsResult {
                    status,
                    body: String::from_utf8_lossy(&body).into_owned(),
                }
            }
            Ok(Ok(SettledFetch::WebSocketUpgrade { .. })) => JsResult {
                status: 500,
                body: "unexpected websocket upgrade".to_string(),
            },
            Ok(Err(e)) => JsResult {
                status: 500,
                body: format!("ERR: {}", e.message),
            },
            Err(_) => JsResult {
                status: 599,
                body: "TIMEOUT".to_string(),
            },
        },
        FetchOutcome::Stream {
            status,
            body_reader,
            ..
        } => {
            let mut body = Vec::new();
            for chunk in body_reader.drain() {
                body.extend_from_slice(&chunk);
            }
            JsResult {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            }
        }
        FetchOutcome::WebSocketUpgrade { .. } => JsResult {
            status: 500,
            body: "unexpected websocket upgrade".to_string(),
        },
    }
}

fn trusted(max_sockets: u32) -> NetPolicy {
    NetPolicy::trusted(max_sockets, 1024 * 1024)
}

fn allowlist(addr: SocketAddr, max_sockets: u32) -> NetPolicy {
    NetPolicy::allowlist(
        vec![HostPort::new("127.0.0.1", addr.port())],
        max_sockets,
        1024 * 1024,
    )
    .unwrap()
}

#[test]
fn socket_echo_lifecycle_and_is_ip() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    if (net.isIP("127.0.0.1") !== 4 || !net.isIPv4("127.0.0.1") || !net.isIPv6("::1")) {{
        resolve("bad-is-ip");
        return;
    }}
    const s = net.createConnection({{ port: {}, host: "127.0.0.1" }});
    const events = [];
    let data = "";
    s.on("connect", () => events.push("connect"));
    s.on("ready", () => events.push("ready"));
    s.on("data", (chunk) => {{
        data += chunk.toString();
        s.end();
    }});
    s.on("end", () => events.push("end"));
    s.on("close", () => {{
        events.push("close");
        resolve(`${{events.join(",")}};data=${{data}};remote=${{s.remoteAddress}}:${{s.remotePort}};read=${{s.bytesRead}};written=${{s.bytesWritten}}`);
    }});
    s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
    s.write("hello");
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                addr.port()
            ),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("connect,ready,end,close")
            && result.body.contains("data=hello")
            && result.body.contains("remote=127.0.0.1:")
            && result.body.contains("read=5")
            && result.body.contains("written=5"),
        "unexpected lifecycle result: {}",
        result.body
    );
}

#[test]
fn socket_half_close_emits_end_then_close() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s = new net.Socket();
    const events = [];
    s.on("connect", () => {{
        events.push("connect");
        s.write("bye");
        s.end();
    }});
    s.on("data", (chunk) => events.push("data:" + chunk.toString()));
    s.on("end", () => events.push("end"));
    s.on("close", () => resolve(events.join("|")));
    s.on("error", (err) => resolve("error:" + err.code + ":" + err.message));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve("timeout:" + events.join("|")), 3000);
}});
"#,
                addr.port()
            ),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("connect|data:bye|end"),
        "expected half-close lifecycle, got: {}",
        result.body
    );
}

#[test]
fn socket_connect_refused_emits_error_and_close() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let port = unused_loopback_port();
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s = new net.Socket();
    const events = [];
    s.on("error", (err) => events.push("error:" + (err.code || "") + ":" + err.message));
    s.on("close", () => resolve(events.join("|") + "|close"));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve("timeout:" + events.join("|")), 3000);
}});
"#,
                port
            ),
            NetPolicy::allowlist(vec![HostPort::new("127.0.0.1", port)], 4, 1024 * 1024)
                .unwrap(),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("error:ECONNREFUSED") && result.body.contains("|close"),
        "expected ECONNREFUSED close, got: {}",
        result.body
    );
}

#[test]
fn socket_set_timeout_fires_timeout() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s = new net.Socket();
    let timedOut = false;
    s.setTimeout(50, () => {{
        timedOut = true;
        s.destroy();
    }});
    s.on("timeout", () => {{
        if (timedOut) resolve("timeout-event");
    }});
    s.on("close", () => {{
        if (timedOut) resolve("timeout-close");
    }});
    s.on("error", (err) => resolve("error:" + err.message));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve("miss"), 1000);
}});
"#,
                addr.port()
            ),
            allowlist(addr, 4),
            Duration::from_secs(3),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body == "timeout-event" || result.body == "timeout-close",
        "expected timeout, got: {}",
        result.body
    );
}

#[test]
fn socket_pause_resume_gates_data() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::SendOnConnect(b"paused")).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s = net.createConnection({}, "127.0.0.1");
    let got = "";
    let beforeResume = null;
    s.pause();
    s.on("data", (chunk) => {{
        got += chunk.toString();
        s.destroy();
        resolve(`before=${{beforeResume}};data=${{got}}`);
    }});
    setTimeout(() => {{
        beforeResume = got;
        s.resume();
    }}, 100);
    s.on("error", (err) => resolve("error:" + err.message));
    setTimeout(() => resolve(`timeout;before=${{beforeResume}};data=${{got}}`), 2000);
}});
"#,
                addr.port()
            ),
            allowlist(addr, 4),
            Duration::from_secs(4),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(
        result.body, "before=;data=paused",
        "pause/resume should hold data until resume"
    );
}

#[test]
fn socket_write_backpressure_false_then_drain() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s = new net.Socket();
    let writeOk = null;
    let sawDrain = false;
    s.on("connect", () => {{
        writeOk = s.write("x".repeat(32768));
    }});
    s.on("drain", () => {{
        sawDrain = true;
        s.end();
    }});
    s.on("data", () => {{}});
    s.on("close", () => resolve(`writeOk=${{writeOk}};drain=${{sawDrain}}`));
    s.on("error", (err) => resolve("error:" + err.message));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve(`timeout;writeOk=${{writeOk}};drain=${{sawDrain}}`), 3000);
}});
"#,
                addr.port()
            ),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(result.body, "writeOk=false;drain=true");
}

#[test]
fn denied_policy_does_not_resolve_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js_module(
            wrap_module(
                r#"import net from "node:net";"#,
                r#"
return typeof net;
"#,
            ),
            NetPolicy::Denied,
            Duration::from_secs(3),
        )
        .await
    });
    assert!(
        result.status >= 400 || result.body.contains("ERR:"),
        "denied policy should fail import, got status={} body={}",
        result.status,
        result.body
    );
    assert!(
        result.body.contains("node:net") || result.body.contains("Cannot resolve"),
        "denied failure should mention node:net resolution, got: {}",
        result.body
    );
}

#[test]
fn allowlist_policy_rejects_non_allowlisted_target_synchronously() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        let blocked_port = unused_loopback_port();
        run_net_js(
            &format!(
                r#"
try {{
    const s = new net.Socket();
    s.connect({}, "127.0.0.1");
    return "allowed";
}} catch (err) {{
    return `${{err.code}}:${{err.message}}`;
}}
"#,
                blocked_port
            ),
            allowlist(addr, 4),
            Duration::from_secs(3),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("capability_violation"),
        "expected capability violation, got: {}",
        result.body
    );
}

#[test]
fn ssrf_gate_rejects_link_local_even_for_trusted_policy() {
    let _lock = lock_env();
    let _env = EnvGuard::set(false, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_net_js(
            r#"
return new Promise((resolve) => {
    const s = new net.Socket();
    s.on("error", (err) => resolve(`${err.code}:${err.message}`));
    s.on("close", () => resolve("closed-without-error"));
    s.connect(80, "169.254.169.254");
    setTimeout(() => resolve("timeout"), 3000);
});
"#,
            trusted(4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("ERR_NET_SSRF") || result.body.to_ascii_lowercase().contains("ssrf"),
        "expected SSRF denial, got: {}",
        result.body
    );
}

#[test]
fn per_app_socket_cap_rejects_past_limit() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, None);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s1 = new net.Socket();
    s1.on("connect", () => {{
        try {{
            const s2 = new net.Socket();
            s2.connect({}, "127.0.0.1");
            resolve("allowed");
        }} catch (err) {{
            s1.destroy();
            resolve(`${{err.code}}:${{err.message}}`);
        }}
    }});
    s1.on("error", (err) => resolve("first-error:" + err.message));
    s1.connect({}, "127.0.0.1");
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                addr.port(),
                addr.port()
            ),
            allowlist(addr, 1),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("EMFILE") || result.body.contains("capability_violation"),
        "expected per-app socket cap rejection, got: {}",
        result.body
    );
}

#[test]
fn global_socket_cap_rejects_past_limit() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true, Some(1));
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
    const s1 = new net.Socket();
    s1.on("connect", () => {{
        try {{
            const s2 = new net.Socket();
            s2.connect({}, "127.0.0.1");
            resolve("allowed");
        }} catch (err) {{
            s1.destroy();
            resolve(`${{err.code}}:${{err.message}}`);
        }}
    }});
    s1.on("error", (err) => resolve("first-error:" + err.message));
    s1.connect({}, "127.0.0.1");
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                addr.port(),
                addr.port()
            ),
            allowlist(addr, 8),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("EMFILE") || result.body.contains("process-wide"),
        "expected global socket cap rejection, got: {}",
        result.body
    );
}
