use std::cell::RefCell;
use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use compio_tls::TlsAcceptor;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use zeroship_core::app_id::AppId;
use zeroship_core::usage_event::UsageEvent;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{EgressResolver, EgressRule, ResolveFuture, Verdict,

    EnvSnapshot, FetchOutcome, ModuleEntry, NetPolicy, RequestCtx, Runtime, SettledFetch,
};

/// Mirrors `DEFAULT_GLOBAL_MAX_SOCKETS` / `RESOLVE_TIMEOUT` in
/// `crates/runtime/src/transport/{net_policy,egress}.rs`. A test that names
/// neither wants them out of the way, not a specific number.
const DEFAULT_GLOBAL_MAX_SOCKETS: u32 = 4096;
const DEFAULT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// The four runtime settings the tests in this file vary. `Default` is the
/// production shape - dev mode OFF, so the SSRF floor is running, and no
/// bound tightened - which is what several of these tests need in order to
/// observe a refusal at all.
#[derive(Default)]
struct Settings {
    /// Dev mode. OFF by default, deliberately: with it on, `is_blocked_ip` is
    /// bypassed and the floor arm of a refusal split is never taken.
    dev_mode: bool,
    /// Process-wide socket ceiling. `None` leaves it out of the way.
    global_max_sockets: Option<u32>,
    /// PHASE 2 bound. `None` leaves it at the production default.
    resolve_timeout: Option<Duration>,
    /// Debug-only hold on the blocking DNS lookup, as `(host, delay)`.
    resolve_hang: Option<(String, Duration)>,
}

/// States [`Settings`] on the runtime's process-level cells and restores the
/// previous values on drop.
///
/// The cells are process-wide, so every user still takes [`lock_env`]: two
/// tests disagreeing about dev mode still disagree. What they are not is
/// process ENVIRONMENT - the previous shape set `ZEROSHIP_DEV` and friends
/// with `std::env::set_var`, which races concurrent libc `getenv` and is
/// undefined behaviour no mutex in this file could have covered.
struct SettingsGuard {
    prev_dev: bool,
    prev_global_max_sockets: u32,
    prev_resolve_timeout: Duration,
}

impl SettingsGuard {
    fn set(settings: Settings) -> Self {
        let guard = Self {
            prev_dev: zeroship_runtime::dev_mode_enabled(),
            prev_global_max_sockets: zeroship_runtime::global_max_sockets(),
            prev_resolve_timeout: zeroship_runtime::resolve_timeout(),
        };
        zeroship_runtime::set_dev_mode(settings.dev_mode);
        zeroship_runtime::set_global_max_sockets(
            settings.global_max_sockets.unwrap_or(DEFAULT_GLOBAL_MAX_SOCKETS),
        );
        zeroship_runtime::set_resolve_timeout(
            settings.resolve_timeout.unwrap_or(DEFAULT_RESOLVE_TIMEOUT),
        );
        zeroship_runtime::set_resolve_hang(settings.resolve_hang);
        guard
    }
}

impl Drop for SettingsGuard {
    fn drop(&mut self) {
        zeroship_runtime::set_dev_mode(self.prev_dev);
        zeroship_runtime::set_global_max_sockets(self.prev_global_max_sockets);
        zeroship_runtime::set_resolve_timeout(self.prev_resolve_timeout);
        zeroship_runtime::set_resolve_hang(None);
    }
}

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_env() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|err| err.into_inner())
}

