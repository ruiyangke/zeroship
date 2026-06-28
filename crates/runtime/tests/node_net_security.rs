#![allow(unsafe_code)]

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use compio_tls::TlsAcceptor;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use uuid::Uuid;
use zeroship_core::types::AppUsage;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EnvSnapshot, FetchOutcome, HostPort, ModuleEntry, NetPolicy, RequestCtx, Runtime, SettledFetch,
};

struct EnvGuard {
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set(vars: &[(&'static str, Option<String>)]) -> Self {
        let keys = [
            "ZEROSHIP_DEV",
            "ZEROSHIP_NET_GLOBAL_MAX_SOCKETS",
            "ZEROSHIP_NET_RESOLVE_TIMEOUT_MS",
            "ZEROSHIP_NET_TEST_DNS_HANG_HOST",
            "ZEROSHIP_NET_TEST_DNS_HANG_MS",
        ];
        let prev = keys
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect::<Vec<_>>();
        unsafe {
            for key in keys {
                std::env::remove_var(key);
            }
            for (key, val) in vars {
                match val {
                    Some(val) => std::env::set_var(key, val),
                    None => std::env::remove_var(key),
                }
            }
        }
        Self { prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, val) in &self.prev {
                match val {
                    Some(val) => std::env::set_var(key, val),
                    None => std::env::remove_var(key),
                }
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
                if stream.write_all(buf[..n].to_vec()).await.0.is_err() {
                    return;
                }
            }
        }
        ServerMode::Idle => {
            compio::time::sleep(Duration::from_secs(30)).await;
        }
    }
}

fn unused_loopback_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind unused port");
    listener.local_addr().expect("unused port addr").port()
}

#[derive(Debug)]
struct StaticResolver {
    key: Arc<CertifiedKey>,
}

impl ResolvesServerCert for StaticResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.key.clone())
    }
}

async fn spawn_self_signed_tls_server() -> SocketAddr {
    let certified =
        rcgen::generate_simple_self_signed(vec!["db.local.test".to_string()]).unwrap();
    let cert_der = certified.cert.der().clone();
    let key_der = certified.signing_key.serialize_der();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let key = CertifiedKey::from_der(
        vec![CertificateDer::from(cert_der.to_vec())],
        PrivateKeyDer::Pkcs8(key_der.into()),
        &provider,
    )
    .unwrap();
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(StaticResolver { key: Arc::new(key) }));
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tls test server");
    let addr = listener.local_addr().expect("tls test server local_addr");
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let acceptor = acceptor.clone();
            compio::runtime::spawn(async move {
                let _ = acceptor.accept(stream).await;
            })
            .detach();
        }
    })
    .detach();
    addr
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

async fn run_js_module(
    module_src: String,
    policy: NetPolicy,
    max_wait: Duration,
    meter: Option<(Uuid, Arc<zeroship_metering::Meter>)>,
) -> JsResult {
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module_src,
    }];
    let mut builder = Runtime::builder().modules(modules).net_policy(policy);
    if let Some((app_id, meter)) = meter {
        builder = builder.app_id(app_id).meter(meter);
    }
    let runtime = builder.build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    drive_fetch_outcome(outcome, max_wait).await
}

async fn run_net_js(body: &str, policy: NetPolicy, max_wait: Duration) -> JsResult {
    run_js_module(
        wrap_module(r#"import net from "node:net";"#, body),
        policy,
        max_wait,
        None,
    )
    .await
}

async fn dispatch_rpc_js(
    module_src: String,
    rpc_id: &str,
    policy: NetPolicy,
    max_wait: Duration,
) -> JsResult {
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: module_src,
    }];
    let runtime = Runtime::builder().modules(modules).net_policy(policy).build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let url = format!("http://localhost/__zeroship/v1/{rpc_id}");
    let outcome = runtime.call_fetch_handler(
        "POST",
        &url,
        &[("content-type".into(), "application/json".into())],
        r#"{"json":null}"#,
        &env,
        ctx,
    );
    drive_fetch_outcome(outcome, max_wait).await
}

