#![allow(unsafe_code)]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use compio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};
use compio_tls::TlsAcceptor;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{
    EnvSnapshot, FetchOutcome, HostPort, ModuleEntry, NetPolicy, RequestCtx, Runtime, SettledFetch,
};

struct EnvGuard {
    prev: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set(dev: bool) -> Self {
        Self::set_with_native_roots(dev, None)
    }

    fn set_with_native_roots(dev: bool, cert_file: Option<&Path>) -> Self {
        let keys = ["ZEROSHIP_DEV", "SSL_CERT_FILE", "SSL_CERT_DIR"];
        let prev = keys
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect::<Vec<_>>();
        unsafe {
            if dev {
                std::env::set_var("ZEROSHIP_DEV", "1");
            } else {
                std::env::remove_var("ZEROSHIP_DEV");
            }
            match cert_file {
                Some(path) => std::env::set_var("SSL_CERT_FILE", path),
                None => std::env::remove_var("SSL_CERT_FILE"),
            }
            std::env::remove_var("SSL_CERT_DIR");
        }
        Self { prev }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (key, val) in &self.prev {
                match val {
                    Some(v) => std::env::set_var(key, v),
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

#[derive(Clone)]
struct TestCert {
    ca_pem: String,
    acceptor: TlsAcceptor,
    seen_sni: Arc<Mutex<Vec<Option<String>>>>,
}

#[derive(Debug)]
struct RecordingResolver {
    key: Arc<CertifiedKey>,
    seen_sni: Arc<Mutex<Vec<Option<String>>>>,
}

impl ResolvesServerCert for RecordingResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.seen_sni
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .push(client_hello.server_name().map(ToOwned::to_owned));
        Some(self.key.clone())
    }
}

fn test_cert() -> TestCert {
    let certified =
        rcgen::generate_simple_self_signed(vec!["db.local.test".to_string()]).unwrap();
    let ca_pem = certified.cert.pem();
    let cert_der = certified.cert.der().clone();
    let key_der = certified.signing_key.serialize_der();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let key = CertifiedKey::from_der(
        vec![CertificateDer::from(cert_der.to_vec())],
        PrivateKeyDer::Pkcs8(key_der.clone().into()),
        &provider,
    )
    .unwrap();
    let seen_sni = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver {
        key: Arc::new(key),
        seen_sni: seen_sni.clone(),
    };
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    TestCert {
        ca_pem,
        acceptor: TlsAcceptor::from(Arc::new(cfg)),
        seen_sni,
    }
}

fn ca_signed_test_cert() -> TestCert {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "zeroship test root");
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
    let ca_key = KeyPair::generate().unwrap();
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf_params = CertificateParams::new(vec!["db.local.test".to_string()]).unwrap();
    leaf_params
        .distinguished_name
        .push(DnType::CommonName, "db.local.test");
    leaf_params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    leaf_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    leaf_params.use_authority_key_identifier_extension = true;
    let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
    let provider = rustls::crypto::aws_lc_rs::default_provider();
    let key = CertifiedKey::from_der(
        vec![CertificateDer::from(leaf_cert.der().to_vec())],
        PrivateKeyDer::Pkcs8(leaf_key.serialize_der().into()),
        &provider,
    )
    .unwrap();
    let seen_sni = Arc::new(Mutex::new(Vec::new()));
    let resolver = RecordingResolver {
        key: Arc::new(key),
        seen_sni: seen_sni.clone(),
    };
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    TestCert {
        ca_pem: ca_cert.pem(),
        acceptor: TlsAcceptor::from(Arc::new(cfg)),
        seen_sni,
    }
}

fn ca_pem_only(common_name: &str) -> String {
    let mut ca_params = CertificateParams::new(Vec::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, common_name);
    ca_params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    ca_params.key_usages.push(KeyUsagePurpose::KeyCertSign);
    ca_params.key_usages.push(KeyUsagePurpose::CrlSign);
    let ca_key = KeyPair::generate().unwrap();
    ca_params.self_signed(&ca_key).unwrap().pem()
}

fn assert_seen_sni(cert: &TestCert, expected: &str) {
    let seen = cert
        .seen_sni
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    assert!(
        seen.iter().any(|name| name.as_deref() == Some(expected)),
        "expected SNI {expected:?}, got {seen:?}"
    );
}

#[derive(Clone, Copy)]
enum TlsServerMode {
    DirectEcho,
    StartTlsEcho,
    PlaintextBeforeTls,
}

async fn spawn_tls_server(cert: TestCert, mode: TlsServerMode) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind tls test server");
    let addr = listener.local_addr().expect("tls test server local_addr");
    compio::runtime::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            let acceptor = cert.acceptor.clone();
            compio::runtime::spawn(handle_tls_connection(stream, acceptor, mode)).detach();
        }
    })
    .detach();
    addr
}