fn usage_value(events: &[UsageEvent], app_id: &AppId, meter: &str) -> Option<u64> {
    events
        .iter()
        .find(|event| event.subject.app.as_ref() == Some(app_id) && event.meter == meter)
        .map(|event| event.value)
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
    meter: Option<(AppId, Arc<zeroship_metering::Meter>)>,
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

/// PHASE 2 of the SHIPPED connect path, recorded.
///
/// Everything below the seam - the off-thread lookup and its timeout - is
/// replaced, and every call is logged. Whether the production path resolved a
/// name is not otherwise observable: a refusal that arrives quickly cannot be
/// told from a lookup that was fast, so without this the DNS gate can only be
/// asserted about `evaluate` and never about what `connect()` does.
struct RecordingResolver {
    answers: Vec<SocketAddr>,
    calls: RefCell<Vec<(String, u16)>>,
}

impl RecordingResolver {
    fn new(answers: &[SocketAddr]) -> Self {
        Self {
            answers: answers.to_vec(),
            calls: RefCell::new(Vec::new()),
        }
    }
    fn lookups(&self) -> usize {
        self.calls.borrow().len()
    }
}

impl EgressResolver for RecordingResolver {
    fn resolve<'a>(&'a self, host: &'a str, port: u16) -> ResolveFuture<'a> {
        self.calls.borrow_mut().push((host.to_string(), port));
        let answers = self.answers.clone();
        Box::pin(async move { Ok(answers) })
    }
}

async fn run_net_js_with_resolver(
    body: &str,
    policy: NetPolicy,
    resolver: Rc<RecordingResolver>,
    max_wait: Duration,
) -> JsResult {
    let modules = vec![ModuleEntry {
        specifier: "index.js".to_string(),
        source: wrap_module(r#"import net from "node:net";"#, body),
    }];
    let runtime = Runtime::builder()
        .modules(modules)
        .net_policy(policy)
        .egress_resolver(resolver)
        .build();
    runtime.start_pump();

    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    let outcome = runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx);
    drive_fetch_outcome(outcome, max_wait).await
}

/// A connect to a DNS NAME, reporting either the socket error or the echoed
/// bytes. Every other rule-policy row in these suites targets an IP literal,
/// which skips the name phase entirely and so says nothing about the gate.
fn connect_to_name(host: &str, port: u16) -> String {
    format!(
        r#"
return await new Promise((resolve) => {{
  const s = new net.Socket();
  let data = "";
  s.on("connect", () => s.write("ok"));
  s.on("data", (chunk) => {{ data += chunk.toString(); s.end(); }});
  s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
  s.on("close", () => resolve(`closed:data=${{data}}`));
  s.connect({port}, "{host}");
  setTimeout(() => resolve("timeout"), 2000);
}});
"#
    )
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
        FetchOutcome::Response { status, body, .. } => JsResult {
            status,
            body: String::from_utf8_lossy(&body).into_owned(),
        },
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(max_wait, rx.recv()).await
        {
            Ok(Ok(SettledFetch::Response { status, body, .. })) => JsResult {
                status,
                body: String::from_utf8_lossy(&body).into_owned(),
            },
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
    NetPolicy::rules(
        vec![accept_target(host, port)],
        max_sockets,
        egress_ceiling_bytes,
    )
    .unwrap()
}

/// The DNS gate, asserted about the code that SHIPS rather than about the
/// evaluator in isolation.
///
/// Every other rule-policy row in these suites connects to `127.0.0.1` or
/// `169.254.169.254`, both IP literals, which skip the name phase, so without
/// this row and the connect path's resolver seam no test could see whether a
/// name was resolved at all - and deleting the gate from the shipped path would
/// leave every suite green.
///
/// The three arms differ from each other in ONE thing each, so a green result
/// says which rule decided it rather than only that something refused.
#[test]
fn the_shipped_connect_path_holds_the_dns_gate() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
    compio::runtime::Runtime::new().unwrap().block_on(async {
        let addr = spawn_tcp_server(ServerMode::Echo).await;
        let port = addr.port();
        let names_only = NetPolicy::rules(
            vec![EgressRule::parse(Verdict::Accept, "granted.example.test", port).unwrap()],
            4,
            1024 * 1024,
        )
        .unwrap();

        // ARM 1 - an ungranted name under a names-only policy. No lookup.
        let gated = Rc::new(RecordingResolver::new(&[addr]));
        let before = zeroship_runtime::gate_opened_resolutions();
        let refused = run_net_js_with_resolver(
            &connect_to_name("ungranted.example.test", port),
            names_only.clone(),
            gated.clone(),
            Duration::from_secs(3),
        )
        .await;
        // The lookup count is asserted FIRST because it is the property; the
        // refusal is the consequence. A path that resolved and then refused
        // would satisfy only the second.
        assert_eq!(
            gated.lookups(),
            0,
            "the shipped path resolved an ungranted name: the attacker-chosen \
             label reached a nameserver. body={}",
            refused.body
        );
        assert!(
            refused.body.contains("ERR_NET_EGRESS_DENIED"),
            "expected a creator-rule refusal, got: {}",
            refused.body
        );
        assert_eq!(
            zeroship_runtime::gate_opened_resolutions(),
            before,
            "the gate held, so no gate-opened resolution may be counted"
        );

        // ARM 2 - the control, differing in ONE thing: the name asked for. The
        // same policy resolves and connects for the name it grants, so arm 1 is
        // about the gate and not about a policy that refuses everything.
        let granted = Rc::new(RecordingResolver::new(&[addr]));
        let ok = run_net_js_with_resolver(
            &connect_to_name("granted.example.test", port),
            names_only,
            granted.clone(),
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(granted.lookups(), 1, "unexpected body: {}", ok.body);
        assert!(
            ok.body.contains("closed:data=ok"),
            "a granted name must connect, got: {}",
            ok.body
        );
        assert_eq!(
            zeroship_runtime::gate_opened_resolutions(),
            before,
            "a lookup for a name a Name ACCEPT admits is not a gate-opened one"
        );

        // ARM 3 - the residual, differing from arm 1 in ONE rule: a Range
        // ACCEPT at the same port. The gate opens, the ungranted name IS
        // resolved, and the platform counts it.
        let with_range = NetPolicy::rules(
            vec![
                EgressRule::parse(Verdict::Accept, "granted.example.test", port).unwrap(),
                EgressRule::parse(Verdict::Accept, "127.0.0.0/24", port).unwrap(),
            ],
            4,
            1024 * 1024,
        )
        .unwrap();
        let opened = Rc::new(RecordingResolver::new(&[addr]));
        let leaked = run_net_js_with_resolver(
            &connect_to_name("ungranted.example.test", port),
            with_range,
            opened.clone(),
            Duration::from_secs(3),
        )
        .await;
        assert_eq!(
            opened.lookups(),
            1,
            "holding a Range ACCEPT at this port opens the gate. body={}",
            leaked.body
        );
        assert!(
            leaked.body.contains("closed:data=ok"),
            "the range must admit the resolved address, got: {}",
            leaked.body
        );
        assert_eq!(
            zeroship_runtime::gate_opened_resolutions(),
            before + 1,
            "GATE_OPENED_RESOLUTIONS must count the shipped path's leak, not \
             only the evaluator's; it is the one piece of observability the \
             design leaves for this channel"
        );
    });
}

/// 5.7, with the platform floor LIVE.
///
/// Every other `node:net` row runs with dev mode ON, which bypasses
/// `is_blocked_ip` - so the floor arm of the refusal split is never taken and
/// collapsing the two codes into one leaves them all green. These two arms
/// differ in ONE thing, the address the name resolved to, and must report
/// DIFFERENT codes: an app cannot act on "your own rules refused this" if it is
/// spelled the same as "the platform refused this".
#[test]
fn the_floor_and_the_creators_rules_refuse_with_different_codes() {
    let _lock = lock_env();
    // Deliberately NOT dev mode: the floor has to be running.
    let _env = SettingsGuard::set(Settings::default());
    compio::runtime::Runtime::new().unwrap().block_on(async {
        // A public range the floor permits, so the creator's own rules are what
        // decide anything outside it.
        let policy = NetPolicy::rules(
            vec![EgressRule::parse(Verdict::Accept, "93.184.216.0/24", 443).unwrap()],
            4,
            1024 * 1024,
        )
        .unwrap();

        let private: SocketAddr = "10.0.0.5:443".parse().unwrap();
        let floored = Rc::new(RecordingResolver::new(&[private]));
        let by_floor = run_net_js_with_resolver(
            &connect_to_name("internal.example.test", 443),
            policy.clone(),
            floored,
            Duration::from_secs(3),
        )
        .await;
        assert!(
            by_floor.body.contains("ERR_NET_SSRF"),
            "an answer set the floor empties must report the floor, got: {}",
            by_floor.body
        );

        // The control: same policy, same port, same everything but the address
        // the name resolved to. Public, so it clears the floor and is refused
        // by the creator's own rule set instead.
        let public: SocketAddr = "203.0.114.9:443".parse().unwrap();
        let unmatched = Rc::new(RecordingResolver::new(&[public]));
        let by_rules = run_net_js_with_resolver(
            &connect_to_name("outside.example.test", 443),
            policy,
            unmatched,
            Duration::from_secs(3),
        )
        .await;
        assert!(
            by_rules.body.contains("ERR_NET_EGRESS_DENIED"),
            "an address that clears the floor and matches no ACCEPT is the \
             creator's refusal, got: {}",
            by_rules.body
        );
        assert!(
            !by_rules.body.contains("ERR_NET_SSRF"),
            "the floor did not refuse this one: {}",
            by_rules.body
        );
    });
}

#[test]
fn ssrf_denies_metadata_cgnat_and_dns_to_private_even_trusted() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings::default());
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

/// With dev mode OFF the SSRF floor is running and the metadata address is
/// refused, even on a `Trusted` policy that skips rule matching.
///
/// WHAT THIS DOES NOT CATCH. The `ZEROSHIP_DEV` spelling half - that neither
/// `"0"` nor `""` counts as dev - is not testable from this process: dev mode is
/// a cached process-level cell, so no test here can ask the environment twice.
/// That half is `only_exactly_one_is_dev_mode` in
/// `crates/zeroship-runtime/src/transport/ssrf.rs`,
/// which asks it directly and without an environment. This half asks the other
/// question - that an off mode really does leave the floor running - which the
/// spelling test cannot reach.
#[test]
fn dev_mode_off_does_not_relax_ssrf() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings::default());
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
        "dev mode off must not relax SSRF; got: {}",
        result.body
    );
}