async fn drive_fetch_outcome(outcome: FetchOutcome, max_wait: Duration) -> JsResult {
    match outcome {
        FetchOutcome::Response { status, body, .. } => JsResult { status, body },
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(max_wait, rx.recv()).await
        {
            Ok(Ok(SettledFetch::Response { status, body, .. })) => JsResult { status, body },
            Ok(Ok(SettledFetch::Stream {
                status,
                body_reader,
                ..
            })) => {
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

fn trusted(max_sockets: u32, egress_ceiling_bytes: u64) -> NetPolicy {
    NetPolicy::trusted(max_sockets, egress_ceiling_bytes)
}

fn allowlist(host: &str, port: u16, max_sockets: u32, egress_ceiling_bytes: u64) -> NetPolicy {
    NetPolicy::allowlist(
        vec![HostPort::new(host, port)],
        max_sockets,
        egress_ceiling_bytes,
    )
    .unwrap()
}

#[test]
fn ssrf_denies_metadata_cgnat_and_dns_to_private_even_trusted() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_net_js(
            r#"
const targets = [
  ["metadata", "169.254.169.254", 80],
  ["cgnat", "100.64.0.1", 80],
  ["localhost-name", "localhost", 80],
];
const out = [];
for (const [label, host, port] of targets) {
  out.push(await new Promise((resolve) => {
    const s = new net.Socket();
    s.on("error", (err) => resolve(`${label}:${err.code}:${err.message}`));
    s.on("close", () => resolve(`${label}:closed-without-error`));
    s.connect(port, host);
    setTimeout(() => resolve(`${label}:timeout`), 1000);
  }));
}
return out.join("|");
"#,
            trusted(8, 1024 * 1024),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    for label in ["metadata", "cgnat", "localhost-name"] {
        assert!(
            result.body.contains(&format!("{label}:ERR_NET_SSRF"))
                || result.body.to_ascii_lowercase().contains("ssrf"),
            "expected SSRF denial for {label}, got: {}",
            result.body
        );
    }
}

#[test]
fn non_affirmative_dev_env_does_not_relax_ssrf() {
    let _lock = lock_env();
    for dev_value in [Some("0".to_string()), Some(String::new())] {
        let _env = EnvGuard::set(&[("ZEROSHIP_DEV", dev_value)]);
        let result = compio::runtime::Runtime::new().unwrap().block_on(async {
            run_net_js(
                r#"
return await new Promise((resolve) => {
  const s = new net.Socket();
  s.on("error", (err) => resolve(`${err.code}:${err.message}`));
  s.on("close", () => resolve("closed-without-error"));
  s.connect(80, "169.254.169.254");
  setTimeout(() => resolve("timeout"), 1000);
});
"#,
                trusted(4, 1024 * 1024),
                Duration::from_secs(2),
            )
            .await
        });
        assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
        assert!(
            result.body.contains("ERR_NET_SSRF") || result.body.to_ascii_lowercase().contains("ssrf"),
            "ZEROSHIP_DEV must not relax SSRF unless it is exactly 1; got: {}",
            result.body
        );
    }
}

#[test]
fn dns_timeout_fails_closed() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[
        ("ZEROSHIP_NET_RESOLVE_TIMEOUT_MS", Some("20".to_string())),
        ("ZEROSHIP_NET_TEST_DNS_HANG_HOST", Some("hang.test".to_string())),
        ("ZEROSHIP_NET_TEST_DNS_HANG_MS", Some("250".to_string())),
    ]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_net_js(
            r#"
return await new Promise((resolve) => {
  const s = new net.Socket();
  s.on("error", (err) => resolve(`${err.code}:${err.message}`));
  s.on("close", () => resolve("closed-without-error"));
  s.connect(443, "hang.test");
  setTimeout(() => resolve("timeout"), 1000);
});
"#,
            trusted(4, 1024 * 1024),
            Duration::from_secs(2),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("ERR_NET_DNS_TIMEOUT"),
        "expected DNS timeout fail-closed, got: {}",
        result.body
    );
}

#[test]
fn denied_policy_cannot_resolve_node_net() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js_module(
            wrap_module(
                r#"import net from "node:net";"#,
                r#"return typeof net;"#,
            ),
            NetPolicy::Denied,
            Duration::from_secs(3),
            None,
        )
        .await
    });
    assert!(
        result.status >= 400 || result.body.contains("ERR:"),
        "denied import should fail closed, got status={} body={}",
        result.status,
        result.body
    );
    assert!(
        result.body.contains("node:net") || result.body.contains("Cannot resolve"),
        "denied import should mention node:net resolution, got: {}",
        result.body
    );
}