async fn handle_tls_connection(mut stream: TcpStream, acceptor: TlsAcceptor, mode: TlsServerMode) {
    match mode {
        TlsServerMode::DirectEcho => {
            if let Ok(tls) = acceptor.accept(stream).await {
                echo_loop(tls).await;
            }
        }
        TlsServerMode::StartTlsEcho => {
            let mut buf = vec![0u8; 64];
            let compio::BufResult(read, next_buf) = stream.read(buf).await;
            buf = next_buf;
            let Ok(n) = read else {
                return;
            };
            if &buf[..n] != b"STARTTLS\n" {
                return;
            }
            if stream.write_all(b"READY\n".to_vec()).await.0.is_err() {
                return;
            }
            if stream.flush().await.is_err() {
                return;
            }
            if let Ok(tls) = acceptor.accept(stream).await {
                echo_loop(tls).await;
            }
        }
        TlsServerMode::PlaintextBeforeTls => {
            let _ = stream.write_all(b"PRETLS".to_vec()).await;
            let _ = stream.flush().await;
            let _ = acceptor.accept(stream).await;
        }
    }
}

async fn echo_loop<S>(mut stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin + 'static,
{
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
        if stream.flush().await.is_err() {
            return;
        }
    }
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

fn allowlist(addr: SocketAddr, max_sockets: u32) -> NetPolicy {
    NetPolicy::allowlist(
        vec![HostPort::new("127.0.0.1", addr.port())],
        max_sockets,
        1024 * 1024,
    )
    .unwrap()
}

fn trusted(max_sockets: u32) -> NetPolicy {
    NetPolicy::trusted(max_sockets, 1024 * 1024)
}

fn tls_module(body: &str) -> String {
    wrap_module(r#"import tls from "node:tls";"#, body)
}

#[test]
fn direct_tls_connect_ca_echo_and_sni() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let cert = test_cert();
        let ca_json = serde_json::to_string(&cert.ca_pem).unwrap();
        let addr = spawn_tls_server(cert.clone(), TlsServerMode::DirectEcho).await;
        let result = run_js_module(
            tls_module(&format!(
                r#"
return new Promise((resolve) => {{
    const s = tls.connect({{
        host: "127.0.0.1",
        port: {},
        servername: "db.local.test",
        ca: {},
    }});
    const events = [];
    let data = "";
    s.on("secureConnect", () => {{
        events.push(`secure:${{s.encrypted}}:${{s.authorized}}:${{s.authorizationError}}`);
        s.write("hello");
    }});
    s.on("data", (chunk) => {{
        data += chunk.toString();
        s.end();
    }});
    s.on("close", () => resolve(`${{events.join("|")}};data=${{data}}`));
    s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
    setTimeout(() => resolve(`timeout:${{events.join("|")}};data=${{data}}`), 3000);
}});
"#,
                addr.port(),
                ca_json
            )),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await;
        assert_seen_sni(&cert, "db.local.test");
        result
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(result.body, "secure:true:true:null;data=hello");
}

#[test]
fn custom_ca_replaces_native_roots_instead_of_augmenting_them() {
    let _lock = lock_env();
    let cert = ca_signed_test_cert();
    let native_roots_path = std::env::temp_dir().join(format!(
        "zeroship-native-roots-{}-{}.pem",
        std::process::id(),
        std::thread::current().name().unwrap_or("node_tls")
    ));
    std::fs::write(&native_roots_path, cert.ca_pem.as_bytes()).unwrap();

    let wrong_ca_json = serde_json::to_string(&ca_pem_only("zeroship wrong test root")).unwrap();
    let good_ca_json = serde_json::to_string(&cert.ca_pem).unwrap();

    let (wrong, good) = {
        let _env = EnvGuard::set_with_native_roots(true, Some(&native_roots_path));
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let addr = spawn_tls_server(cert.clone(), TlsServerMode::DirectEcho).await;
            let wrong = run_js_module(
                tls_module(&format!(
                    r#"
return new Promise((resolve) => {{
    const s = tls.connect({{
        host: "127.0.0.1",
        port: {},
        servername: "db.local.test",
        ca: {},
    }});
    s.on("secureConnect", () => resolve("unexpected-secure"));
    s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
    s.on("close", () => {{}});
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                    addr.port(),
                    wrong_ca_json
                )),
                allowlist(addr, 4),
                Duration::from_secs(5),
            )
            .await;
            let good = run_js_module(
                tls_module(&format!(
                    r#"
return new Promise((resolve) => {{
    const s = tls.connect({{
        host: "127.0.0.1",
        port: {},
        servername: "db.local.test",
        ca: {},
    }});
    let data = "";
    s.on("secureConnect", () => s.write("pin"));
    s.on("data", (chunk) => {{
        data += chunk.toString();
        s.end();
    }});
    s.on("close", () => resolve(`data=${{data}}`));
    s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
    setTimeout(() => resolve(`timeout:data=${{data}}`), 3000);
}});
"#,
                    addr.port(),
                    good_ca_json
                )),
                allowlist(addr, 4),
                Duration::from_secs(5),
            )
            .await;
            (wrong, good)
        })
    };
    let _ = std::fs::remove_file(&native_roots_path);

    assert_eq!(wrong.status, 200, "unexpected wrong status/body: {}", wrong.body);
    assert!(
        wrong.body.contains("error:ERR_TLS_HANDSHAKE"),
        "custom ca must reject a server chained only to native roots, got: {}",
        wrong.body
    );
    assert_eq!(good.status, 200, "unexpected good status/body: {}", good.body);
    assert_eq!(good.body, "data=pin");
}

#[test]
fn starttls_socket_upgrade_ca_echo() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let cert = test_cert();
        let ca_json = serde_json::to_string(&cert.ca_pem).unwrap();
        let addr = spawn_tls_server(cert.clone(), TlsServerMode::StartTlsEcho).await;
        let result = run_js_module(
            wrap_module(
                r#"import net from "node:net"; import tls from "node:tls";"#,
                &format!(
                    r#"
return new Promise((resolve) => {{
    const plain = net.createConnection({{ host: "127.0.0.1", port: {} }});
    plain.on("connect", () => plain.write("STARTTLS\n"));
    plain.once("data", (chunk) => {{
        if (chunk.toString() !== "READY\n") {{
            resolve("bad-ready:" + chunk.toString());
            return;
        }}
        const s = tls.connect({{
            socket: plain,
            servername: "db.local.test",
            ca: {},
        }});
        let data = "";
        s.on("secureConnect", () => s.write("upgraded"));
        s.on("data", (buf) => {{
            data += buf.toString();
            s.end();
        }});
        s.on("close", () => resolve(`secure=${{s.encrypted}};authorized=${{s.authorized}};data=${{data}}`));
        s.on("error", (err) => resolve(`tls-error:${{err.code}}:${{err.message}}`));
    }});
    plain.on("error", (err) => resolve(`plain-error:${{err.code}}:${{err.message}}`));
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                    addr.port(),
                    ca_json
                ),
            ),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await;
        assert_seen_sni(&cert, "db.local.test");
        result
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(result.body, "secure=true;authorized=true;data=upgraded");
}

#[test]
fn starttls_socket_upgrade_buffers_immediate_write_until_secure() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let cert = test_cert();
        let ca_json = serde_json::to_string(&cert.ca_pem).unwrap();
        let addr = spawn_tls_server(cert.clone(), TlsServerMode::StartTlsEcho).await;
        let result = run_js_module(
            wrap_module(
                r#"import net from "node:net"; import tls from "node:tls";"#,
                &format!(
                    r#"
return new Promise((resolve) => {{
    const plain = net.createConnection({{ host: "127.0.0.1", port: {} }});
    plain.on("connect", () => plain.write("STARTTLS\n"));
    plain.once("data", (chunk) => {{
        if (chunk.toString() !== "READY\n") {{
            resolve("bad-ready:" + chunk.toString());
            return;
        }}
        const s = tls.connect({{
            socket: plain,
            servername: "db.local.test",
            ca: {},
        }});
        let data = "";
        s.write("upgraded");
        s.on("data", (buf) => {{
            data += buf.toString();
            s.end();
        }});
        s.on("close", () => resolve(`secure=${{s.encrypted}};authorized=${{s.authorized}};data=${{data}}`));
        s.on("error", (err) => resolve(`tls-error:${{err.code}}:${{err.message}}`));
    }});
    plain.on("error", (err) => resolve(`plain-error:${{err.code}}:${{err.message}}`));
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                    addr.port(),
                    ca_json
                ),
            ),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await;
        assert_seen_sni(&cert, "db.local.test");
        result
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(result.body, "secure=true;authorized=true;data=upgraded");
}

#[test]
fn default_rejects_self_signed_without_ca() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let cert = test_cert();
        let addr = spawn_tls_server(cert, TlsServerMode::DirectEcho).await;
        run_js_module(
            tls_module(&format!(
                r#"
return new Promise((resolve) => {{
    const s = tls.connect({{
        host: "127.0.0.1",
        port: {},
        servername: "db.local.test",
    }});
    const events = [];
    s.on("secureConnect", () => resolve("unexpected-secure"));
    s.on("error", (err) => events.push(`error:${{err.code}}:${{err.message}}`));
    s.on("close", () => resolve(events.join("|") + "|close"));
    setTimeout(() => resolve("timeout:" + events.join("|")), 3000);
}});
"#,
                addr.port()
            )),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("error:ERR_TLS_HANDSHAKE") && result.body.contains("|close"),
        "expected TLS verification failure, got: {}",
        result.body
    );
}

#[test]
fn reject_unauthorized_false_denied_for_non_trusted() {
    let _lock = lock_env();
    let _env = EnvGuard::set(false);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        run_js_module(
            tls_module(
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
            NetPolicy::allowlist(vec![HostPort::new("127.0.0.1", 443)], 4, 1024 * 1024)
                .unwrap(),
            Duration::from_secs(3),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result
            .body
            .contains("ERR_TLS_REJECT_UNAUTHORIZED_DISABLED"),
        "expected rejectUnauthorized:false denial, got: {}",
        result.body
    );
}

#[test]
fn reject_unauthorized_false_dev_only_permitted() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let cert = test_cert();
        let addr = spawn_tls_server(cert, TlsServerMode::DirectEcho).await;
        run_js_module(
            tls_module(&format!(
                r#"
return new Promise((resolve) => {{
    const s = tls.connect({{
        host: "127.0.0.1",
        port: {},
        servername: "db.local.test",
        rejectUnauthorized: false,
    }});
    let data = "";
    const events = [];
    s.on("secureConnect", () => {{
        events.push(`secure:${{s.encrypted}}:${{s.authorized}}:${{s.authorizationError}}`);
        s.write("trusted");
    }});
    s.on("data", (chunk) => {{
        data += chunk.toString();
        s.end();
    }});
    s.on("close", () => resolve(`authorized=${{s.authorized}};autherr=${{s.authorizationError}};data=${{data}}`));
    s.on("error", (err) => resolve(`error:${{err.code}}:${{err.message}}`));
    setTimeout(() => resolve(`timeout:${{events.join("|")}};data=${{data}}`), 3000);
}});
"#,
                addr.port()
            )),
            trusted(4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert_eq!(
        result.body,
        "authorized=false;autherr=TLS verification disabled;data=trusted"
    );
}

#[test]
fn starttls_pre_upgrade_plaintext_fails_closed() {
    let _lock = lock_env();
    let _env = EnvGuard::set(true);
    let result = compio::runtime::Runtime::new().unwrap().block_on(async {
        let cert = test_cert();
        let ca_json = serde_json::to_string(&cert.ca_pem).unwrap();
        let addr = spawn_tls_server(cert, TlsServerMode::PlaintextBeforeTls).await;
        run_js_module(
            wrap_module(
                r#"import net from "node:net"; import tls from "node:tls";"#,
                &format!(
                    r#"
return new Promise((resolve) => {{
    const plain = net.createConnection({{ host: "127.0.0.1", port: {} }});
    plain.pause();
    plain.on("connect", () => {{
        setTimeout(() => {{
            const s = tls.connect({{
                socket: plain,
                servername: "db.local.test",
                ca: {},
            }});
            const events = [];
            s.on("secureConnect", () => resolve("unexpected-secure"));
            s.on("error", (err) => events.push(`error:${{err.code}}:${{err.message}}`));
            s.on("close", () => resolve(events.join("|") + "|close"));
        }}, 100);
    }});
    plain.on("error", (err) => resolve(`plain-error:${{err.code}}:${{err.message}}`));
    setTimeout(() => resolve("timeout"), 3000);
}});
"#,
                    addr.port(),
                    ca_json
                ),
            ),
            allowlist(addr, 4),
            Duration::from_secs(5),
        )
        .await
    });
    assert_eq!(result.status, 200, "unexpected status/body: {}", result.body);
    assert!(
        result.body.contains("error:ERR_TLS_HANDSHAKE") && result.body.contains("|close"),
        "expected STARTTLS fail-closed, got: {}",
        result.body
    );
}