#[test]
fn dns_timeout_fails_closed() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings {
        resolve_timeout: Some(Duration::from_millis(20)),
        resolve_hang: Some(("hang.test".to_string(), Duration::from_millis(250))),
        ..Settings::default()
    });
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
    // Wildcards are refused because they are not REPRESENTABLE, not by a curated
    // suffix list. The `*.` form is not a grammar the rule parser accepts, so
    // `*.workers.dev` cannot be written at all rather than being written and
    // caught.
    assert!(EgressRule::parse(Verdict::Accept, "*", 443).is_err());
    assert!(EgressRule::parse(Verdict::Accept, "*.workers.dev", 443).is_err());
    assert!(EgressRule::parse(Verdict::Accept, "*.neon.tech", 5432).is_err());
    // An exact host under the same suffix stays grantable: the deleted list was
    // enforcing taste, and reckless is the creator's problem now.
    assert!(EgressRule::parse(Verdict::Accept, "db.neon.tech", 5432).is_ok());
    // And one ACCEPT rule can no longer be the whole internet.
    assert!(EgressRule::parse(Verdict::Accept, "0.0.0.0/0", 443).is_err());

    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let allowed = spawn_tcp_server(ServerMode::Idle).await;
        let blocked_port = unused_loopback_port();
        run_net_js(
            &format!(
                r#"
return new Promise((resolve) => {{
  const s = new net.Socket();
  s.on("error", (err) => resolve(`${{err.code}}:${{err.message}}`));
  s.on("close", () => resolve("closed-without-error"));
  s.connect({}, "127.0.0.1");
  setTimeout(() => resolve("timeout"), 2000);
}});
"#,
                blocked_port
            ),
            allowlist("127.0.0.1", allowed.port(), 4, 1024 * 1024),
            Duration::from_secs(3),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    // An IP literal at a port no rule names is refused in the ADDRESS phase,
    // so the refusal arrives on the `error` event rather than throwing from
    // `connect()`: an address is decided by the same ordered phase that decides
    // a resolved one.
    assert!(
        result.body.contains("ERR_NET_EGRESS_DENIED"),
        "expected an egress-rule denial, got: {}",
        result.body
    );
}