#[test]
fn unconnected_socket_wrappers_are_capped_and_reclaimed() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        run_net_js(
            &format!(
                r#"
const firstBatch = [];
let capFailure = "";
for (let i = 0; i < 64; i++) {{
  try {{
    firstBatch.push(new net.Socket());
  }} catch (err) {{
    capFailure = `${{i}}:${{err.code}}:${{err.message}}`;
    break;
  }}
}}
if (!capFailure) return "cap-not-hit";

await Promise.all(firstBatch.map((s) => new Promise((resolve) => {{
  s.on("close", resolve);
  s.destroy();
}})));

let reclaimed = "not-checked";
try {{
  const s = new net.Socket();
  s.destroy();
  reclaimed = "reclaimed";
}} catch (err) {{
  reclaimed = `reclaim-failed:${{err.code}}`;
}}

const cycle = await new Promise((resolve) => {{
  const s = new net.Socket();
  let data = "";
  s.on("connect", () => s.write("ok"));
  s.on("data", (chunk) => {{ data += chunk.toString(); s.end(); }});
  s.on("error", (err) => resolve(`cycle-error:${{err.code}}:${{err.message}}`));
  s.on("close", () => resolve(`cycle-data=${{data}}`));
  s.connect({}, "127.0.0.1");
  setTimeout(() => resolve(`cycle-timeout:data=${{data}}`), 3000);
}});

return `${{capFailure}}|${{reclaimed}}|${{cycle}}`;
"#,
                addr.port()
            ),
            allowlist("127.0.0.1", addr.port(), 1, 1024 * 1024),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("EMFILE")
            && result.body.contains("reclaimed")
            && result.body.contains("cycle-data=ok"),
        "expected wrapper cap, reclaim, and normal connect cycle, got: {}",
        result.body
    );
}

#[test]
fn allowlist_denies_miss_and_rejects_broad_entries_at_config_time() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    assert!(HostPort::try_new("*", 443).is_err());
    assert!(HostPort::try_new("*.workers.dev", 443).is_err());
    assert!(HostPort::try_new("*.neon.tech", 5432).is_err());

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let allowed = spawn_tcp_server(ServerMode::Idle).await;
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
            allowlist("127.0.0.1", allowed.port(), 4, 1024 * 1024),
            Duration::from_secs(3),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("capability_violation"),
        "expected allowlist miss denial, got: {}",
        result.body
    );
}

#[test]
fn kind_transition_globals_are_hidden_and_query_cannot_forge_action() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let (query, action) = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        let module = format!(
            r#"
import net from "node:net";

async function attemptQuery() {{
  const enterType = typeof globalThis.__zsEnterKind;
  const clearType = typeof globalThis.__zsClearKind;
  try {{
    if (typeof globalThis.__zsEnterKind === "function") {{
      globalThis.__zsEnterKind("action");
    }}
    const s = new net.Socket();
    return await new Promise((resolve) => {{
      s.on("connect", () => {{ s.destroy(); resolve(`query:types=${{enterType}}/${{clearType}};connected`); }});
      s.on("error", (err) => resolve(`query:types=${{enterType}}/${{clearType}};error:${{err.code}}:${{err.message}}`));
      s.connect({}, "127.0.0.1");
      setTimeout(() => resolve(`query:types=${{enterType}}/${{clearType}};timeout`), 3000);
    }});
  }} catch (err) {{
    return `query:types=${{enterType}}/${{clearType}};throw:${{err.code}}:${{err.message}}`;
  }}
}}
attemptQuery.config = {{ kind: "query" }};

async function attemptAction() {{
  const enterType = typeof globalThis.__zsEnterKind;
  const clearType = typeof globalThis.__zsClearKind;
  const s = new net.Socket();
  return await new Promise((resolve) => {{
    s.on("connect", () => {{ s.destroy(); resolve(`action:types=${{enterType}}/${{clearType}};connected`); }});
    s.on("error", (err) => resolve(`action:types=${{enterType}}/${{clearType}};error:${{err.code}}:${{err.message}}`));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve(`action:types=${{enterType}}/${{clearType}};timeout`), 3000);
  }});
}}
attemptAction.config = {{ kind: "action" }};

export default {{ rpc: {{ attemptQuery, attemptAction }} }};
"#,
            addr.port(),
            addr.port()
        );
        let policy = allowlist("127.0.0.1", addr.port(), 8, 1024 * 1024);
        let query = dispatch_rpc_js(
            module.clone(),
            "attemptQuery",
            policy.clone(),
            Duration::from_secs(5),
        )
        .await;
        let action = dispatch_rpc_js(module, "attemptAction", policy, Duration::from_secs(5)).await;
        (query, action)
    });
    assert_eq!(query.status, 200, "unexpected query status/body: {}", query.body);
    assert_eq!(
        action.status, 200,
        "unexpected action status/body: {}",
        action.body
    );
    assert!(
        query.body.contains("types=undefined/undefined")
            && query.body.contains("capability_violation"),
        "expected hidden globals and query denial, got: {}",
        query.body
    );
    assert!(
        action.body.contains("types=undefined/undefined")
            && action.body.contains("action:")
            && action.body.contains("connected"),
        "expected hidden globals and action success, got: {}",
        action.body
    );
}

#[test]
fn query_and_mutation_connect_hit_capability_violation_but_action_can_connect() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let (query, mutation, action) = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        let module = format!(
            r#"
import net from "node:net";

function denied(kind) {{
  const s = new net.Socket();
  try {{
    s.connect({}, "127.0.0.1");
    return `${{kind}}:allowed`;
  }} catch (err) {{
    return `${{kind}}:${{err.code}}:${{err.message}}`;
  }}
}}

function queryProc() {{ return denied("query"); }}
queryProc.config = {{ kind: "query" }};
function mutationProc() {{ return denied("mutation"); }}
mutationProc.config = {{ kind: "mutation" }};
function actionProc() {{
  const s = new net.Socket();
  return new Promise((resolve) => {{
    s.on("connect", () => {{ s.destroy(); resolve("action:connected"); }});
    s.on("error", (err) => resolve("action:error:" + err.code));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve("action:timeout"), 3000);
  }});
}}
actionProc.config = {{ kind: "action" }};

export default {{ rpc: {{ queryProc, mutationProc, actionProc }} }};
"#,
            addr.port(),
            addr.port()
        );
        let policy = allowlist("127.0.0.1", addr.port(), 8, 1024 * 1024);
        let query = dispatch_rpc_js(
            module.clone(),
            "queryProc",
            policy.clone(),
            Duration::from_secs(5),
        )
        .await;
        let mutation = dispatch_rpc_js(
            module.clone(),
            "mutationProc",
            policy.clone(),
            Duration::from_secs(5),
        )
        .await;
        let action = dispatch_rpc_js(module, "actionProc", policy, Duration::from_secs(5)).await;
        (query, mutation, action)
    });
    assert_eq!(query.status, 200, "unexpected query status/body: {}", query.body);
    assert_eq!(
        mutation.status, 200,
        "unexpected mutation status/body: {}",
        mutation.body
    );
    assert_eq!(
        action.status, 200,
        "unexpected action status/body: {}",
        action.body
    );
    assert!(
        query.body.contains("query:capability_violation")
            && mutation.body.contains("mutation:capability_violation")
            && action.body.contains("action:connected"),
        "expected query/mutation denial and action success, got query={} mutation={} action={}",
        query.body,
        mutation.body,
        action.body
    );
}

#[test]
fn outbound_hard_cap_destroys_socket() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        run_net_js(
            &format!(
                r#"
return await new Promise((resolve) => {{
  const s = new net.Socket();
  let error = "";
  let writes = 0;
  s.on("connect", () => {{
    const chunk = "x".repeat(64 * 1024);
    for (let i = 0; i < 40; i++) {{
      try {{
        s.write(chunk);
        writes++;
      }} catch (err) {{
        error = `throw:${{err.code}}`;
        break;
      }}
    }}
  }});
  s.on("error", (err) => {{ error = `${{err.code}}:${{err.message}}`; }});
  s.on("close", (hadError) => resolve(`error=${{error}};close=${{hadError}};writes=${{writes}}`));
  s.connect({}, "127.0.0.1");
  setTimeout(() => resolve(`timeout:error=${{error}};writes=${{writes}}`), 3000);
}});
"#,
                addr.port()
            ),
            allowlist("127.0.0.1", addr.port(), 4, 8 * 1024 * 1024),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("ERR_NET_WRITE_CAP") && result.body.contains("close=true"),
        "expected write hard-cap destroy, got: {}",
        result.body
    );
}