#[test]
fn kind_transition_globals_are_hidden_and_query_cannot_forge_action() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
            NetPolicy::rules(
                vec![
                    accept_target("127.0.0.1", idle.port()),
                    accept_target("127.0.0.1", echo.port()),
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
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
    let app_id = AppId::mint();
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
            Some((app_id.clone(), Arc::clone(&meter))),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("ERR_NET_EGRESS_CAP") && result.body.contains("close=true"),
        "expected egress cap destroy, got: {}",
        result.body
    );
    let events = meter.drain();
    assert_eq!(
        usage_value(&events, &app_id, "egress_bytes"),
        Some(40),
        "accepted socket egress must feed fixed spend metric"
    );
    assert_eq!(
        usage_value(&events, &app_id, "net_egress_bytes"),
        Some(40),
        "net-specific attribution metric should also be stamped"
    );
}

#[test]
fn socket_reads_feed_net_ingress_meter() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
    let app_id = AppId::mint();
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
            Some((app_id.clone(), Arc::clone(&meter))),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(result.body, "hello");
    let events = meter.drain();
    assert_eq!(
        usage_value(&events, &app_id, "ingress_bytes"),
        Some(5),
        "accepted socket reads feed fixed ingress metric"
    );
    assert_eq!(
        usage_value(&events, &app_id, "net_ingress_bytes"),
        Some(5),
        "net-specific ingress attribution metric should also be stamped"
    );
}