#[test]
fn pre_connect_write_queue_is_bounded_but_small_write_flushes() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let idle = spawn_tcp_server(ServerMode::Idle).await;
        let echo = spawn_tcp_server(ServerMode::Echo).await;
        run_net_js(
            &format!(
                r#"
const refused = (() => {{
  const s = new net.Socket();
  s.on("error", () => {{}});
  s.connect({}, "127.0.0.1");
  const chunk = "x".repeat(300 * 1024);
  const writes = [];
  for (let i = 0; i < 5; i++) {{
    const ok = s.write(chunk);
    writes.push(ok);
    if (!ok) break;
  }}
  s.destroy();
  return writes.join(",");
}})();

const flushed = await new Promise((resolve) => {{
  const s = new net.Socket();
  let data = "";
  s.on("data", (chunk) => {{ data += chunk.toString(); s.end(); }});
  s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
  s.on("close", () => resolve(data));
  s.connect({}, "127.0.0.1");
  const ok = s.write("small-before-connect");
  if (!ok) resolve("small-write-refused");
  setTimeout(() => resolve(`timeout:${{data}}`), 3000);
}});

return `writes=${{refused}}|flushed=${{flushed}}`;
"#,
                idle.port(),
                echo.port()
            ),
            NetPolicy::allowlist(
                vec![
                    HostPort::new("127.0.0.1", idle.port()),
                    HostPort::new("127.0.0.1", echo.port()),
                ],
                4,
                8 * 1024 * 1024,
            )
            .unwrap(),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("writes=true,true,true,false")
            && result.body.contains("flushed=small-before-connect"),
        "expected pending queue refusal and small write flush, got: {}",
        result.body
    );
}

#[test]
fn egress_ceiling_destroys_socket_and_feeds_spend_meter() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let app_id = Uuid::new_v4();
    let meter = Arc::new(zeroship_metering::Meter::new());
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        run_js_module(
            wrap_module(
                r#"import net from "node:net";"#,
                &format!(
                    r#"
return await new Promise((resolve) => {{
  const s = new net.Socket();
  let error = "";
  s.on("connect", () => {{
    s.write("a".repeat(40));
    s.write("b".repeat(40));
  }});
  s.on("error", (err) => {{ error = `${{err.code}}:${{err.message}}`; }});
  s.on("close", (hadError) => resolve(`error=${{error}};close=${{hadError}}`));
  s.connect({}, "127.0.0.1");
  setTimeout(() => resolve(`timeout:error=${{error}}`), 3000);
}});
"#,
                    addr.port()
                ),
            ),
            allowlist("127.0.0.1", addr.port(), 4, 64),
            Duration::from_secs(5),
            Some((app_id, Arc::clone(&meter))),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("ERR_NET_EGRESS_CAP") && result.body.contains("close=true"),
        "expected egress cap destroy, got: {}",
        result.body
    );
    let usage: AppUsage = meter
        .drain()
        .remove(&app_id)
        .expect("net egress should feed the spend meter");
    assert_eq!(usage.egress_bytes, 40, "accepted socket egress must feed fixed spend metric");
    assert_eq!(
        usage.custom.get("net_egress_bytes").copied(),
        Some(40),
        "net-specific attribution metric should also be stamped"
    );
}

#[test]
fn socket_reads_feed_net_ingress_meter() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let app_id = Uuid::new_v4();
    let meter = Arc::new(zeroship_metering::Meter::new());
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        run_js_module(
            wrap_module(
                r#"import net from "node:net";"#,
                &format!(
                    r#"
return await new Promise((resolve) => {{
  const s = new net.Socket();
  let received = "";
  s.on("connect", () => s.write("hello"));
  s.on("data", (chunk) => {{
    received += chunk.toString();
    s.destroy();
    resolve(received);
  }});
  s.on("error", (err) => resolve(`error:${{err.code}}`));
  s.connect({}, "127.0.0.1");
  setTimeout(() => resolve(`timeout:${{received}}`), 3000);
}});
"#,
                    addr.port()
                ),
            ),
            allowlist("127.0.0.1", addr.port(), 4, 1024 * 1024),
            Duration::from_secs(5),
            Some((app_id, Arc::clone(&meter))),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(result.body, "hello");
    let usage: AppUsage = meter
        .drain()
        .remove(&app_id)
        .expect("net ingress should feed the spend meter");
    assert_eq!(usage.ingress_bytes, 5, "accepted socket reads feed fixed ingress metric");
    assert_eq!(
        usage.custom.get("net_ingress_bytes").copied(),
        Some(5),
        "net-specific ingress attribution metric should also be stamped"
    );
}

#[test]
fn egress_ceiling_resets_between_dispatches() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
    let (first, second) = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        let modules = vec![ModuleEntry {
            specifier: "index.js".to_string(),
            source: format!(
                r#"
import net from "node:net";

function tripCap() {{
  return new Promise((resolve) => {{
    const s = new net.Socket();
    let error = "";
    s.on("connect", () => {{
      s.write("a".repeat(40));
      s.write("b".repeat(40));
    }});
    s.on("error", (err) => {{ error = `${{err.code}}:${{err.message}}`; }});
    s.on("close", (hadError) => resolve(`trip:error=${{error}};close=${{hadError}}`));
    s.connect({}, "127.0.0.1");
    setTimeout(() => resolve(`trip:timeout:error=${{error}}`), 3000);
  }});
}}

function smallWrite() {{
  return new Promise((resolve) => {{
    const s = new net.Socket();
    let data = "";
    s.on("connect", () => s.write("ok"));
    s.on("data", (chunk) => {{
      data += chunk.toString();
      s.end();
    }});
    s.on("error", (err) => resolve(`small:error:${{err.code}}:${{err.message}}`));
    s.on("close", () => resolve(`small:data=${{data}}`));
    try {{
      s.connect({}, "127.0.0.1");
    }} catch (err) {{
      resolve(`small:throw:${{err.code}}:${{err.message}}`);
    }}
    setTimeout(() => resolve(`small:timeout:data=${{data}}`), 3000);
  }});
}}

export default {{
  async fetch(req) {{
    const path = new URL(req.url).pathname;
    return new Response(path === "/trip" ? await tripCap() : await smallWrite());
  }},
}};
"#,
                addr.port(),
                addr.port()
            ),
        }];
        let runtime = Runtime::builder()
            .modules(modules)
            .net_policy(allowlist("127.0.0.1", addr.port(), 4, 64))
            .build();
        runtime.start_pump();

        let env = EnvSnapshot::empty();
        let first = runtime.call_fetch_handler(
            "GET",
            "http://localhost/trip",
            &[],
            "",
            &env,
            RequestCtx::new(CancelFlag::new()),
        );
        let first = drive_fetch_outcome(first, Duration::from_secs(5)).await;

        let second = runtime.call_fetch_handler(
            "GET",
            "http://localhost/small",
            &[],
            "",
            &env,
            RequestCtx::new(CancelFlag::new()),
        );
        let second = drive_fetch_outcome(second, Duration::from_secs(5)).await;
        (first, second)
    });

    assert_eq!(first.status, 200, "unexpected first status/body: {}", first.body);
    assert!(
        first.body.contains("ERR_NET_EGRESS_CAP") && first.body.contains("close=true"),
        "first dispatch should trip egress cap, got: {}",
        first.body
    );
    assert_eq!(
        second.body, "small:data=ok",
        "second dispatch should not inherit the prior egress-exhausted latch"
    );
}