#[test]
fn egress_ceiling_resets_between_dispatches() {
    let _lock = lock_env();
    let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
    let _env = SettingsGuard::set(Settings {
        dev_mode: true,
        global_max_sockets: Some(1),
        ..Settings::default()
    });
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

/// `rejectUnauthorized: false` is admitted ONLY in dev mode, and the control
/// below proves the same call succeeds once the mode is on - so the refusal is
/// attributable to the mode and not to the call being broken.
///
/// WHAT THIS DOES NOT CATCH. The `ZEROSHIP_DEV` spelling half - that neither
/// `"0"` nor `""` counts as dev - is `only_exactly_one_is_dev_mode` in
/// `crates/zeroship-runtime/src/transport/ssrf.rs`: dev mode is a cached
/// process-level cell, so no test in this process can ask the environment
/// twice.
#[test]
fn reject_unauthorized_false_requires_dev_mode() {
    let _lock = lock_env();
    {
        let _env = SettingsGuard::set(Settings::default());
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
            "rejectUnauthorized:false must fail closed outside dev mode, got: {}",
            denied.body
        );
    }

    let allowed = compio::runtime::Runtime::new().unwrap().block_on(async {
        let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
        let _env = SettingsGuard::set(Settings { dev_mode: true, ..Settings::default() });
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
        let _env = SettingsGuard::set(Settings::default());
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
        let _env = SettingsGuard::set(Settings::default());
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

/// Build an ACCEPT rule for a `node:net` test target.
///
/// These tests target literal addresses, and an IP literal is NOT a
/// representable `Name` - it must be written as a range, so a reader of a rule
/// always knows which check decides it. `is_blocked_ip` would refuse loopback
/// outright; these tests run with dev mode on, which bypasses the floor
/// and nothing else.
fn accept_target(host: &str, port: u16) -> EgressRule {
    let destination = match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => format!("{v4}/32"),
        Ok(std::net::IpAddr::V6(v6)) => format!("{v6}/128"),
        Err(_) => host.to_string(),
    };
    EgressRule::parse(Verdict::Accept, &destination, port).expect("valid test egress rule")
}