#[test]
fn global_socket_ceiling_rejects_past_process_cap() {
    let _lock = lock_env();
    let _env = EnvGuard::set(&[
        ("ZEROSHIP_DEV", Some("1".to_string())),
        ("ZEROSHIP_NET_GLOBAL_MAX_SOCKETS", Some("1".to_string())),
    ]);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Idle).await;
        run_net_js(
            &format!(
                r#"
return await new Promise((resolve) => {{
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
            allowlist("127.0.0.1", addr.port(), 8, 1024 * 1024),
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

#[test]
fn reject_unauthorized_false_requires_affirmative_dev_env() {
    let _lock = lock_env();
    for dev_value in [Some("0".to_string()), Some(String::new())] {
        let _env = EnvGuard::set(&[("ZEROSHIP_DEV", dev_value)]);
        let denied = compio::runtime::Runtime::new().unwrap().block_on(async {
            run_js_module(
                wrap_module(
                    r#"import tls from "node:tls";"#,
                    r#"
try {
  tls.connect({
    host: "127.0.0.1",
    port: 443,
    servername: "db.local.test",
    rejectUnauthorized: false,
  });
  return "allowed";
} catch (err) {
  return `${err.code}:${err.message}`;
}
"#,
                ),
                trusted(4, 1024 * 1024),
                Duration::from_secs(3),
                None,
            )
            .await
        });
        assert_eq!(denied.status, 200, "unexpected status/body: {}", denied.body);
        assert!(
            denied
                .body
                .contains("ERR_TLS_REJECT_UNAUTHORIZED_DISABLED"),
            "rejectUnauthorized:false must fail closed for non-affirmative dev env, got: {}",
            denied.body
        );
    }

    let allowed = compio::runtime::Runtime::new().unwrap().block_on(async {
        let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
        let addr = spawn_self_signed_tls_server().await;
        run_js_module(
            wrap_module(
                r#"import tls from "node:tls";"#,
                &format!(
                    r#"
return await new Promise((resolve) => {{
  const s = tls.connect({{
    host: "127.0.0.1",
    port: {},
    servername: "db.local.test",
    rejectUnauthorized: false,
  }});
  s.on("secureConnect", () => {{ s.destroy(); resolve("secure"); }});
  s.on("error", (err) => resolve(`${{err.code}}:${{err.message}}`));
  setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                    addr.port()
                ),
            ),
            allowlist("127.0.0.1", addr.port(), 4, 1024 * 1024),
            Duration::from_secs(5),
            None,
        )
        .await
    });
    assert_eq!(allowed.status, 200, "unexpected status/body: {}", allowed.body);
    assert_eq!(allowed.body, "secure", "affirmative dev env should allow TLS verify disable");
}

#[test]
fn tls_verify_defaults_secure_and_verify_disable_is_not_trusted_escape() {
    let _lock = lock_env();
    let default_result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let _env = EnvGuard::set(&[("ZEROSHIP_DEV", Some("1".to_string()))]);
        let addr = spawn_self_signed_tls_server().await;
        run_js_module(
            wrap_module(
                r#"import tls from "node:tls";"#,
                &format!(
                    r#"
return await new Promise((resolve) => {{
  const s = tls.connect({{
    host: "127.0.0.1",
    port: {},
    servername: "db.local.test",
  }});
  s.on("secureConnect", () => resolve("unexpected-secure"));
  s.on("error", (err) => resolve(`${{err.code}}:${{err.message}}`));
  setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                    addr.port()
                ),
            ),
            allowlist("127.0.0.1", addr.port(), 4, 1024 * 1024),
            Duration::from_secs(5),
            None,
        )
        .await
    });
    assert_eq!(
        default_result.status, 200,
        "unexpected status/body: {}",
        default_result.body
    );
    assert!(
        default_result.body.contains("ERR_TLS_HANDSHAKE"),
        "default rejectUnauthorized=true should reject self-signed cert, got: {}",
        default_result.body
    );

    let denied = compio::runtime::Runtime::new().unwrap().block_on(async {
        let _env = EnvGuard::set(&[]);
        run_js_module(
            wrap_module(
                r#"import tls from "node:tls";"#,
                r#"
try {
  tls.connect({
    host: "127.0.0.1",
    port: 443,
    servername: "db.local.test",
    rejectUnauthorized: false,
  });
  return "allowed";
} catch (err) {
  return `${err.code}:${err.message}`;
}
"#,
            ),
            allowlist("127.0.0.1", 443, 4, 1024 * 1024),
            Duration::from_secs(3),
            None,
        )
        .await
    });
    assert_eq!(denied.status, 200, "unexpected status/body: {}", denied.body);
    assert!(
        denied
            .body
            .contains("ERR_TLS_REJECT_UNAUTHORIZED_DISABLED"),
        "rejectUnauthorized:false should be denied for Allowlist, got: {}",
        denied.body
    );

    let trusted_denied = compio::runtime::Runtime::new().unwrap().block_on(async {
        let _env = EnvGuard::set(&[]);
        run_js_module(
            wrap_module(
                r#"import tls from "node:tls";"#,
                r#"
try {
  tls.connect({
    host: "127.0.0.1",
    port: 443,
    servername: "db.local.test",
    rejectUnauthorized: false,
  });
  return "allowed";
} catch (err) {
  return `${err.code}:${err.message}`;
}
"#,
            ),
            trusted(4, 1024 * 1024),
            Duration::from_secs(3),
            None,
        )
        .await
    });
    assert_eq!(
        trusted_denied.status, 200,
        "unexpected status/body: {}",
        trusted_denied.body
    );
    assert!(
        trusted_denied
            .body
            .contains("ERR_TLS_REJECT_UNAUTHORIZED_DISABLED"),
        "rejectUnauthorized:false should be denied even for Trusted outside dev, got: {}",
        trusted_denied.body
    );
}
